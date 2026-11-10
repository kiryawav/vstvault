use std::io;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};

use crate::layout::{
    Block, CONTEXT_BYTES, CacheLine, ChannelHeader, MAGIC, Meta, PROTOCOL_VERSION, ParamPoint, RawEvent, shared_size,
};

pub fn qpc_now() -> i64 {
    let mut v = 0;
    unsafe { QueryPerformanceCounter(&mut v) };
    v
}

pub fn qpc_freq() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut v = 0;
        unsafe { QueryPerformanceFrequency(&mut v) };
        v
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundtripStatus {
    Ok,
    Timeout,
    Busy,
}

pub struct Channel {
    mapping: HANDLE,
    view: *mut ChannelHeader,
    input: *mut f32,
    output: *mut f32,
    request_event: HANDLE,
    response_event: HANDLE,
    max_frames: u32,
    input_channels: u32,
    output_channels: u32,
    next_seq: u64,
    abandoned_seq: u64,
    last_served: u64,
}

unsafe impl Send for Channel {}

impl Channel {
    pub fn create(name: &str, max_frames: u32, input_channels: u32, output_channels: u32) -> io::Result<Self> {
        let size = shared_size(max_frames, input_channels, output_channels).ok_or_else(|| invalid("channel size overflows"))?;
        let mut c = Self::empty(max_frames, input_channels, output_channels);
        c.mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                ptr::null(),
                PAGE_READWRITE,
                ((size as u64) >> 32) as u32,
                size as u32,
                object_name(name, "shm").as_ptr(),
            )
        };
        if c.mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        c.map_view()?;
        unsafe {
            ptr::write(
                ptr::addr_of_mut!((*c.view).meta),
                CacheLine(Meta { magic: MAGIC, version: PROTOCOL_VERSION, max_frames, input_channels, output_channels }),
            );
            c.request_event = CreateEventW(ptr::null(), 0, 0, object_name(name, "req").as_ptr());
            c.response_event = CreateEventW(ptr::null(), 0, 0, object_name(name, "resp").as_ptr());
        }
        c.finish_setup()?;
        Ok(c)
    }

    pub fn open(name: &str) -> io::Result<Self> {
        let mut c = Self::empty(0, 0, 0);
        c.mapping = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, object_name(name, "shm").as_ptr()) };
        if c.mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        c.map_view()?;
        let mapped = c.mapped_size()?;
        if mapped < size_of::<ChannelHeader>() {
            return Err(invalid("channel mapping is too small"));
        }
        let meta = unsafe { ptr::read(ptr::addr_of!((*c.view).meta.0)) };
        if meta.magic != MAGIC || meta.version != PROTOCOL_VERSION {
            return Err(invalid("channel has a different protocol version"));
        }
        match shared_size(meta.max_frames, meta.input_channels, meta.output_channels) {
            Some(needed) if needed <= mapped => {}
            _ => return Err(invalid("channel header does not fit its mapping")),
        }
        c.max_frames = meta.max_frames;
        c.input_channels = meta.input_channels;
        c.output_channels = meta.output_channels;
        let access = SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE;
        unsafe {
            c.request_event = OpenEventW(access, 0, object_name(name, "req").as_ptr());
            c.response_event = OpenEventW(access, 0, object_name(name, "resp").as_ptr());
        }
        c.finish_setup()?;
        Ok(c)
    }

    pub fn max_frames(&self) -> u32 {
        self.max_frames
    }

    pub fn input_channels(&self) -> u32 {
        self.input_channels
    }

    pub fn output_channels(&self) -> u32 {
        self.output_channels
    }

    pub fn write_input(&mut self, channel: u32, samples: &[f32]) {
        assert!(channel < self.input_channels && samples.len() <= self.max_frames as usize);
        unsafe { ptr::copy_nonoverlapping(samples.as_ptr(), self.input_ptr(channel), samples.len()) };
    }

    pub fn clear_input(&mut self, channel: u32, frames: u32) {
        assert!(channel < self.input_channels && frames <= self.max_frames);
        unsafe { ptr::write_bytes(self.input_ptr(channel), 0, frames as usize) };
    }

    pub fn read_output(&self, channel: u32, samples: &mut [f32]) {
        assert!(channel < self.output_channels && samples.len() <= self.max_frames as usize);
        unsafe { ptr::copy_nonoverlapping(self.output_ptr(channel), samples.as_mut_ptr(), samples.len()) };
    }

    pub fn write_input_silence(&mut self, bus: usize, flags: u64) {
        if let Some(slot) = self.header().request.0.input_silence.get(bus) {
            slot.store(flags, Ordering::Relaxed);
        }
    }

    pub fn read_output_silence(&self, bus: usize) -> u64 {
        self.header().output_silence.0.get(bus).map_or(0, |slot| slot.load(Ordering::Relaxed))
    }

    pub fn write_context(&mut self, context: &[u8]) {
        let request = &self.header().request.0;
        let length = context.len().min(CONTEXT_BYTES);
        unsafe { ptr::copy_nonoverlapping(context.as_ptr(), request.context.get().cast::<u8>(), length) };
        request.has_context.store(1, Ordering::Relaxed);
    }

    pub fn write_input_params(&mut self, points: &[ParamPoint]) {
        write_block(&self.header().input_params.0, points);
    }

    pub fn read_output_params(&self, out: &mut Vec<ParamPoint>) {
        read_block(&self.header().output_params.0, out);
    }

    pub fn write_input_events(&mut self, events: &[RawEvent]) {
        write_block(&self.header().input_events.0, events);
    }

    pub fn read_output_events(&self, out: &mut Vec<RawEvent>) {
        read_block(&self.header().output_events.0, out);
    }

    pub fn roundtrip(&mut self, frames: u32, spin_us: u32, timeout_ms: u32) -> RoundtripStatus {
        if self.abandoned_seq != 0 {
            if self.header().response_seq.0.load(Ordering::Acquire) < self.abandoned_seq {
                return RoundtripStatus::Busy;
            }
            self.abandoned_seq = 0;
        }
        self.next_seq += 1;
        let seq = self.next_seq;
        let h = self.header();
        h.request.0.num_frames.store(frames, Ordering::Relaxed);
        signal(&h.request.0.seq, seq, &h.host_sleeping.0, self.request_event);
        if wait_for(&h.response_seq.0, seq, &h.client_sleeping.0, self.response_event, spin_us, timeout_ms) {
            RoundtripStatus::Ok
        } else {
            self.abandoned_seq = seq;
            RoundtripStatus::Timeout
        }
    }

    pub fn wait_host_ready(&self, timeout_ms: u32) -> bool {
        let deadline = qpc_now() + qpc_freq() * i64::from(timeout_ms) / 1000;
        while qpc_now() < deadline {
            if self.header().control.0.host_ready.load(Ordering::Acquire) != 0 {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    pub fn request_stop(&self) {
        self.header().control.0.stop.store(1, Ordering::Release);
        unsafe { SetEvent(self.request_event) };
    }

    pub fn mark_host_ready(&self) {
        self.header().control.0.host_ready.store(1, Ordering::Release);
    }

    pub fn stop_requested(&self) -> bool {
        self.header().control.0.stop.load(Ordering::Acquire) != 0
    }

    pub fn wait_request(&self, spin_us: u32, timeout_ms: u32) -> Option<u64> {
        let h = self.header();
        let got = wait_for(
            &h.request.0.seq,
            self.last_served + 1,
            &h.host_sleeping.0,
            self.request_event,
            spin_us,
            timeout_ms,
        );
        got.then(|| h.request.0.seq.load(Ordering::Acquire))
    }

    pub fn request_frames(&self) -> u32 {
        self.header().request.0.num_frames.load(Ordering::Relaxed).min(self.max_frames)
    }

    pub fn read_context(&self, out: &mut [u8]) -> bool {
        let request = &self.header().request.0;
        if request.has_context.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let length = out.len().min(CONTEXT_BYTES);
        unsafe { ptr::copy_nonoverlapping(request.context.get().cast::<u8>(), out.as_mut_ptr(), length) };
        true
    }

    pub fn read_input_params(&self, out: &mut Vec<ParamPoint>) {
        read_block(&self.header().input_params.0, out);
    }

    pub fn read_input_events(&self, out: &mut Vec<RawEvent>) {
        read_block(&self.header().input_events.0, out);
    }

    pub fn write_output_events(&mut self, events: &[RawEvent]) {
        write_block(&self.header().output_events.0, events);
    }

    pub fn write_output_params(&mut self, points: &[ParamPoint]) {
        write_block(&self.header().output_params.0, points);
    }

    pub fn read_input_silence(&self, bus: usize) -> u64 {
        self.header().request.0.input_silence.get(bus).map_or(0, |slot| slot.load(Ordering::Relaxed))
    }

    pub fn write_output_silence(&mut self, bus: usize, flags: u64) {
        if let Some(slot) = self.header().output_silence.0.get(bus) {
            slot.store(flags, Ordering::Relaxed);
        }
    }

    pub fn pass_through(&mut self) {
        let frames = self.request_frames();
        for channel in 0..self.input_channels.min(self.output_channels) {
            unsafe { ptr::copy_nonoverlapping(self.input_ptr(channel), self.output_ptr(channel), frames as usize) };
        }
    }

    pub fn input_ptr(&self, channel: u32) -> *mut f32 {
        assert!(channel < self.input_channels);
        unsafe { self.input.add(channel as usize * self.max_frames as usize) }
    }

    pub fn output_ptr(&self, channel: u32) -> *mut f32 {
        assert!(channel < self.output_channels);
        unsafe { self.output.add(channel as usize * self.max_frames as usize) }
    }

    pub fn complete_request(&mut self, seq: u64) {
        self.last_served = seq;
        let h = self.header();
        signal(&h.response_seq.0, seq, &h.client_sleeping.0, self.response_event);
    }

    fn empty(max_frames: u32, input_channels: u32, output_channels: u32) -> Self {
        Self {
            mapping: ptr::null_mut(),
            view: ptr::null_mut(),
            input: ptr::null_mut(),
            output: ptr::null_mut(),
            request_event: ptr::null_mut(),
            response_event: ptr::null_mut(),
            max_frames,
            input_channels,
            output_channels,
            next_seq: 0,
            abandoned_seq: 0,
            last_served: 0,
        }
    }

    fn map_view(&mut self) -> io::Result<()> {
        let view = unsafe { MapViewOfFile(self.mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        self.view = view.Value.cast();
        Ok(())
    }

    fn mapped_size(&self) -> io::Result<usize> {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let written =
            unsafe { VirtualQuery(self.view.cast(), &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.RegionSize)
    }

    fn finish_setup(&mut self) -> io::Result<()> {
        if self.request_event.is_null() || self.response_event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let per_buffer = self.max_frames as usize * self.input_channels as usize;
        unsafe {
            self.input = self.view.add(1).cast::<f32>();
            self.output = self.input.add(per_buffer);
        }
        Ok(())
    }

    fn header(&self) -> &ChannelHeader {
        unsafe { &*self.view }
    }

}

impl Drop for Channel {
    fn drop(&mut self) {
        unsafe {
            if !self.request_event.is_null() {
                CloseHandle(self.request_event);
            }
            if !self.response_event.is_null() {
                CloseHandle(self.response_event);
            }
            if !self.view.is_null() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            }
            if !self.mapping.is_null() {
                CloseHandle(self.mapping);
            }
        }
    }
}

fn write_block<T: Copy, const N: usize>(block: &Block<T, N>, items: &[T]) {
    let count = items.len().min(N);
    unsafe { ptr::copy_nonoverlapping(items.as_ptr(), block.items.get().cast::<T>(), count) };
    block.count.store(count as u32, Ordering::Relaxed);
}

fn read_block<T: Copy, const N: usize>(block: &Block<T, N>, out: &mut Vec<T>) {
    out.clear();
    let count = (block.count.load(Ordering::Relaxed) as usize).min(N);
    out.reserve(count);
    unsafe {
        ptr::copy_nonoverlapping(block.items.get().cast::<T>(), out.as_mut_ptr(), count);
        out.set_len(count);
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn object_name(name: &str, suffix: &str) -> Vec<u16> {
    format!("Local\\vault.{name}.{suffix}").encode_utf16().chain(Some(0)).collect()
}

fn signal(seq: &AtomicU64, value: u64, sleeping: &AtomicU32, event: HANDLE) {
    seq.store(value, Ordering::SeqCst);
    if sleeping.swap(0, Ordering::SeqCst) != 0 {
        unsafe { SetEvent(event) };
    }
}

fn wait_for(
    seq: &AtomicU64,
    target: u64,
    sleeping: &AtomicU32,
    event: HANDLE,
    spin_us: u32,
    timeout_ms: u32,
) -> bool {
    let freq = qpc_freq();
    let start = qpc_now();
    if spin_us > 0 {
        let spin_end = start + freq * i64::from(spin_us) / 1_000_000;
        loop {
            if seq.load(Ordering::Acquire) >= target {
                return true;
            }
            std::hint::spin_loop();
            if qpc_now() >= spin_end {
                break;
            }
        }
    }
    let deadline = start + freq * i64::from(timeout_ms) / 1000;
    sleeping.store(1, Ordering::SeqCst);
    loop {
        if seq.load(Ordering::SeqCst) >= target {
            sleeping.store(0, Ordering::Relaxed);
            return true;
        }
        let left = deadline - qpc_now();
        if left <= 0 {
            sleeping.store(0, Ordering::Relaxed);
            return seq.load(Ordering::SeqCst) >= target;
        }
        let ms = (left * 1000 / freq) as u32 + 1;
        unsafe { WaitForSingleObject(event, ms) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{EVENT_BYTES, MAX_BUSES, MAX_EVENTS, MAX_PARAM_POINTS};
    use std::sync::atomic::AtomicU32 as Counter;

    fn unique_name() -> String {
        static NEXT: Counter = Counter::new(0);
        format!("test.{}.{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn spawn_host(name: String, spin_us: u32) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut host = Channel::open(&name).expect("open");
            host.mark_host_ready();
            while !host.stop_requested() {
                if let Some(seq) = host.wait_request(spin_us, 50) {
                    host.pass_through();
                    host.complete_request(seq);
                }
            }
        })
    }

    #[test]
    fn audio_comes_back_unchanged_in_the_same_block() {
        let name = unique_name();
        let mut client = Channel::create(&name, 16, 2, 2).unwrap();
        let host = spawn_host(name, 0);
        assert!(client.wait_host_ready(5000));

        for block in 0..100u32 {
            let left: Vec<f32> = (0..16).map(|i| (block * 100 + i) as f32).collect();
            let right: Vec<f32> = (0..16).map(|i| -((block * 100 + i) as f32)).collect();
            client.write_input(0, &left);
            client.write_input(1, &right);
            assert_eq!(client.roundtrip(16, 0, 5000), RoundtripStatus::Ok);
            let mut out = [0.0f32; 16];
            client.read_output(0, &mut out);
            assert_eq!(out.as_slice(), left.as_slice());
            client.read_output(1, &mut out);
            assert_eq!(out.as_slice(), right.as_slice());
        }
        client.request_stop();
        host.join().unwrap();
    }

    #[test]
    fn no_wakeup_is_lost_at_any_spin_setting() {
        for (client_spin, host_spin) in [(0, 0), (1, 0), (0, 1), (1, 1), (50, 50)] {
            let name = unique_name();
            let mut client = Channel::create(&name, 8, 1, 1).unwrap();
            let host = spawn_host(name, host_spin);
            assert!(client.wait_host_ready(5000));
            for _ in 0..20_000 {
                assert_eq!(client.roundtrip(8, client_spin, 5000), RoundtripStatus::Ok);
            }
            client.request_stop();
            host.join().unwrap();
        }
    }

    #[test]
    fn timed_out_block_is_not_mistaken_for_the_next_one() {
        let name = unique_name();
        let mut client = Channel::create(&name, 4, 1, 1).unwrap();
        let mut host = Channel::open(&name).unwrap();

        client.write_input(0, &[1.0; 4]);
        assert_eq!(client.roundtrip(4, 0, 1), RoundtripStatus::Timeout);
        assert_eq!(client.roundtrip(4, 0, 1), RoundtripStatus::Busy);

        let seq = host.wait_request(0, 1000).expect("request");
        assert_eq!(seq, 1);
        host.pass_through();
        host.complete_request(seq);

        client.write_input(0, &[2.0; 4]);
        let answered = std::thread::spawn(move || {
            let seq = host.wait_request(0, 5000).expect("request");
            assert_eq!(seq, 2);
            host.pass_through();
            host.complete_request(seq);
        });
        assert_eq!(client.roundtrip(4, 0, 5000), RoundtripStatus::Ok);
        answered.join().unwrap();
        let mut out = [0.0f32; 4];
        client.read_output(0, &mut out);
        assert_eq!(out, [2.0; 4]);
    }

    #[test]
    fn context_reaches_the_host_with_its_block() {
        let name = unique_name();
        let mut client = Channel::create(&name, 4, 1, 1).unwrap();
        let host = Channel::open(&name).unwrap();
        let mut seen = [0u8; 8];
        assert!(!host.read_context(&mut seen), "nothing was sent yet");

        client.write_context(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(host.read_context(&mut seen));
        assert_eq!(seen, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn parameter_changes_travel_both_ways_and_are_clamped() {
        let name = unique_name();
        let mut client = Channel::create(&name, 4, 1, 1).unwrap();
        let mut host = Channel::open(&name).unwrap();
        let point = |id| ParamPoint { id, sample_offset: 3, value: 0.25 };

        client.write_input_params(&[point(1), point(2)]);
        let mut seen = Vec::new();
        host.read_input_params(&mut seen);
        assert_eq!(seen, [point(1), point(2)]);

        host.write_output_params(&[point(9)]);
        client.read_output_params(&mut seen);
        assert_eq!(seen, [point(9)]);

        let too_many = vec![point(5); MAX_PARAM_POINTS + 10];
        client.write_input_params(&too_many);
        host.read_input_params(&mut seen);
        assert_eq!(seen.len(), MAX_PARAM_POINTS);

        client.header().input_params.0.count.store(u32::MAX, Ordering::Relaxed);
        host.read_input_params(&mut seen);
        assert_eq!(seen.len(), MAX_PARAM_POINTS);
    }

    #[test]
    fn events_travel_both_ways_and_are_clamped() {
        let name = unique_name();
        let mut client = Channel::create(&name, 4, 0, 2).unwrap();
        let mut host = Channel::open(&name).unwrap();
        let event = |byte| RawEvent([byte; EVENT_BYTES]);

        client.write_input_events(&[event(1), event(2)]);
        let mut seen = Vec::new();
        host.read_input_events(&mut seen);
        assert_eq!(seen, [event(1), event(2)]);
        client.write_input_events(&[]);
        host.read_input_events(&mut seen);
        assert!(seen.is_empty(), "a block's events are not replayed in the next");

        host.write_output_events(&[event(9)]);
        client.read_output_events(&mut seen);
        assert_eq!(seen, [event(9)]);

        client.write_input_events(&vec![event(5); MAX_EVENTS + 10]);
        host.read_input_events(&mut seen);
        assert_eq!(seen.len(), MAX_EVENTS);
        client.header().input_events.0.count.store(u32::MAX, Ordering::Relaxed);
        host.read_input_events(&mut seen);
        assert_eq!(seen.len(), MAX_EVENTS);
    }

    #[test]
    fn inputs_and_outputs_can_differ_and_stay_apart() {
        let name = unique_name();
        let mut client = Channel::create(&name, 4, 4, 2).unwrap();
        let mut host = Channel::open(&name).unwrap();
        assert_eq!((host.input_channels(), host.output_channels()), (4, 2));

        for channel in 0..4 {
            client.write_input(channel, &[channel as f32 + 1.0; 4]);
        }
        unsafe {
            assert_eq!(*host.input_ptr(3), 4.0, "the sidechain arrives in its own channels");
            *host.output_ptr(1) = 9.0;
        }
        let mut out = [0.0f32; 4];
        client.read_output(1, &mut out);
        assert_eq!(out[0], 9.0);
        assert_eq!(unsafe { *host.input_ptr(3) }, 4.0);

        client.clear_input(3, 4);
        assert_eq!(unsafe { *host.input_ptr(3) }, 0.0);

        client.write_input_silence(1, 0b11);
        assert_eq!(host.read_input_silence(1), 0b11);
        host.write_output_silence(0, 0b01);
        assert_eq!(client.read_output_silence(0), 0b01);
        client.write_input_silence(MAX_BUSES, 1);
        assert_eq!(host.read_input_silence(MAX_BUSES), 0);
    }

    #[test]
    fn pass_through_copies_only_channels_both_sides_have() {
        let name = unique_name();
        let mut client = Channel::create(&name, 4, 4, 2).unwrap();
        let mut host = Channel::open(&name).unwrap();
        client.write_input(1, &[5.0; 4]);
        client.header().request.0.num_frames.store(4, Ordering::Relaxed);
        host.pass_through();
        let mut out = [0.0f32; 4];
        client.read_output(1, &mut out);
        assert_eq!(out, [5.0; 4]);
    }

    #[test]
    fn open_rejects_a_missing_channel() {
        assert!(Channel::open(&unique_name()).is_err());
    }

    #[test]
    fn open_rejects_a_header_larger_than_its_mapping() {
        let name = unique_name();
        let creator = Channel::create(&name, 4, 1, 1).unwrap();
        unsafe { (*creator.view).meta.0.max_frames = 1 << 30 };
        assert!(Channel::open(&name).is_err());
    }
}

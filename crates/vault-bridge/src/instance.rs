use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use vault_protocol::control::{BusInfo as VaultBusInfo, BusLayout, EditorInfo, Event, ParamInfo, PluginInfo, StateKind};
use vault_protocol::manifest::Manifest;
use vault_protocol::{CONTEXT_BYTES, Channel, HostSession, MAX_EVENTS, MAX_PARAM_POINTS, ParamPoint, RawEvent, RoundtripStatus};
use vst3::Steinberg::Vst::RestartFlags_;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

use crate::dispatch::{Dispatcher, EventQueue};
use crate::events::{from_raw, to_raw};
use crate::log_error;
use crate::text::copy_utf16;
use crate::view::BridgeView;

const _: () = assert!(size_of::<ProcessContext>() <= CONTEXT_BYTES);

const DEFAULT_RATE: f64 = 48000.0;
const DEFAULT_MAX_FRAMES: u32 = 4096;
const REALTIME_TIMEOUT_MS: u32 = 200;
const OFFLINE_TIMEOUT_MS: u32 = 60_000;
const STATE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct Setup {
    sample_rate: f64,
    max_frames: u32,
    process_mode: i32,
}

struct AudioPath {
    channel: Channel,
    max_frames: u32,
    timeout_ms: u32,
    input_params: Vec<ParamPoint>,
    output_params: Vec<ParamPoint>,
    input_events: Vec<RawEvent>,
    output_events: Vec<RawEvent>,
}

#[derive(Default)]
struct ViewLink {
    frame: Option<ComPtr<IPlugFrame>>,
    view: Option<ViewPointer>,
    size: (i32, i32),
}

#[derive(Clone, Copy, PartialEq)]
struct ViewPointer(*mut IPlugView);

pub struct Inner {
    manifest: &'static Manifest,
    session: Mutex<Option<HostSession>>,
    session_thread: AtomicU32,
    audio: Mutex<Option<AudioPath>>,
    setup: Mutex<Setup>,
    latency_samples: AtomicU32,
    tail_samples: AtomicU32,
    buses: Mutex<BusLayout>,
    midi_assignments: Mutex<HashMap<(i32, i16, i16), u32>>,
    parameters: Mutex<Vec<ParamInfo>>,
    values: Mutex<HashMap<u32, f64>>,
    handler: Mutex<Option<ComPtr<IComponentHandler>>>,
    view: Mutex<ViewLink>,
    events: Arc<EventQueue>,
}

unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Inner {
    fn remember(&self, info: PluginInfo) {
        self.latency_samples.store(info.latency_samples, Ordering::Relaxed);
        self.tail_samples.store(info.tail_samples, Ordering::Relaxed);
    }

    fn refresh_buses(&self) {
        match self.with_session(|session| session.buses()) {
            Ok(layout) => {
                if let Ok(mut buses) = self.buses.lock() {
                    *buses = layout;
                }
            }
            Err(message) => log_error(&format!("reading buses failed: {message}")),
        }
    }

    fn refresh_midi_assignments(&self) {
        let has_event_inputs = self.buses.lock().is_ok_and(|buses| !buses.event_inputs.is_empty());
        let assignments = if has_event_inputs { self.with_session(|session| session.midi_assignments()) } else { Ok(Vec::new()) };
        match assignments {
            Ok(assignments) => {
                if let Ok(mut map) = self.midi_assignments.lock() {
                    *map = assignments.into_iter().map(|a| ((a.bus, a.channel, a.controller), a.id)).collect();
                }
            }
            Err(message) => log_error(&format!("reading MIDI controller assignments failed: {message}")),
        }
    }

    fn bus_count(&self, media_type: MediaType, direction: BusDirection) -> usize {
        self.buses.lock().ok().and_then(|buses| bus_list(&buses, media_type, direction).map(Vec::len)).unwrap_or(0)
    }

    fn bus(&self, media_type: MediaType, direction: BusDirection, index: int32) -> Option<VaultBusInfo> {
        let buses = self.buses.lock().ok()?;
        bus_list(&buses, media_type, direction)?.get(usize::try_from(index).ok()?).cloned()
    }

    pub fn with_session<T>(&self, action: impl FnOnce(&mut HostSession) -> Result<T, String>) -> Result<T, String> {
        let me = unsafe { GetCurrentThreadId() };
        if self.session_thread.load(Ordering::Relaxed) == me {
            return Err("nested call during a command".into());
        }
        let mut session = self.session.lock().map_err(|_| "the session lock is poisoned")?;
        self.session_thread.store(me, Ordering::Relaxed);
        let result = match session.as_mut() {
            Some(session) => action(session),
            None => Err("the plugin is not initialized".into()),
        };
        self.session_thread.store(0, Ordering::Relaxed);
        result
    }

    fn start_audio(&self) -> Result<(), String> {
        let setup = *self.setup.lock().map_err(|_| "the setup lock is poisoned")?;
        let timeout_ms = if setup.process_mode == ProcessModes_::kOffline as i32 {
            OFFLINE_TIMEOUT_MS
        } else {
            REALTIME_TIMEOUT_MS
        };
        let (input_channels, output_channels) = {
            let buses = self.buses.lock().map_err(|_| "the bus lock is poisoned")?;
            (buses.input_channels(), buses.output_channels())
        };
        let (channel, info) = self.with_session(|session| {
            let name = session.next_channel_name();
            let channel = Channel::create(&name, setup.max_frames, input_channels, output_channels)
                .map_err(|e| format!("cannot create the audio channel: {e}"))?;
            let info = session.activate(&name, setup.sample_rate, setup.max_frames, setup.process_mode)?;
            Ok((channel, info))
        })?;
        self.remember(info);
        let mut audio = self.audio.lock().map_err(|_| "the audio lock is poisoned")?;
        *audio = Some(AudioPath {
            channel,
            max_frames: setup.max_frames,
            timeout_ms,
            input_params: Vec::with_capacity(MAX_PARAM_POINTS),
            output_params: Vec::with_capacity(MAX_PARAM_POINTS),
            input_events: Vec::with_capacity(MAX_EVENTS),
            output_events: Vec::with_capacity(MAX_EVENTS),
        });
        Ok(())
    }

    fn stop_audio(&self) -> Result<(), String> {
        let path = self.audio.lock().map_err(|_| "the audio lock is poisoned")?.take();
        if let Some(path) = path {
            path.channel.request_stop();
            self.with_session(|session| session.deactivate())?;
        }
        Ok(())
    }

    fn get_state(&self, kind: StateKind, stream: *mut IBStream) -> tresult {
        let Some(stream) = (unsafe { ComRef::from_raw(stream) }) else { return kInvalidArgument };
        let state = match self.with_session(|session| session.get_state(kind)) {
            Ok(state) => state,
            Err(message) => {
                log_error(&format!("getState failed: {message}"));
                return kResultFalse;
            }
        };
        for chunk in state.chunks(i32::MAX as usize) {
            let mut written = 0;
            let result = unsafe { stream.write(chunk.as_ptr().cast_mut().cast(), chunk.len() as i32, &mut written) };
            if result != kResultOk {
                return result;
            }
        }
        kResultOk
    }

    fn set_state(&self, kind: StateKind, stream: *mut IBStream) -> tresult {
        let Some(stream) = (unsafe { ComRef::from_raw(stream) }) else { return kInvalidArgument };
        let mut state = Vec::new();
        let mut chunk = vec![0u8; STATE_CHUNK_BYTES];
        loop {
            let mut read = 0;
            let result = unsafe { stream.read(chunk.as_mut_ptr().cast(), chunk.len() as i32, &mut read) };
            if result != kResultOk || read <= 0 {
                break;
            }
            state.extend_from_slice(&chunk[..read as usize]);
        }
        match self.with_session(|session| session.set_state(kind, state)) {
            Ok(()) => {
                self.forget_values();
                kResultOk
            }
            Err(message) => {
                log_error(&format!("setState failed: {message}"));
                kResultFalse
            }
        }
    }

    fn forget_values(&self) {
        if let Ok(mut values) = self.values.lock() {
            values.clear();
        }
    }

    fn refresh_parameters(&self) {
        match self.with_session(|session| session.parameters()) {
            Ok(parameters) => {
                if let Ok(mut list) = self.parameters.lock() {
                    *list = parameters;
                }
            }
            Err(message) => log_error(&format!("reading parameters failed: {message}")),
        }
    }

    pub fn remember_view(&self, view: *mut IPlugView) {
        if let Ok(mut link) = self.view.lock() {
            link.view = Some(ViewPointer(view));
        }
    }

    pub fn forget_view(&self, view: *mut IPlugView) {
        if let Ok(mut link) = self.view.lock() {
            if link.view == Some(ViewPointer(view)) {
                link.view = None;
            }
        }
    }

    pub fn view_size(&self) -> (i32, i32) {
        self.view.lock().map(|link| link.size).unwrap_or_default()
    }

    pub fn ask_for_view_size(&self, width: i32, height: i32) {
        self.events.push(Event::ResizeView { width, height });
    }

    pub fn set_view_size(&self, width: i32, height: i32) {
        if let Ok(mut link) = self.view.lock() {
            link.size = (width, height);
        }
    }

    pub fn set_frame(&self, frame: Option<ComPtr<IPlugFrame>>) {
        if let Ok(mut link) = self.view.lock() {
            link.frame = frame;
        }
    }

    pub fn drain_events(&self) {
        while let Some(event) = self.events.next() {
            self.handle_event(event);
        }
    }

    fn handle_event(&self, event: Event) {
        let handler = || self.handler.lock().ok().and_then(|handler| handler.clone());
        unsafe {
            match event {
                Event::BeginEdit { id } => {
                    if let Some(handler) = handler() {
                        handler.beginEdit(id);
                    }
                }
                Event::PerformEdit { id, value } => {
                    if let Ok(mut values) = self.values.lock() {
                        values.insert(id, value);
                    }
                    if let Some(handler) = handler() {
                        handler.performEdit(id, value);
                    }
                }
                Event::EndEdit { id } => {
                    if let Some(handler) = handler() {
                        handler.endEdit(id);
                    }
                }
                Event::Restart { flags } => {
                    if flags & RestartFlags_::kLatencyChanged as i32 != 0 {
                        if let Ok(info) = self.with_session(|session| session.info()) {
                            self.remember(info);
                        }
                    }
                    if flags & RestartFlags_::kIoChanged as i32 != 0 {
                        self.refresh_buses();
                    }
                    if flags & (RestartFlags_::kIoChanged | RestartFlags_::kMidiCCAssignmentChanged) as i32 != 0 {
                        self.refresh_midi_assignments();
                    }
                    if flags & RestartFlags_::kParamValuesChanged as i32 != 0 {
                        self.forget_values();
                    }
                    if flags & RestartFlags_::kParamTitlesChanged as i32 != 0 {
                        self.refresh_parameters();
                    }
                    if let Some(handler) = handler() {
                        handler.restartComponent(flags);
                    }
                }
                Event::ResizeView { width, height } => {
                    let (frame, view) = match self.view.lock() {
                        Ok(link) => (link.frame.clone(), link.view),
                        Err(_) => return,
                    };
                    if let (Some(frame), Some(view)) = (frame, view) {
                        self.set_view_size(width, height);
                        let mut rect = ViewRect { left: 0, top: 0, right: width, bottom: height };
                        frame.resizeView(view.0, &mut rect);
                    }
                }
            }
        }
    }
}

pub struct Instance {
    inner: Arc<Inner>,
    dispatcher: Mutex<Option<Dispatcher>>,
}

impl Class for Instance {
    type Interfaces = (IComponent, IAudioProcessor, IEditController, IMidiMapping);
}

fn bus_list(buses: &BusLayout, media_type: MediaType, direction: BusDirection) -> Option<&Vec<VaultBusInfo>> {
    let output = direction == BusDirections_::kOutput as BusDirection;
    match media_type {
        t if t == MediaTypes_::kAudio as MediaType => Some(if output { &buses.outputs } else { &buses.inputs }),
        t if t == MediaTypes_::kEvent as MediaType => Some(if output { &buses.event_outputs } else { &buses.event_inputs }),
        _ => None,
    }
}

impl Instance {
    pub fn new(manifest: &'static Manifest) -> Self {
        Self {
            inner: Arc::new(Inner {
                manifest,
                session: Mutex::new(None),
                session_thread: AtomicU32::new(0),
                audio: Mutex::new(None),
                setup: Mutex::new(Setup {
                    sample_rate: DEFAULT_RATE,
                    max_frames: DEFAULT_MAX_FRAMES,
                    process_mode: ProcessModes_::kRealtime as i32,
                }),
                latency_samples: AtomicU32::new(0),
                tail_samples: AtomicU32::new(0),
                buses: Mutex::new(BusLayout::default()),
                midi_assignments: Mutex::new(HashMap::new()),
                parameters: Mutex::new(Vec::new()),
                values: Mutex::new(HashMap::new()),
                handler: Mutex::new(None),
                view: Mutex::new(ViewLink::default()),
                events: Arc::new(EventQueue::default()),
            }),
            dispatcher: Mutex::new(None),
        }
    }

    fn start(&self) -> Result<(), String> {
        let inner = &self.inner;
        let dispatcher = Dispatcher::new(inner, Arc::clone(&inner.events))?;
        let queue = Arc::clone(&inner.events);
        let manifest = &inner.manifest;
        let environment = (!manifest.environment.is_empty()).then(|| Path::new(&manifest.environment));
        let mut session = HostSession::spawn(Path::new(&manifest.host), environment, move |event| queue.push(event))?;
        let info = session.load(Some(&inner.manifest.plugin))?;
        inner.remember(info);
        *inner.session.lock().map_err(|_| "the session lock is poisoned")? = Some(session);
        inner.refresh_buses();
        inner.refresh_midi_assignments();
        inner.refresh_parameters();
        *self.dispatcher.lock().map_err(|_| "the dispatcher lock is poisoned")? = Some(dispatcher);
        Ok(())
    }
}

unsafe fn bus_channels(bus: &AudioBusBuffers) -> &[*mut f32] {
    unsafe {
        let channels = bus.__field0.channelBuffers32;
        if channels.is_null() {
            return &[];
        }
        slice::from_raw_parts(channels, bus.numChannels.max(0) as usize)
    }
}

unsafe fn silence_outputs(data: &mut ProcessData) {
    if data.outputs.is_null() || data.numSamples <= 0 || data.symbolicSampleSize != SymbolicSampleSizes_::kSample32 as i32 {
        return;
    }
    unsafe {
        for bus in slice::from_raw_parts_mut(data.outputs, data.numOutputs.max(0) as usize) {
            let channels = bus.__field0.channelBuffers32;
            if channels.is_null() {
                continue;
            }
            for &channel in slice::from_raw_parts(channels, bus.numChannels.max(0) as usize) {
                if !channel.is_null() {
                    ptr::write_bytes(channel, 0, data.numSamples as usize);
                }
            }
            bus.silenceFlags = u64::MAX;
        }
    }
}

impl AudioPath {
    unsafe fn run(&mut self, data: &mut ProcessData) -> bool {
        let frames = data.numSamples.max(0) as u32;
        if frames > self.max_frames || data.symbolicSampleSize != SymbolicSampleSizes_::kSample32 as i32 {
            return false;
        }
        unsafe {
            if frames > 0 {
                self.write_inputs(data, frames);
            }
            if !data.processContext.is_null() {
                let bytes = slice::from_raw_parts(data.processContext.cast::<u8>(), size_of::<ProcessContext>());
                self.channel.write_context(bytes);
            }
            self.collect_input_params(data.inputParameterChanges);
            self.channel.write_input_params(&self.input_params);
            self.collect_input_events(data.inputEvents);
            self.channel.write_input_events(&self.input_events);

            if self.channel.roundtrip(frames, 0, self.timeout_ms) != RoundtripStatus::Ok {
                return false;
            }

            if frames > 0 {
                self.read_outputs(data, frames);
            }
            self.channel.read_output_params(&mut self.output_params);
            self.deliver_output_params(data.outputParameterChanges);
            self.channel.read_output_events(&mut self.output_events);
            self.deliver_output_events(data.outputEvents);
        }
        true
    }

    unsafe fn write_inputs(&mut self, data: &ProcessData, frames: u32) {
        let mut next = 0;
        unsafe {
            if !data.inputs.is_null() {
                for (index, bus) in slice::from_raw_parts(data.inputs, data.numInputs.max(0) as usize).iter().enumerate() {
                    self.channel.write_input_silence(index, bus.silenceFlags);
                    for &channel in bus_channels(bus) {
                        if next >= self.channel.input_channels() {
                            return;
                        }
                        if channel.is_null() {
                            self.channel.clear_input(next, frames);
                        } else {
                            self.channel.write_input(next, slice::from_raw_parts(channel, frames as usize));
                        }
                        next += 1;
                    }
                }
            }
        }
        for channel in next..self.channel.input_channels() {
            self.channel.clear_input(channel, frames);
        }
    }

    unsafe fn read_outputs(&self, data: &mut ProcessData, frames: u32) {
        if data.outputs.is_null() {
            return;
        }
        let mut next = 0;
        unsafe {
            for (index, bus) in slice::from_raw_parts_mut(data.outputs, data.numOutputs.max(0) as usize).iter_mut().enumerate() {
                for &channel in bus_channels(bus) {
                    if channel.is_null() {
                        next += 1;
                    } else if next < self.channel.output_channels() {
                        self.channel.read_output(next, slice::from_raw_parts_mut(channel, frames as usize));
                        next += 1;
                    } else {
                        ptr::write_bytes(channel, 0, frames as usize);
                    }
                }
                bus.silenceFlags = self.channel.read_output_silence(index);
            }
        }
    }

    unsafe fn collect_input_params(&mut self, changes: *mut IParameterChanges) {
        self.input_params.clear();
        unsafe {
            let Some(changes) = ComRef::from_raw(changes) else { return };
            for queue_index in 0..changes.getParameterCount() {
                let Some(queue) = ComRef::from_raw(changes.getParameterData(queue_index)) else { continue };
                let id = queue.getParameterId();
                for point_index in 0..queue.getPointCount() {
                    let (mut sample_offset, mut value) = (0, 0.0);
                    if queue.getPoint(point_index, &mut sample_offset, &mut value) == kResultOk
                        && self.input_params.len() < MAX_PARAM_POINTS
                    {
                        self.input_params.push(ParamPoint { id, sample_offset, value });
                    }
                }
            }
        }
    }

    unsafe fn collect_input_events(&mut self, events: *mut IEventList) {
        self.input_events.clear();
        unsafe {
            let Some(events) = ComRef::from_raw(events) else { return };
            for index in 0..events.getEventCount() {
                let mut event: vst3::Steinberg::Vst::Event = std::mem::zeroed();
                if events.getEvent(index, &mut event) == kResultOk && self.input_events.len() < MAX_EVENTS {
                    self.input_events.extend(to_raw(&event));
                }
            }
        }
    }

    unsafe fn deliver_output_events(&self, events: *mut IEventList) {
        unsafe {
            let Some(events) = ComRef::from_raw(events) else { return };
            for raw in &self.output_events {
                let mut event = from_raw(raw);
                events.addEvent(&mut event);
            }
        }
    }

    unsafe fn deliver_output_params(&self, changes: *mut IParameterChanges) {
        unsafe {
            let Some(changes) = ComRef::from_raw(changes) else { return };
            for point in &self.output_params {
                let mut queue_index = 0;
                let queue = changes.addParameterData(&point.id, &mut queue_index);
                if let Some(queue) = ComRef::from_raw(queue) {
                    let mut point_index = 0;
                    queue.addPoint(point.sample_offset, point.value, &mut point_index);
                }
            }
        }
    }
}

impl IPluginBaseTrait for Instance {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        match self.start() {
            Ok(()) => kResultOk,
            Err(message) => {
                log_error(&format!("{}: {message}", self.inner.manifest.name));
                kResultFalse
            }
        }
    }

    unsafe fn terminate(&self) -> tresult {
        if let Err(message) = self.inner.stop_audio() {
            log_error(&format!("stopping audio failed: {message}"));
        }
        if let Ok(mut handler) = self.inner.handler.lock() {
            *handler = None;
        }
        if let Ok(mut slot) = self.inner.session.lock() {
            if let Some(mut session) = slot.take() {
                session.shutdown();
            }
        }
        if let Ok(mut dispatcher) = self.dispatcher.lock() {
            *dispatcher = None;
        }
        kResultOk
    }
}

impl IComponentTrait for Instance {
    unsafe fn getControllerClassId(&self, _class_id: *mut TUID) -> tresult {
        kResultFalse
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, media_type: MediaType, direction: BusDirection) -> int32 {
        self.inner.bus_count(media_type, direction) as int32
    }

    unsafe fn getBusInfo(&self, media_type: MediaType, direction: BusDirection, index: int32, bus: *mut BusInfo) -> tresult {
        let Some(info) = self.inner.bus(media_type, direction, index) else { return kInvalidArgument };
        let bus = unsafe { &mut *bus };
        bus.mediaType = media_type;
        bus.direction = direction;
        bus.channelCount = info.channels() as int32;
        copy_utf16(&info.name, &mut bus.name);
        bus.busType = info.bus_type;
        bus.flags = info.flags;
        kResultOk
    }

    unsafe fn getRoutingInfo(&self, _input: *mut RoutingInfo, _output: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(&self, media_type: MediaType, direction: BusDirection, index: int32, state: TBool) -> tresult {
        if self.inner.bus(media_type, direction, index).is_none() {
            return kInvalidArgument;
        }
        if media_type == MediaTypes_::kEvent as MediaType {
            return kResultOk;
        }
        let is_output = direction == BusDirections_::kOutput as BusDirection;
        match self.inner.with_session(|session| session.activate_bus(is_output, index, state != 0)) {
            Ok(()) => kResultOk,
            Err(message) => {
                log_error(&format!("activateBus failed: {message}"));
                kResultFalse
            }
        }
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        let inner = &self.inner;
        let result = if state != 0 { inner.stop_audio().and_then(|()| inner.start_audio()) } else { inner.stop_audio() };
        match result {
            Ok(()) => kResultOk,
            Err(message) => {
                log_error(&format!("setActive failed: {message}"));
                kResultFalse
            }
        }
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        self.inner.set_state(StateKind::Component, state)
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        self.inner.get_state(StateKind::Component, state)
    }
}

impl IAudioProcessorTrait for Instance {
    unsafe fn setBusArrangements(&self, inputs: *mut SpeakerArrangement, num_inputs: int32, outputs: *mut SpeakerArrangement, num_outputs: int32) -> tresult {
        let (inputs, outputs) = unsafe {
            let list = |pointer: *mut SpeakerArrangement, count: int32| {
                if pointer.is_null() || count <= 0 { Vec::new() } else { slice::from_raw_parts(pointer, count as usize).to_vec() }
            };
            (list(inputs, num_inputs), list(outputs, num_outputs))
        };
        let accepted = self.inner.with_session(|session| session.set_arrangements(inputs, outputs));
        self.inner.refresh_buses();
        match accepted {
            Ok(true) => kResultTrue,
            Ok(false) => kResultFalse,
            Err(message) => {
                log_error(&format!("setBusArrangements failed: {message}"));
                kResultFalse
            }
        }
    }

    unsafe fn getBusArrangement(&self, direction: BusDirection, index: int32, arrangement: *mut SpeakerArrangement) -> tresult {
        let Some(info) = self.inner.bus(MediaTypes_::kAudio as MediaType, direction, index) else { return kInvalidArgument };
        unsafe { *arrangement = info.arrangement };
        kResultOk
    }

    unsafe fn canProcessSampleSize(&self, size: int32) -> tresult {
        if size == SymbolicSampleSizes_::kSample32 as int32 { kResultTrue } else { kResultFalse }
    }

    unsafe fn getLatencySamples(&self) -> uint32 {
        self.inner.latency_samples.load(Ordering::Relaxed)
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        let setup = unsafe { &*setup };
        let Ok(mut stored) = self.inner.setup.lock() else { return kResultFalse };
        stored.sample_rate = setup.sampleRate;
        stored.max_frames = setup.maxSamplesPerBlock.max(1) as u32;
        stored.process_mode = setup.processMode;
        kResultOk
    }

    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        unsafe {
            let data = &mut *data;
            let done = match self.inner.audio.try_lock() {
                Ok(mut audio) => audio.as_mut().is_some_and(|path| path.run(data)),
                Err(_) => false,
            };
            if !done {
                silence_outputs(data);
            }
        }
        kResultOk
    }

    unsafe fn getTailSamples(&self) -> uint32 {
        self.inner.tail_samples.load(Ordering::Relaxed)
    }
}

impl IEditControllerTrait for Instance {
    unsafe fn setComponentState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        self.inner.set_state(StateKind::Controller, state)
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        self.inner.get_state(StateKind::Controller, state)
    }

    unsafe fn getParameterCount(&self) -> int32 {
        self.inner.parameters.lock().map_or(0, |list| list.len() as int32)
    }

    unsafe fn getParameterInfo(&self, index: int32, info: *mut ParameterInfo) -> tresult {
        let Ok(list) = self.inner.parameters.lock() else { return kResultFalse };
        let Some(parameter) = usize::try_from(index).ok().and_then(|i| list.get(i)) else { return kInvalidArgument };
        let info = unsafe { &mut *info };
        info.id = parameter.id;
        copy_utf16(&parameter.title, &mut info.title);
        copy_utf16(&parameter.short_title, &mut info.shortTitle);
        copy_utf16(&parameter.units, &mut info.units);
        info.stepCount = parameter.step_count;
        info.defaultNormalizedValue = parameter.default_value;
        info.unitId = parameter.unit_id;
        info.flags = parameter.flags;
        kResultOk
    }

    unsafe fn getParamStringByValue(&self, id: ParamID, value: ParamValue, text: *mut String128) -> tresult {
        match self.inner.with_session(|session| session.param_to_text(id, value)) {
            Ok(result) => {
                copy_utf16(&result, unsafe { &mut *text });
                kResultOk
            }
            Err(_) => kResultFalse,
        }
    }

    unsafe fn getParamValueByString(&self, id: ParamID, text: *mut TChar, value: *mut ParamValue) -> tresult {
        let wide = unsafe {
            let mut length = 0;
            while *text.add(length) != 0 {
                length += 1;
            }
            slice::from_raw_parts(text, length)
        };
        let parsed = String::from_utf16_lossy(wide);
        match self.inner.with_session(|session| session.text_to_param(id, parsed)) {
            Ok(result) => {
                unsafe { *value = result };
                kResultOk
            }
            Err(_) => kResultFalse,
        }
    }

    unsafe fn normalizedParamToPlain(&self, id: ParamID, value: ParamValue) -> ParamValue {
        self.inner.with_session(|session| session.normalized_to_plain(id, value)).unwrap_or(value)
    }

    unsafe fn plainParamToNormalized(&self, id: ParamID, value: ParamValue) -> ParamValue {
        self.inner.with_session(|session| session.plain_to_normalized(id, value)).unwrap_or(value)
    }

    unsafe fn getParamNormalized(&self, id: ParamID) -> ParamValue {
        if let Some(value) = self.inner.values.lock().ok().and_then(|values| values.get(&id).copied()) {
            return value;
        }
        let value = self.inner.with_session(|session| session.get_param(id)).unwrap_or(0.0);
        if let Ok(mut values) = self.inner.values.lock() {
            values.insert(id, value);
        }
        value
    }

    unsafe fn setParamNormalized(&self, id: ParamID, value: ParamValue) -> tresult {
        if let Ok(mut values) = self.inner.values.lock() {
            values.remove(&id);
        }
        match self.inner.with_session(|session| session.set_param(id, value)) {
            Ok(()) => kResultOk,
            Err(_) => kResultFalse,
        }
    }

    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        let handler = unsafe { ComRef::from_raw(handler) }.map(|handler| handler.to_com_ptr());
        match self.inner.handler.lock() {
            Ok(mut slot) => {
                *slot = handler;
                kResultOk
            }
            Err(_) => kResultFalse,
        }
    }

    unsafe fn createView(&self, name: FIDString) -> *mut IPlugView {
        if name.is_null() {
            return ptr::null_mut();
        }
        if unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes() != b"editor" {
            return ptr::null_mut();
        }
        let info: EditorInfo = match self.inner.with_session(|session| session.create_editor()) {
            Ok(info) => info,
            Err(message) => {
                log_error(&format!("creating the editor failed: {message}"));
                return ptr::null_mut();
            }
        };
        self.inner.set_view_size(info.width, info.height);
        let view = ComWrapper::new(BridgeView {
            inner: Arc::clone(&self.inner),
            this: Cell::new(ptr::null_mut()),
            info: Cell::new(info),
        });
        let Some(pointer) = view.to_com_ptr::<IPlugView>() else { return ptr::null_mut() };
        view.this.set(pointer.as_ptr());
        pointer.into_raw()
    }
}

impl IMidiMappingTrait for Instance {
    unsafe fn getMidiControllerAssignment(&self, bus: int32, channel: int16, controller: CtrlNumber, id: *mut ParamID) -> tresult {
        let found = self.inner.midi_assignments.lock().ok().and_then(|map| map.get(&(bus, channel, controller)).copied());
        match found {
            Some(found) if !id.is_null() => {
                unsafe { *id = found };
                kResultOk
            }
            _ => kResultFalse,
        }
    }
}

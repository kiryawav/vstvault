use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64};

pub const MAGIC: u32 = 0x3154_4C56;
pub const PROTOCOL_VERSION: u32 = 5;
pub const MAX_BUSES: usize = 32;
pub const MAX_PARAM_POINTS: usize = 2048;
pub const MAX_EVENTS: usize = 1024;
pub const EVENT_BYTES: usize = 64;
pub const CONTEXT_BYTES: usize = 256;

#[repr(C, align(64))]
pub struct CacheLine<T>(pub T);

#[repr(C)]
pub struct Meta {
    pub magic: u32,
    pub version: u32,
    pub max_frames: u32,
    pub input_channels: u32,
    pub output_channels: u32,
}

#[repr(C)]
pub struct Request {
    pub seq: AtomicU64,
    pub num_frames: AtomicU32,
    pub has_context: AtomicU32,
    pub context: UnsafeCell<[u8; CONTEXT_BYTES]>,
    pub input_silence: [AtomicU64; MAX_BUSES],
}

#[repr(C)]
pub struct Control {
    pub host_ready: AtomicU32,
    pub stop: AtomicU32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ParamPoint {
    pub id: u32,
    pub sample_offset: i32,
    pub value: f64,
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawEvent(pub [u8; EVENT_BYTES]);

impl Default for RawEvent {
    fn default() -> Self {
        Self([0; EVENT_BYTES])
    }
}

#[repr(C)]
pub struct Block<T, const N: usize> {
    pub count: AtomicU32,
    pub items: UnsafeCell<[T; N]>,
}

pub type ParamBlock = Block<ParamPoint, MAX_PARAM_POINTS>;
pub type EventBlock = Block<RawEvent, MAX_EVENTS>;

#[repr(C)]
pub struct ChannelHeader {
    pub meta: CacheLine<Meta>,
    pub request: CacheLine<Request>,
    pub response_seq: CacheLine<AtomicU64>,
    pub output_silence: CacheLine<[AtomicU64; MAX_BUSES]>,
    pub host_sleeping: CacheLine<AtomicU32>,
    pub client_sleeping: CacheLine<AtomicU32>,
    pub control: CacheLine<Control>,
    pub input_params: CacheLine<ParamBlock>,
    pub output_params: CacheLine<ParamBlock>,
    pub input_events: CacheLine<EventBlock>,
    pub output_events: CacheLine<EventBlock>,
}

const _: () = assert!(size_of::<ChannelHeader>() % align_of::<f32>() == 0);

pub fn shared_size(max_frames: u32, input_channels: u32, output_channels: u32) -> Option<usize> {
    let channels = (input_channels as usize).checked_add(output_channels as usize)?;
    let samples = (max_frames as usize).checked_mul(channels)?;
    samples.checked_mul(size_of::<f32>())?.checked_add(size_of::<ChannelHeader>())
}

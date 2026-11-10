use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::FromRawHandle;

use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT};

const MAX_MESSAGE_BYTES: u32 = 1 << 30;
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateKind {
    Component,
    Controller,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Load { path: Option<String> },
    GetInfo,
    GetState(StateKind),
    SetState(StateKind, Vec<u8>),
    Activate { channel: String, sample_rate: f64, max_frames: u32, process_mode: i32 },
    Deactivate,
    GetParameters,
    ParamToText { id: u32, value: f64 },
    TextToParam { id: u32, text: String },
    NormalizedToPlain { id: u32, value: f64 },
    PlainToNormalized { id: u32, value: f64 },
    GetParam { id: u32 },
    SetParam { id: u32, value: f64 },
    CreateEditor,
    AttachEditor { parent: u64 },
    DetachEditor,
    DestroyEditor,
    CheckEditorSize { width: i32, height: i32 },
    SetEditorSize { width: i32, height: i32 },
    GetBuses,
    SetArrangements { inputs: Vec<u64>, outputs: Vec<u64> },
    ActivateBus { is_output: bool, index: i32, active: bool },
    ReadIdentity { path: String },
    MidiAssignments,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PluginInfo {
    pub latency_samples: u32,
    pub tail_samples: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusInfo {
    pub name: String,
    pub bus_type: i32,
    pub flags: u32,
    pub arrangement: u64,
}

impl BusInfo {
    pub fn channels(&self) -> u32 {
        self.arrangement.count_ones()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusLayout {
    pub inputs: Vec<BusInfo>,
    pub outputs: Vec<BusInfo>,
    pub event_inputs: Vec<BusInfo>,
    pub event_outputs: Vec<BusInfo>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiAssignment {
    pub bus: i32,
    pub channel: i16,
    pub controller: i16,
    pub id: u32,
}

impl BusLayout {
    pub fn input_channels(&self) -> u32 {
        self.inputs.iter().map(BusInfo::channels).sum()
    }

    pub fn output_channels(&self) -> u32 {
        self.outputs.iter().map(BusInfo::channels).sum()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParamInfo {
    pub id: u32,
    pub title: String,
    pub short_title: String,
    pub units: String,
    pub step_count: i32,
    pub default_value: f64,
    pub unit_id: i32,
    pub flags: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditorInfo {
    pub window: u64,
    pub width: i32,
    pub height: i32,
    pub resizable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Info(PluginInfo),
    State(Vec<u8>),
    Parameters(Vec<ParamInfo>),
    Text(String),
    Value(f64),
    Editor(EditorInfo),
    Size { width: i32, height: i32 },
    Buses(BusLayout),
    Accepted(bool),
    Identity { manifest: String, audio_classes: u32 },
    MidiAssignments(Vec<MidiAssignment>),
    Done,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    BeginEdit { id: u32 },
    PerformEdit { id: u32, value: f64 },
    EndEdit { id: u32 },
    Restart { flags: i32 },
    ResizeView { width: i32, height: i32 },
}

fn kind_byte(kind: StateKind) -> u8 {
    match kind {
        StateKind::Component => 0,
        StateKind::Controller => 1,
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn put_id_value(out: &mut Vec<u8>, id: u32, value: f64) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_pair(out: &mut Vec<u8>, a: i32, b: i32) {
    out.extend_from_slice(&a.to_le_bytes());
    out.extend_from_slice(&b.to_le_bytes());
}

fn put_arrangements(out: &mut Vec<u8>, arrangements: &[u64]) {
    out.extend_from_slice(&(arrangements.len() as u32).to_le_bytes());
    for arrangement in arrangements {
        out.extend_from_slice(&arrangement.to_le_bytes());
    }
}

fn put_buses(out: &mut Vec<u8>, buses: &[BusInfo]) {
    out.extend_from_slice(&(buses.len() as u32).to_le_bytes());
    for bus in buses {
        put_bytes(out, bus.name.as_bytes());
        out.extend_from_slice(&bus.bus_type.to_le_bytes());
        out.extend_from_slice(&bus.flags.to_le_bytes());
        out.extend_from_slice(&bus.arrangement.to_le_bytes());
    }
}

struct Decoder<'a>(&'a [u8]);

impl<'a> Decoder<'a> {
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < count {
            return Err(malformed("message is truncated"));
        }
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("exactly 4 bytes")))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("exactly 8 bytes")))
    }

    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().expect("exactly 4 bytes")))
    }

    fn i16(&mut self) -> io::Result<i16> {
        Ok(i16::from_le_bytes(self.take(2)?.try_into().expect("exactly 2 bytes")))
    }

    fn f64(&mut self) -> io::Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().expect("exactly 8 bytes")))
    }

    fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    fn text(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| malformed("text is not UTF-8"))
    }

    fn kind(&mut self) -> io::Result<StateKind> {
        match self.u8()? {
            0 => Ok(StateKind::Component),
            1 => Ok(StateKind::Controller),
            _ => Err(malformed("unknown state kind")),
        }
    }

    fn arrangements(&mut self) -> io::Result<Vec<u64>> {
        let count = self.u32()? as usize;
        let mut arrangements = Vec::with_capacity(count.min(self.0.len()));
        for _ in 0..count {
            arrangements.push(self.u64()?);
        }
        Ok(arrangements)
    }

    fn buses(&mut self) -> io::Result<Vec<BusInfo>> {
        let count = self.u32()? as usize;
        let mut buses = Vec::with_capacity(count.min(self.0.len()));
        for _ in 0..count {
            buses.push(BusInfo { name: self.text()?, bus_type: self.i32()?, flags: self.u32()?, arrangement: self.u64()? });
        }
        Ok(buses)
    }
}

fn malformed(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Load { path } => {
                out.push(0);
                out.push(u8::from(path.is_some()));
                put_bytes(&mut out, path.as_deref().unwrap_or("").as_bytes());
            }
            Self::GetState(kind) => {
                out.push(1);
                out.push(kind_byte(*kind));
            }
            Self::SetState(kind, state) => {
                out.push(2);
                out.push(kind_byte(*kind));
                put_bytes(&mut out, state);
            }
            Self::Activate { channel, sample_rate, max_frames, process_mode } => {
                out.push(3);
                put_bytes(&mut out, channel.as_bytes());
                out.extend_from_slice(&sample_rate.to_le_bytes());
                out.extend_from_slice(&max_frames.to_le_bytes());
                out.extend_from_slice(&process_mode.to_le_bytes());
            }
            Self::Deactivate => out.push(4),
            Self::Shutdown => out.push(5),
            Self::GetInfo => out.push(6),
            Self::GetParameters => out.push(7),
            Self::ParamToText { id, value } => {
                out.push(8);
                put_id_value(&mut out, *id, *value);
            }
            Self::TextToParam { id, text } => {
                out.push(9);
                out.extend_from_slice(&id.to_le_bytes());
                put_bytes(&mut out, text.as_bytes());
            }
            Self::NormalizedToPlain { id, value } => {
                out.push(10);
                put_id_value(&mut out, *id, *value);
            }
            Self::PlainToNormalized { id, value } => {
                out.push(11);
                put_id_value(&mut out, *id, *value);
            }
            Self::GetParam { id } => {
                out.push(12);
                out.extend_from_slice(&id.to_le_bytes());
            }
            Self::SetParam { id, value } => {
                out.push(13);
                put_id_value(&mut out, *id, *value);
            }
            Self::CreateEditor => out.push(14),
            Self::AttachEditor { parent } => {
                out.push(15);
                out.extend_from_slice(&parent.to_le_bytes());
            }
            Self::DetachEditor => out.push(16),
            Self::DestroyEditor => out.push(17),
            Self::CheckEditorSize { width, height } => {
                out.push(18);
                put_pair(&mut out, *width, *height);
            }
            Self::GetBuses => out.push(19),
            Self::SetArrangements { inputs, outputs } => {
                out.push(20);
                put_arrangements(&mut out, inputs);
                put_arrangements(&mut out, outputs);
            }
            Self::ActivateBus { is_output, index, active } => {
                out.push(21);
                out.push(u8::from(*is_output));
                out.extend_from_slice(&index.to_le_bytes());
                out.push(u8::from(*active));
            }
            Self::ReadIdentity { path } => {
                out.push(22);
                put_bytes(&mut out, path.as_bytes());
            }
            Self::MidiAssignments => out.push(23),
            Self::SetEditorSize { width, height } => {
                out.push(24);
                put_pair(&mut out, *width, *height);
            }
        }
        out
    }

    pub fn decode(message: &[u8]) -> io::Result<Self> {
        let mut d = Decoder(message);
        Ok(match d.u8()? {
            0 => {
                let has_path = d.u8()? != 0;
                let path = d.text()?;
                Self::Load { path: has_path.then_some(path) }
            }
            1 => Self::GetState(d.kind()?),
            2 => Self::SetState(d.kind()?, d.bytes()?.to_vec()),
            3 => Self::Activate { channel: d.text()?, sample_rate: d.f64()?, max_frames: d.u32()?, process_mode: d.i32()? },
            4 => Self::Deactivate,
            5 => Self::Shutdown,
            6 => Self::GetInfo,
            7 => Self::GetParameters,
            8 => Self::ParamToText { id: d.u32()?, value: d.f64()? },
            9 => Self::TextToParam { id: d.u32()?, text: d.text()? },
            10 => Self::NormalizedToPlain { id: d.u32()?, value: d.f64()? },
            11 => Self::PlainToNormalized { id: d.u32()?, value: d.f64()? },
            12 => Self::GetParam { id: d.u32()? },
            13 => Self::SetParam { id: d.u32()?, value: d.f64()? },
            14 => Self::CreateEditor,
            15 => Self::AttachEditor { parent: d.u64()? },
            16 => Self::DetachEditor,
            17 => Self::DestroyEditor,
            18 => Self::CheckEditorSize { width: d.i32()?, height: d.i32()? },
            19 => Self::GetBuses,
            20 => Self::SetArrangements { inputs: d.arrangements()?, outputs: d.arrangements()? },
            21 => Self::ActivateBus { is_output: d.u8()? != 0, index: d.i32()?, active: d.u8()? != 0 },
            22 => Self::ReadIdentity { path: d.text()? },
            23 => Self::MidiAssignments,
            24 => Self::SetEditorSize { width: d.i32()?, height: d.i32()? },
            _ => return Err(malformed("unknown request")),
        })
    }
}

impl ParamInfo {
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        put_bytes(out, self.title.as_bytes());
        put_bytes(out, self.short_title.as_bytes());
        put_bytes(out, self.units.as_bytes());
        out.extend_from_slice(&self.step_count.to_le_bytes());
        out.extend_from_slice(&self.default_value.to_le_bytes());
        out.extend_from_slice(&self.unit_id.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
    }

    fn decode_from(d: &mut Decoder) -> io::Result<Self> {
        Ok(Self {
            id: d.u32()?,
            title: d.text()?,
            short_title: d.text()?,
            units: d.text()?,
            step_count: d.i32()?,
            default_value: d.f64()?,
            unit_id: d.i32()?,
            flags: d.i32()?,
        })
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Info(info) => {
                out.push(0);
                out.extend_from_slice(&info.latency_samples.to_le_bytes());
                out.extend_from_slice(&info.tail_samples.to_le_bytes());
            }
            Self::State(state) => {
                out.push(1);
                put_bytes(&mut out, state);
            }
            Self::Done => out.push(2),
            Self::Error(message) => {
                out.push(3);
                put_bytes(&mut out, message.as_bytes());
            }
            Self::Parameters(parameters) => {
                out.push(4);
                out.extend_from_slice(&(parameters.len() as u32).to_le_bytes());
                for parameter in parameters {
                    parameter.encode_into(&mut out);
                }
            }
            Self::Text(text) => {
                out.push(5);
                put_bytes(&mut out, text.as_bytes());
            }
            Self::Value(value) => {
                out.push(6);
                out.extend_from_slice(&value.to_le_bytes());
            }
            Self::Editor(editor) => {
                out.push(7);
                out.extend_from_slice(&editor.window.to_le_bytes());
                put_pair(&mut out, editor.width, editor.height);
                out.push(u8::from(editor.resizable));
            }
            Self::Size { width, height } => {
                out.push(8);
                put_pair(&mut out, *width, *height);
            }
            Self::Buses(layout) => {
                out.push(9);
                for buses in [&layout.inputs, &layout.outputs, &layout.event_inputs, &layout.event_outputs] {
                    put_buses(&mut out, buses);
                }
            }
            Self::Accepted(accepted) => {
                out.push(10);
                out.push(u8::from(*accepted));
            }
            Self::Identity { manifest, audio_classes } => {
                out.push(11);
                put_bytes(&mut out, manifest.as_bytes());
                out.extend_from_slice(&audio_classes.to_le_bytes());
            }
            Self::MidiAssignments(assignments) => {
                out.push(12);
                out.extend_from_slice(&(assignments.len() as u32).to_le_bytes());
                for assignment in assignments {
                    out.extend_from_slice(&assignment.bus.to_le_bytes());
                    out.extend_from_slice(&assignment.channel.to_le_bytes());
                    out.extend_from_slice(&assignment.controller.to_le_bytes());
                    out.extend_from_slice(&assignment.id.to_le_bytes());
                }
            }
        }
        out
    }

    pub fn decode(message: &[u8]) -> io::Result<Self> {
        let mut d = Decoder(message);
        Ok(match d.u8()? {
            0 => Self::Info(PluginInfo { latency_samples: d.u32()?, tail_samples: d.u32()? }),
            1 => Self::State(d.bytes()?.to_vec()),
            2 => Self::Done,
            3 => Self::Error(d.text()?),
            4 => {
                let count = d.u32()? as usize;
                let mut parameters = Vec::with_capacity(count.min(d.0.len()));
                for _ in 0..count {
                    parameters.push(ParamInfo::decode_from(&mut d)?);
                }
                Self::Parameters(parameters)
            }
            5 => Self::Text(d.text()?),
            6 => Self::Value(d.f64()?),
            7 => Self::Editor(EditorInfo { window: d.u64()?, width: d.i32()?, height: d.i32()?, resizable: d.u8()? != 0 }),
            8 => Self::Size { width: d.i32()?, height: d.i32()? },
            9 => Self::Buses(BusLayout { inputs: d.buses()?, outputs: d.buses()?, event_inputs: d.buses()?, event_outputs: d.buses()? }),
            10 => Self::Accepted(d.u8()? != 0),
            11 => Self::Identity { manifest: d.text()?, audio_classes: d.u32()? },
            12 => {
                let count = d.u32()? as usize;
                let mut assignments = Vec::with_capacity(count.min(d.0.len()));
                for _ in 0..count {
                    let bus = d.i32()?;
                    let channel = d.i16()?;
                    let controller = d.i16()?;
                    assignments.push(MidiAssignment { bus, channel, controller, id: d.u32()? });
                }
                Self::MidiAssignments(assignments)
            }
            _ => return Err(malformed("unknown response")),
        })
    }
}

impl Event {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::BeginEdit { id } => {
                out.push(0);
                out.extend_from_slice(&id.to_le_bytes());
            }
            Self::PerformEdit { id, value } => {
                out.push(1);
                put_id_value(&mut out, *id, *value);
            }
            Self::EndEdit { id } => {
                out.push(2);
                out.extend_from_slice(&id.to_le_bytes());
            }
            Self::Restart { flags } => {
                out.push(3);
                out.extend_from_slice(&flags.to_le_bytes());
            }
            Self::ResizeView { width, height } => {
                out.push(4);
                put_pair(&mut out, *width, *height);
            }
        }
        out
    }

    pub fn decode(message: &[u8]) -> io::Result<Self> {
        let mut d = Decoder(message);
        Ok(match d.u8()? {
            0 => Self::BeginEdit { id: d.u32()? },
            1 => Self::PerformEdit { id: d.u32()?, value: d.f64()? },
            2 => Self::EndEdit { id: d.u32()? },
            3 => Self::Restart { flags: d.i32()? },
            4 => Self::ResizeView { width: d.i32()?, height: d.i32()? },
            _ => return Err(malformed("unknown event")),
        })
    }
}

pub fn write_message(pipe: &mut impl Write, message: &[u8]) -> io::Result<()> {
    let length = u32::try_from(message.len()).ok().filter(|l| *l <= MAX_MESSAGE_BYTES);
    let length = length.ok_or_else(|| malformed("message is too large"))?;
    pipe.write_all(&length.to_le_bytes())?;
    pipe.write_all(message)?;
    pipe.flush()
}

pub fn read_message(pipe: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut prefix = [0; 4];
    pipe.read_exact(&mut prefix)?;
    let length = u32::from_le_bytes(prefix);
    if length > MAX_MESSAGE_BYTES {
        return Err(malformed("message is too large"));
    }
    let mut message = vec![0; length as usize];
    pipe.read_exact(&mut message)?;
    Ok(message)
}

pub fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\vault.{name}")
}

pub fn event_pipe_name(name: &str) -> String {
    format!("{name}.events")
}

pub fn serve_pipe(name: &str) -> io::Result<File> {
    let wide: Vec<u16> = pipe_path(name).encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) } != 0;
    if !connected && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_parameter() -> ParamInfo {
        ParamInfo {
            id: u32::MAX,
            title: "Глубина".into(),
            short_title: "Depth".into(),
            units: "%".into(),
            step_count: 3,
            default_value: 0.5,
            unit_id: -1,
            flags: 65537,
        }
    }

    fn all_requests() -> Vec<Request> {
        vec![
            Request::Load { path: Some("C:\\Плагины\\OTT.vst3".into()) },
            Request::Load { path: None },
            Request::GetInfo,
            Request::GetState(StateKind::Component),
            Request::GetState(StateKind::Controller),
            Request::SetState(StateKind::Component, vec![0, 255, 7]),
            Request::SetState(StateKind::Controller, Vec::new()),
            Request::Activate { channel: "a.b".into(), sample_rate: 44100.5, max_frames: 8192, process_mode: 2 },
            Request::Deactivate,
            Request::GetParameters,
            Request::ParamToText { id: 7, value: 0.25 },
            Request::TextToParam { id: 7, text: "-3 дБ".into() },
            Request::NormalizedToPlain { id: 1, value: 0.5 },
            Request::PlainToNormalized { id: 1, value: 440.0 },
            Request::GetParam { id: 2 },
            Request::SetParam { id: 2, value: 1.0 },
            Request::CreateEditor,
            Request::AttachEditor { parent: u64::MAX },
            Request::DetachEditor,
            Request::DestroyEditor,
            Request::CheckEditorSize { width: 640, height: -1 },
            Request::SetEditorSize { width: 1378, height: 807 },
            Request::GetBuses,
            Request::SetArrangements { inputs: vec![3, 1], outputs: vec![u64::MAX] },
            Request::SetArrangements { inputs: Vec::new(), outputs: Vec::new() },
            Request::ActivateBus { is_output: true, index: 1, active: false },
            Request::ReadIdentity { path: r"C:\Program Files\Плагин.vst3".into() },
            Request::MidiAssignments,
            Request::Shutdown,
        ]
    }

    fn all_responses() -> Vec<Response> {
        vec![
            Response::Info(PluginInfo { latency_samples: 2, tail_samples: u32::MAX }),
            Response::Buses(BusLayout {
                inputs: vec![
                    BusInfo { name: "Вход".into(), bus_type: 0, flags: 1, arrangement: 3 },
                    BusInfo { name: "Sidechain".into(), bus_type: 1, flags: 0, arrangement: 3 },
                ],
                outputs: vec![BusInfo { name: "Output".into(), bus_type: 0, flags: 1, arrangement: 3 }],
                event_inputs: vec![BusInfo { name: "MIDI".into(), bus_type: 0, flags: 1, arrangement: 0xFFFF }],
                event_outputs: Vec::new(),
            }),
            Response::MidiAssignments(vec![MidiAssignment { bus: 0, channel: 15, controller: 129, id: u32::MAX }]),
            Response::MidiAssignments(Vec::new()),
            Response::Buses(BusLayout::default()),
            Response::Accepted(true),
            Response::Accepted(false),
            Response::Identity { manifest: "name=Плагин\n".into(), audio_classes: 2 },
            Response::State(vec![1, 2, 3]),
            Response::State(Vec::new()),
            Response::Parameters(vec![sample_parameter(), ParamInfo::default()]),
            Response::Parameters(Vec::new()),
            Response::Text("-3 дБ".into()),
            Response::Value(0.125),
            Response::Editor(EditorInfo { window: u64::MAX, width: 800, height: 600, resizable: true }),
            Response::Size { width: 1, height: 2 },
            Response::Done,
            Response::Error("не загрузился".into()),
        ]
    }

    fn all_events() -> Vec<Event> {
        vec![
            Event::BeginEdit { id: 3 },
            Event::PerformEdit { id: 3, value: 0.75 },
            Event::EndEdit { id: 3 },
            Event::Restart { flags: 12 },
            Event::ResizeView { width: 900, height: 700 },
        ]
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        for request in all_requests() {
            assert_eq!(Request::decode(&request.encode()).unwrap(), request);
        }
        for response in all_responses() {
            assert_eq!(Response::decode(&response.encode()).unwrap(), response);
        }
        for event in all_events() {
            assert_eq!(Event::decode(&event.encode()).unwrap(), event);
        }
    }

    #[test]
    fn truncated_messages_are_errors_not_panics() {
        for request in all_requests() {
            let bytes = request.encode();
            for length in 0..bytes.len() {
                assert!(Request::decode(&bytes[..length]).is_err(), "{request:?} cut at {length}");
            }
        }
        for response in all_responses() {
            let bytes = response.encode();
            for length in 0..bytes.len() {
                assert!(Response::decode(&bytes[..length]).is_err(), "{response:?} cut at {length}");
            }
        }
        for event in all_events() {
            let bytes = event.encode();
            for length in 0..bytes.len() {
                assert!(Event::decode(&bytes[..length]).is_err(), "{event:?} cut at {length}");
            }
        }
    }

    #[test]
    fn a_corrupted_parameter_count_fails_without_a_huge_allocation() {
        let mut message = vec![4];
        message.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Response::decode(&message).is_err());
    }

    #[test]
    fn corrupted_bus_counts_fail_without_a_huge_allocation() {
        let mut message = vec![9];
        message.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Response::decode(&message).is_err());
        let mut message = vec![20];
        message.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Request::decode(&message).is_err());
    }

    #[test]
    fn a_layout_counts_the_channels_of_all_its_buses() {
        let bus = |arrangement| BusInfo { arrangement, ..BusInfo::default() };
        let layout = BusLayout { inputs: vec![bus(3), bus(1)], outputs: vec![bus(3), bus(3), bus(0b11_1111)], ..BusLayout::default() };
        assert_eq!(layout.input_channels(), 3);
        assert_eq!(layout.output_channels(), 10);
    }

    #[test]
    fn unknown_opcodes_are_errors() {
        assert!(Request::decode(&[99]).is_err());
        assert!(Response::decode(&[99]).is_err());
        assert!(Event::decode(&[99]).is_err());
    }

    #[test]
    fn framing_keeps_messages_apart_and_refuses_absurd_lengths() {
        let mut wire = Vec::new();
        write_message(&mut wire, b"first").unwrap();
        write_message(&mut wire, b"").unwrap();
        write_message(&mut wire, b"third").unwrap();
        let mut reader = wire.as_slice();
        assert_eq!(read_message(&mut reader).unwrap(), b"first");
        assert_eq!(read_message(&mut reader).unwrap(), b"");
        assert_eq!(read_message(&mut reader).unwrap(), b"third");
        assert!(read_message(&mut reader).is_err(), "the stream has ended");

        let absurd = u32::MAX.to_le_bytes();
        assert!(read_message(&mut absurd.as_slice()).is_err());
    }

    #[test]
    fn pipe_carries_messages_both_ways() {
        let name = format!("test.{}", std::process::id());
        let server_name = name.clone();
        let server = std::thread::spawn(move || {
            let mut pipe = serve_pipe(&server_name).unwrap();
            let message = read_message(&mut pipe).unwrap();
            write_message(&mut pipe, &message).unwrap();
        });
        let path = pipe_path(&name);
        let mut client = loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => break file,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        };
        write_message(&mut client, b"ping").unwrap();
        assert_eq!(read_message(&mut client).unwrap(), b"ping");
        server.join().unwrap();
    }
}

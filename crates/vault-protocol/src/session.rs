use std::fs::File;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE};
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CreateEventW, SetEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MSG, MsgWaitForMultipleObjects, PM_NOREMOVE, PM_QS_SENDMESSAGE, PeekMessageW, QS_SENDMESSAGE,
};

use crate::channel::qpc_now;
use crate::control::{
    BusLayout, EditorInfo, Event, MidiAssignment, ParamInfo, PluginInfo, Request, Response, StateKind, event_pipe_name, pipe_path,
    read_message, write_message,
};
use crate::manifest::Manifest;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const LOAD_TIMEOUT: Duration = Duration::from_secs(120);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

struct ReplySignal(HANDLE);

unsafe impl Send for ReplySignal {}
unsafe impl Sync for ReplySignal {}

impl ReplySignal {
    fn new() -> Result<Self, String> {
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(format!("cannot create an event: {}", io::Error::last_os_error()));
        }
        Ok(Self(event))
    }

    fn set(&self) {
        unsafe { SetEvent(self.0) };
    }
}

impl Drop for ReplySignal {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub struct HostSession {
    process: Child,
    requests: Option<Sender<Vec<u8>>>,
    replies: Receiver<io::Result<Vec<u8>>>,
    signal: std::sync::Arc<ReplySignal>,
    threads: Vec<JoinHandle<()>>,
    id: String,
    activations: u32,
}

impl HostSession {
    pub fn spawn(host_exe: &Path, environment: Option<&Path>, on_event: impl FnMut(Event) + Send + 'static) -> Result<Self, String> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id = format!("{}.{}.{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed), qpc_now());
        let mut command = Command::new(host_exe);
        command.args(["--control", &id]);
        if let Some(environment) = environment {
            command.arg("--env").arg(environment);
        }
        let mut process = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", host_exe.display()))?;
        let control = connect(&mut process, &pipe_path(&id))?;
        let events = connect(&mut process, &pipe_path(&event_pipe_name(&id)))?;

        let signal = std::sync::Arc::new(ReplySignal::new()?);
        let (requests, request_queue) = channel();
        let (reply_queue, replies) = channel();
        let worker_signal = std::sync::Arc::clone(&signal);
        let threads = vec![
            std::thread::spawn(move || pipe_worker(control, &request_queue, &reply_queue, &worker_signal)),
            std::thread::spawn(move || event_reader(events, on_event)),
        ];
        Ok(Self { process, requests: Some(requests), replies, signal, threads, id, activations: 0 })
    }

    pub fn process(&self) -> &Child {
        &self.process
    }

    pub fn next_channel_name(&mut self) -> String {
        self.activations += 1;
        format!("{}.{}", self.id, self.activations)
    }

    pub fn load(&mut self, path: Option<&str>) -> Result<PluginInfo, String> {
        let request = Request::Load { path: path.map(str::to_owned) };
        expect_info(self.call_with_timeout(&request, LOAD_TIMEOUT)?)
    }

    pub fn info(&mut self) -> Result<PluginInfo, String> {
        expect_info(self.call(&Request::GetInfo)?)
    }

    pub fn activate(&mut self, channel: &str, sample_rate: f64, max_frames: u32, process_mode: i32) -> Result<PluginInfo, String> {
        expect_info(self.call(&Request::Activate { channel: channel.to_owned(), sample_rate, max_frames, process_mode })?)
    }

    pub fn deactivate(&mut self) -> Result<(), String> {
        expect_done(self.call(&Request::Deactivate)?)
    }

    pub fn get_state(&mut self, kind: StateKind) -> Result<Vec<u8>, String> {
        match self.call(&Request::GetState(kind))? {
            Response::State(state) => Ok(state),
            other => Err(unexpected(other)),
        }
    }

    pub fn set_state(&mut self, kind: StateKind, state: Vec<u8>) -> Result<(), String> {
        expect_done(self.call(&Request::SetState(kind, state))?)
    }

    pub fn parameters(&mut self) -> Result<Vec<ParamInfo>, String> {
        match self.call(&Request::GetParameters)? {
            Response::Parameters(parameters) => Ok(parameters),
            other => Err(unexpected(other)),
        }
    }

    pub fn param_to_text(&mut self, id: u32, value: f64) -> Result<String, String> {
        match self.call(&Request::ParamToText { id, value })? {
            Response::Text(text) => Ok(text),
            other => Err(unexpected(other)),
        }
    }

    pub fn text_to_param(&mut self, id: u32, text: String) -> Result<f64, String> {
        expect_value(self.call(&Request::TextToParam { id, text })?)
    }

    pub fn normalized_to_plain(&mut self, id: u32, value: f64) -> Result<f64, String> {
        expect_value(self.call(&Request::NormalizedToPlain { id, value })?)
    }

    pub fn plain_to_normalized(&mut self, id: u32, value: f64) -> Result<f64, String> {
        expect_value(self.call(&Request::PlainToNormalized { id, value })?)
    }

    pub fn get_param(&mut self, id: u32) -> Result<f64, String> {
        expect_value(self.call(&Request::GetParam { id })?)
    }

    pub fn set_param(&mut self, id: u32, value: f64) -> Result<(), String> {
        expect_done(self.call(&Request::SetParam { id, value })?)
    }

    pub fn create_editor(&mut self) -> Result<EditorInfo, String> {
        expect_editor(self.call(&Request::CreateEditor)?)
    }

    pub fn attach_editor(&mut self, parent: u64) -> Result<EditorInfo, String> {
        expect_editor(self.call(&Request::AttachEditor { parent })?)
    }

    pub fn detach_editor(&mut self) -> Result<(), String> {
        expect_done(self.call(&Request::DetachEditor)?)
    }

    pub fn destroy_editor(&mut self) -> Result<(), String> {
        expect_done(self.call(&Request::DestroyEditor)?)
    }

    pub fn set_editor_size(&mut self, width: i32, height: i32) -> Result<(i32, i32), String> {
        match self.call(&Request::SetEditorSize { width, height })? {
            Response::Size { width, height } => Ok((width, height)),
            other => Err(unexpected(other)),
        }
    }

    pub fn check_editor_size(&mut self, width: i32, height: i32) -> Result<(i32, i32), String> {
        match self.call(&Request::CheckEditorSize { width, height })? {
            Response::Size { width, height } => Ok((width, height)),
            other => Err(unexpected(other)),
        }
    }

    pub fn midi_assignments(&mut self) -> Result<Vec<MidiAssignment>, String> {
        match self.call(&Request::MidiAssignments)? {
            Response::MidiAssignments(assignments) => Ok(assignments),
            other => Err(unexpected(other)),
        }
    }

    pub fn buses(&mut self) -> Result<BusLayout, String> {
        match self.call(&Request::GetBuses)? {
            Response::Buses(layout) => Ok(layout),
            other => Err(unexpected(other)),
        }
    }

    pub fn set_arrangements(&mut self, inputs: Vec<u64>, outputs: Vec<u64>) -> Result<bool, String> {
        match self.call(&Request::SetArrangements { inputs, outputs })? {
            Response::Accepted(accepted) => Ok(accepted),
            other => Err(unexpected(other)),
        }
    }

    pub fn activate_bus(&mut self, is_output: bool, index: i32, active: bool) -> Result<(), String> {
        expect_done(self.call(&Request::ActivateBus { is_output, index, active })?)
    }

    fn describe_exit(&mut self) -> String {
        for _ in 0..50 {
            if let Ok(Some(status)) = self.process.try_wait() {
                return format!("exit code {:#x}", status.code().unwrap_or(-1));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        "still running".into()
    }

    pub fn read_identity(&mut self, path: &str) -> Result<(Manifest, u32), String> {
        match self.call_with_timeout(&Request::ReadIdentity { path: path.to_owned() }, LOAD_TIMEOUT)? {
            Response::Identity { manifest, audio_classes } => Ok((Manifest::parse(&manifest)?, audio_classes)),
            other => Err(unexpected(other)),
        }
    }

    pub fn shutdown(&mut self) -> bool {
        let answered = matches!(self.call(&Request::Shutdown), Ok(Response::Done));
        let started = Instant::now();
        let mut clean = false;
        while started.elapsed() < EXIT_TIMEOUT {
            match self.process.try_wait() {
                Ok(Some(status)) => {
                    clean = answered && status.success();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
        self.stop_threads();
        clean
    }

    fn stop_threads(&mut self) {
        self.requests = None;
        let _ = self.process.kill();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }

    fn call(&mut self, request: &Request) -> Result<Response, String> {
        self.call_with_timeout(request, COMMAND_TIMEOUT)
    }

    fn call_with_timeout(&mut self, request: &Request, timeout: Duration) -> Result<Response, String> {
        let requests = self.requests.as_ref().ok_or("host is gone")?;
        requests.send(request.encode()).map_err(|_| "host is gone")?;
        let started = Instant::now();
        loop {
            match self.replies.try_recv() {
                Ok(Ok(message)) => {
                    return Response::decode(&message).map_err(|e| format!("bad answer from host: {e}"));
                }
                Ok(Err(error)) => return Err(format!("host is gone: {error}; {}", self.describe_exit())),
                Err(TryRecvError::Disconnected) => return Err("host is gone".into()),
                Err(TryRecvError::Empty) => {}
            }
            let Some(left) = timeout.checked_sub(started.elapsed()) else {
                let _ = self.process.kill();
                return Err(format!("host did not answer within {} s and was stopped", timeout.as_secs()));
            };
            wait_handling_sent_messages(&self.signal, left);
        }
    }
}

impl Drop for HostSession {
    fn drop(&mut self) {
        self.stop_threads();
    }
}

fn wait_handling_sent_messages(signal: &ReplySignal, limit: Duration) {
    let handles = [signal.0];
    let millis = limit.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    unsafe {
        MsgWaitForMultipleObjects(1, handles.as_ptr(), 0, millis, QS_SENDMESSAGE);
        let mut message: MSG = std::mem::zeroed();
        PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE);
    }
}

fn pipe_worker(mut pipe: File, requests: &Receiver<Vec<u8>>, replies: &Sender<io::Result<Vec<u8>>>, signal: &ReplySignal) {
    while let Ok(message) = requests.recv() {
        let reply = write_message(&mut pipe, &message).and_then(|()| read_message(&mut pipe));
        let failed = reply.is_err();
        let _ = replies.send(reply);
        signal.set();
        if failed {
            return;
        }
    }
}

fn event_reader(mut pipe: File, mut on_event: impl FnMut(Event)) {
    while let Ok(message) = read_message(&mut pipe) {
        if let Ok(event) = Event::decode(&message) {
            on_event(event);
        }
    }
}

fn connect(process: &mut Child, path: &str) -> Result<File, String> {
    let started = Instant::now();
    loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => return Ok(file),
            Err(error) => {
                let waiting = [Some(ERROR_FILE_NOT_FOUND as i32), Some(ERROR_PIPE_BUSY as i32)].contains(&error.raw_os_error());
                if !waiting {
                    return Err(format!("cannot connect to host: {error}"));
                }
            }
        }
        if let Ok(Some(status)) = process.try_wait() {
            return Err(format!("host exited during start-up: {status}"));
        }
        if started.elapsed() > CONNECT_TIMEOUT {
            let _ = process.kill();
            return Err("host did not open its pipes".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn expect_info(response: Response) -> Result<PluginInfo, String> {
    match response {
        Response::Info(info) => Ok(info),
        other => Err(unexpected(other)),
    }
}

fn expect_done(response: Response) -> Result<(), String> {
    match response {
        Response::Done => Ok(()),
        other => Err(unexpected(other)),
    }
}

fn expect_value(response: Response) -> Result<f64, String> {
    match response {
        Response::Value(value) => Ok(value),
        other => Err(unexpected(other)),
    }
}

fn expect_editor(response: Response) -> Result<EditorInfo, String> {
    match response {
        Response::Editor(editor) => Ok(editor),
        other => Err(unexpected(other)),
    }
}

fn unexpected(response: Response) -> String {
    match response {
        Response::Error(message) => message,
        other => format!("unexpected answer from host: {other:?}"),
    }
}

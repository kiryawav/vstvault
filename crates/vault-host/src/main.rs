use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use vault_plugin::events::{from_raw, to_raw};
use vault_plugin::{Editor, EventSink, Plugin, PluginAudio, ProcessContext};
use vault_protocol::control::{
    BusLayout, Event, PluginInfo, Request, Response, StateKind, event_pipe_name, read_message, serve_pipe, write_message,
};
use vault_protocol::{CONTEXT_BYTES, Channel, MAX_BUSES, MAX_EVENTS, MAX_PARAM_POINTS, realtime};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, PostThreadMessageW, QS_ALLINPUT,
    TranslateMessage, WM_APP,
};

const _: () = assert!(size_of::<ProcessContext>() <= CONTEXT_BYTES);

const STOP_POLL_MS: u32 = 50;
const PUMP_WAIT_MS: u32 = 10;
const ORPHAN_GRACE: Duration = Duration::from_secs(5);

struct Command {
    request: Request,
    reply: Sender<Response>,
}

struct AudioThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl AudioThread {
    fn start(mut shared: Channel, mut audio: Option<PluginAudio>) -> Result<Self, String> {
        let provided = (shared.input_channels() as usize, shared.output_channels() as usize);
        if let Some(expected) = audio.as_ref().map(PluginAudio::channel_counts) {
            if expected != provided {
                return Err(format!("the channel has {provided:?} channels, the plugin expects {expected:?}"));
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let (started, wait_started) = channel();
        let handle = std::thread::spawn(move || {
            serve_audio(&mut shared, audio.as_mut(), &stop_flag, &started);
            drop(audio);
        });
        let _ = wait_started.recv();
        Ok(Self { stop, handle })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

fn serve_audio(shared: &mut Channel, mut plugin: Option<&mut PluginAudio>, stop: &AtomicBool, started: &Sender<()>) {
    realtime::disable_power_throttling();
    realtime::enter_pro_audio_thread();
    let inputs: Vec<_> = (0..shared.input_channels()).map(|channel| shared.input_ptr(channel)).collect();
    let outputs: Vec<_> = (0..shared.output_channels()).map(|channel| shared.output_ptr(channel)).collect();
    let mut input_silence = [0u64; MAX_BUSES];
    let mut output_silence = [0u64; MAX_BUSES];
    let mut context: ProcessContext = unsafe { std::mem::zeroed() };
    let mut input_params = Vec::with_capacity(MAX_PARAM_POINTS);
    let mut output_params = Vec::with_capacity(MAX_PARAM_POINTS);
    let mut raw_events = Vec::with_capacity(MAX_EVENTS);
    let mut events = Vec::with_capacity(MAX_EVENTS);

    shared.mark_host_ready();
    let _ = started.send(());
    while !stop.load(Ordering::Relaxed) && !shared.stop_requested() {
        let Some(seq) = shared.wait_request(0, STOP_POLL_MS) else { continue };
        match plugin.as_deref_mut() {
            Some(plugin) => {
                let bytes = unsafe {
                    std::slice::from_raw_parts_mut(
                        std::ptr::from_mut(&mut context).cast::<u8>(),
                        size_of::<ProcessContext>(),
                    )
                };
                let has_context = shared.read_context(bytes);
                shared.read_input_params(&mut input_params);
                shared.read_input_events(&mut raw_events);
                plugin.set_input_events(raw_events.iter().map(from_raw));
                for (bus, flags) in input_silence.iter_mut().enumerate() {
                    *flags = shared.read_input_silence(bus);
                }
                unsafe {
                    plugin.process_all(
                        &inputs,
                        &outputs,
                        shared.request_frames(),
                        has_context.then_some(&context),
                        &input_params,
                        &mut output_params,
                        &input_silence,
                        &mut output_silence,
                    );
                }
                shared.write_output_params(&output_params);
                plugin.output_events(&mut events);
                raw_events.clear();
                raw_events.extend(events.iter().filter_map(to_raw));
                shared.write_output_events(&raw_events);
                for (bus, flags) in output_silence.iter().enumerate() {
                    shared.write_output_silence(bus, *flags);
                }
            }
            None => shared.pass_through(),
        }
        shared.complete_request(seq);
    }
}

struct Host {
    editor: Option<Editor>,
    plugin: Option<Plugin>,
    audio: Option<AudioThread>,
    sink: EventSink,
}

impl Host {
    fn info(&self) -> PluginInfo {
        match &self.plugin {
            Some(plugin) => PluginInfo {
                latency_samples: plugin.latency_samples(),
                tail_samples: plugin.tail_samples(),
            },
            None => PluginInfo::default(),
        }
    }

    fn plugin(&self) -> Result<&Plugin, String> {
        self.plugin.as_ref().ok_or_else(|| "no plugin is loaded".to_string())
    }

    fn editor(&self) -> Result<&Editor, String> {
        self.editor.as_ref().ok_or_else(|| "the editor is not open".to_string())
    }

    fn deactivate(&mut self) {
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        if let Some(plugin) = &self.plugin {
            plugin.deactivate();
        }
    }

    fn handle(&mut self, request: Request) -> Result<Response, String> {
        match request {
            Request::Load { path } => {
                if let Some(path) = path {
                    self.plugin = Some(Plugin::load(Path::new(&path), self.sink.clone())?);
                }
                Ok(Response::Info(self.info()))
            }
            Request::GetInfo => Ok(Response::Info(self.info())),
            Request::GetState(kind) => {
                let plugin = self.plugin()?;
                Ok(Response::State(match kind {
                    StateKind::Component => plugin.component_state()?,
                    StateKind::Controller => plugin.controller_state()?,
                }))
            }
            Request::SetState(kind, state) => {
                let plugin = self.plugin()?;
                match kind {
                    StateKind::Component => plugin.set_component_state(state)?,
                    StateKind::Controller => plugin.set_controller_state(state)?,
                }
                Ok(Response::Done)
            }
            Request::Activate { channel, sample_rate, max_frames, process_mode } => {
                self.deactivate();
                let shared = Channel::open(&channel).map_err(|e| format!("cannot open channel: {e}"))?;
                let audio = match &self.plugin {
                    Some(plugin) => Some(plugin.activate(sample_rate, max_frames, process_mode)?),
                    None => None,
                };
                self.audio = Some(AudioThread::start(shared, audio)?);
                Ok(Response::Info(self.info()))
            }
            Request::Deactivate => {
                self.deactivate();
                Ok(Response::Done)
            }
            Request::GetParameters => Ok(Response::Parameters(self.plugin()?.parameters())),
            Request::ParamToText { id, value } => Ok(Response::Text(self.plugin()?.param_to_text(id, value)?)),
            Request::TextToParam { id, text } => Ok(Response::Value(self.plugin()?.text_to_param(id, &text)?)),
            Request::NormalizedToPlain { id, value } => Ok(Response::Value(self.plugin()?.normalized_to_plain(id, value))),
            Request::PlainToNormalized { id, value } => Ok(Response::Value(self.plugin()?.plain_to_normalized(id, value))),
            Request::GetParam { id } => Ok(Response::Value(self.plugin()?.param(id))),
            Request::SetParam { id, value } => {
                self.plugin()?.set_param(id, value);
                Ok(Response::Done)
            }
            Request::CreateEditor => {
                self.editor = None;
                let editor = self.plugin()?.create_editor()?;
                let info = editor.info();
                self.editor = Some(editor);
                Ok(Response::Editor(info))
            }
            Request::AttachEditor { parent } => Ok(Response::Editor(self.editor()?.attach(parent)?)),
            Request::DetachEditor => {
                self.editor()?.detach();
                Ok(Response::Done)
            }
            Request::DestroyEditor => {
                self.editor = None;
                Ok(Response::Done)
            }
            Request::SetEditorSize { width, height } => {
                let editor = self.editor()?;
                editor.resize(width, height);
                let info = editor.info();
                if (info.width, info.height) != (width, height) && info.width > 0 && info.height > 0 {
                    editor.resize(info.width, info.height);
                }
                Ok(Response::Size { width: info.width, height: info.height })
            }
            Request::CheckEditorSize { width, height } => {
                let (width, height) = self.editor()?.check_size(width, height);
                Ok(Response::Size { width, height })
            }
            Request::GetBuses => match &self.plugin {
                Some(plugin) => Ok(Response::Buses(plugin.buses()?)),
                None => Ok(Response::Buses(BusLayout::default())),
            },
            Request::SetArrangements { inputs, outputs } => {
                Ok(Response::Accepted(self.plugin()?.set_arrangements(&inputs, &outputs)))
            }
            Request::ActivateBus { is_output, index, active } => {
                self.plugin()?.activate_bus(is_output, index, active);
                Ok(Response::Done)
            }
            Request::MidiAssignments => Ok(Response::MidiAssignments(self.plugin()?.midi_assignments())),
            Request::ReadIdentity { path } => {
                let (mut manifest, audio_classes) = vault_plugin::read_identity(Path::new(&path))?;
                manifest.plugin = path;
                manifest.host = std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
                Ok(Response::Identity { manifest: manifest.to_text()?, audio_classes })
            }
            Request::Shutdown => {
                self.shut_down();
                Ok(Response::Done)
            }
        }
    }

    fn shut_down(&mut self) {
        self.editor = None;
        self.deactivate();
        self.plugin = None;
    }
}

fn serve_commands(name: &str, commands: &Sender<Option<Command>>, main_thread: u32) {
    let wake_main = || {
        unsafe { PostThreadMessageW(main_thread, WM_APP, 0, 0) };
    };
    if let Ok(mut pipe) = serve_pipe(name) {
        while let Ok(message) = read_message(&mut pipe) {
            let response = match Request::decode(&message) {
                Err(error) => Response::Error(format!("bad request: {error}")),
                Ok(request) => {
                    let is_shutdown = request == Request::Shutdown;
                    let (reply, answer) = channel();
                    if commands.send(Some(Command { request, reply })).is_err() {
                        return;
                    }
                    wake_main();
                    let response = answer.recv().unwrap_or_else(|_| Response::Error("host is stopping".into()));
                    if is_shutdown {
                        let _ = write_message(&mut pipe, &response.encode());
                        return;
                    }
                    response
                }
            };
            if write_message(&mut pipe, &response.encode()).is_err() {
                break;
            }
        }
    }
    let _ = commands.send(None);
    wake_main();
    std::thread::sleep(ORPHAN_GRACE);
    vault_plugin::end_process(2);
}

fn serve_events(name: &str, events: &Receiver<Event>) {
    let Ok(mut pipe) = serve_pipe(name) else { return };
    while let Ok(event) = events.recv() {
        if write_message(&mut pipe, &event.encode()).is_err() {
            return;
        }
    }
}

fn pump_window_messages(editor: Option<&Editor>) {
    unsafe {
        MsgWaitForMultipleObjects(0, std::ptr::null(), 0, PUMP_WAIT_MS, QS_ALLINPUT);
        let mut message: MSG = std::mem::zeroed();
        while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
            if let Some(editor) = editor {
                editor.pass_on_key(&message);
            }
        }
    }
}

fn parse_arguments() -> Result<(String, Option<PathBuf>), String> {
    let usage = || "usage: vault-host --control NAME [--env DIR]".to_string();
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some("--control".as_ref()) {
        return Err(usage());
    }
    let name = args.next().ok_or_else(usage)?.into_string().map_err(|_| usage())?;
    let environment = match args.next() {
        None => None,
        Some(flag) if flag == "--env" => Some(PathBuf::from(args.next().ok_or_else(usage)?)),
        Some(_) => return Err(usage()),
    };
    Ok((name, environment))
}

fn run() -> Result<(), String> {
    let (name, environment) = parse_arguments()?;
    if let Some(environment) = environment {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let tools = exe.parent().ok_or("this program has no folder")?.to_string_lossy().into_owned();
        vault_env::enter(&environment, &tools)?;
    }
    let (send_command, receive_command) = channel();
    let (send_event, receive_event) = channel();
    let main_thread = unsafe { GetCurrentThreadId() };
    let event_pipe = event_pipe_name(&name);
    let reader = std::thread::spawn(move || serve_commands(&name, &send_command, main_thread));
    std::thread::spawn(move || serve_events(&event_pipe, &receive_event));

    let send_event = std::sync::Mutex::new(send_event);
    let sink: EventSink = Arc::new(move |event| {
        if let Ok(sender) = send_event.lock() {
            let _ = sender.send(event);
        }
    });
    let mut host = Host { editor: None, plugin: None, audio: None, sink };
    loop {
        pump_window_messages(host.editor.as_ref());
        while let Ok(command) = receive_command.try_recv() {
            let Some(Command { request, reply }) = command else {
                host.shut_down();
                return Err("the client disconnected".into());
            };
            let is_shutdown = request == Request::Shutdown;
            let response = host.handle(request).unwrap_or_else(Response::Error);
            let _ = reply.send(response);
            if is_shutdown {
                let _ = reader.join();
                return Ok(());
            }
        }
    }
}

fn main() -> ! {
    match run() {
        Ok(()) => vault_plugin::end_process(0),
        Err(message) => {
            eprintln!("vault-host: {message}");
            vault_plugin::end_process(1)
        }
    }
}

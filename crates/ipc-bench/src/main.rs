use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use vault_plugin::events::{note_off, note_on, to_raw};
use vault_plugin::{EventSink, Plugin, PluginAudio};
use vault_protocol::control::{BusInfo, Event, ParamInfo};
use vault_protocol::{Channel, HostSession, MAX_BUSES, ParamPoint, RoundtripStatus, qpc_freq, qpc_now, realtime};
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThread, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, EnumChildWindows, GetClientRect, GetWindowThreadProcessId, MSG,
    PM_REMOVE, PeekMessageW, PostMessageW, SW_SHOWNORMAL, ShowWindow, TranslateMessage, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const PARAM_CAN_AUTOMATE: i32 = 1;
const PARAM_IS_READ_ONLY: i32 = 2;
const PARAM_IS_PROGRAM_CHANGE: i32 = 32768;
const PARAM_IS_BYPASS: i32 = 65536;
const AUTOMATED_PARAMS: usize = 8;
const COMPARED_TEXT_PARAMS: usize = 32;
const MK_LBUTTON: usize = 1;

fn no_events() -> EventSink {
    Arc::new(|_| {})
}

const CHANNELS: u32 = 2;
const NOISE_SAMPLES: usize = 1 << 16;
const PLUGIN_SIGNAL_LEVEL: f32 = 0.25;
const NULL_TEST_TIMEOUT_MS: u32 = 5000;
const REALTIME_MODE: i32 = 0;
const CSV_HEADER: &str = "block,rate,instances,spin_us,host_spin_us,load,callbacks,budget_us,p50_us,p99_us,p999_us,\
max_us,max_pct_budget,deadline_misses,timeouts,busy,mismatches,client_cpu_pct,hosts_cpu_pct,start_late_max_us";

struct Options {
    block: u32,
    rate: u32,
    instances: u32,
    spin_us: u32,
    load: u32,
    seconds: u32,
    timeout_ms: u32,
    csv_header: bool,
    plugin: Option<PathBuf>,
    compare_with: Option<PathBuf>,
    in_process: bool,
    null_test: bool,
    notes: bool,
    state_test: bool,
    param_test: bool,
    sidechain_test: bool,
    editor_test: bool,
    resize_test: bool,
    no_input: bool,
    start_test: bool,
    environment: Option<PathBuf>,
    out: Option<PathBuf>,
}

impl Options {
    fn emit(&self, line: &str) -> Result<(), String> {
        let Some(path) = &self.out else {
            println!("{line}");
            return Ok(());
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        writeln!(file, "{line}").map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

fn parse_options() -> Result<Options, String> {
    let mut o = Options {
        block: 128,
        rate: 48000,
        instances: 1,
        spin_us: 0,
        load: 0,
        seconds: 10,
        timeout_ms: 100,
        csv_header: false,
        plugin: None,
        compare_with: None,
        in_process: false,
        null_test: false,
        notes: false,
        state_test: false,
        param_test: false,
        sidechain_test: false,
        editor_test: false,
        resize_test: false,
        no_input: false,
        start_test: false,
        environment: None,
        out: None,
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let flag = flag.to_string_lossy().into_owned();
        match flag.as_str() {
            "--csv-header" => {
                o.csv_header = true;
                continue;
            }
            "--in-process" => {
                o.in_process = true;
                continue;
            }
            "--null-test" => {
                o.null_test = true;
                continue;
            }
            "--note-test" => {
                o.null_test = true;
                o.notes = true;
                continue;
            }
            "--state-test" => {
                o.state_test = true;
                continue;
            }
            "--param-test" => {
                o.param_test = true;
                continue;
            }
            "--sidechain-test" => {
                o.sidechain_test = true;
                continue;
            }
            "--resize-test" => {
                o.resize_test = true;
                continue;
            }
            "--editor-test" => {
                o.editor_test = true;
                continue;
            }
            "--no-input" => {
                o.no_input = true;
                continue;
            }
            "--start-test" => {
                o.start_test = true;
                continue;
            }
            _ => {}
        }
        let value = args.next().ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--plugin" => {
                o.plugin = Some(PathBuf::from(value));
                continue;
            }
            "--compare-with" => {
                o.compare_with = Some(PathBuf::from(value));
                continue;
            }
            "--out" => {
                o.out = Some(PathBuf::from(value));
                continue;
            }
            "--env" => {
                o.environment = Some(PathBuf::from(value));
                continue;
            }
            _ => {}
        }
        let value = value.to_string_lossy();
        let number: u32 = value.parse().map_err(|_| format!("bad value for {flag}: {value}"))?;
        match flag.as_str() {
            "--block" => o.block = number,
            "--rate" => o.rate = number,
            "--instances" => o.instances = number,
            "--spin-us" => o.spin_us = number,
            "--load" => o.load = number,
            "--seconds" => o.seconds = number,
            "--timeout-ms" => o.timeout_ms = number,
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    if o.block == 0 || o.rate == 0 {
        return Err("block and rate must be positive".into());
    }
    if (o.in_process || o.null_test || o.state_test || o.param_test || o.sidechain_test || o.editor_test || o.resize_test || o.start_test)
        && o.plugin.is_none()
    {
        return Err("this mode needs --plugin".into());
    }
    if (o.state_test || o.param_test || o.sidechain_test) && o.compare_with.is_none() {
        return Err("--state-test, --param-test and --sidechain-test need --compare-with".into());
    }
    Ok(o)
}

enum Backend {
    Hosted { channel: Channel, session: Option<HostSession> },
    Direct { audio: PluginAudio, plugin: Plugin },
}

struct Slot {
    backend: Backend,
    sent: Vec<f32>,
    received: Vec<f32>,
    input_params: Vec<ParamPoint>,
    output_params: Vec<ParamPoint>,
    input_events: Vec<vault_plugin::Event>,
}

impl Slot {
    fn new(backend: Backend, block: u32) -> Self {
        let samples = block as usize * CHANNELS as usize;
        Self {
            backend,
            sent: vec![0.0; samples],
            received: vec![0.0; samples],
            input_params: Vec::new(),
            output_params: Vec::new(),
            input_events: Vec::new(),
        }
    }

    fn run_block(&mut self, block: u32, spin_us: u32, timeout_ms: u32) -> RoundtripStatus {
        let frames = block as usize;
        match &mut self.backend {
            Backend::Hosted { channel, .. } => {
                for (ch, samples) in self.sent.chunks_exact(frames).enumerate().take(channel.input_channels() as usize) {
                    channel.write_input(ch as u32, samples);
                }
                channel.write_input_params(&self.input_params);
                let raw: Vec<_> = self.input_events.iter().filter_map(to_raw).collect();
                channel.write_input_events(&raw);
                let status = channel.roundtrip(block, spin_us, timeout_ms);
                if status == RoundtripStatus::Ok {
                    for (ch, samples) in self.received.chunks_exact_mut(frames).enumerate().take(channel.output_channels() as usize) {
                        channel.read_output(ch as u32, samples);
                    }
                }
                status
            }
            Backend::Direct { audio, .. } => {
                audio.set_input_events(self.input_events.iter().copied());
                let (sent, received) = (self.sent.as_mut_ptr(), self.received.as_mut_ptr());
                unsafe {
                    audio.process_stereo(
                        [sent, sent.add(frames)],
                        [received, received.add(frames)],
                        block,
                        None,
                        &self.input_params,
                        &mut self.output_params,
                    );
                }
                RoundtripStatus::Ok
            }
        }
    }

    fn host_cycles(&self) -> u64 {
        match &self.backend {
            Backend::Hosted { session: Some(session), .. } => process_cycles(session.process()),
            _ => 0,
        }
    }

    fn stop(&mut self) -> bool {
        let Backend::Hosted { channel, session } = &mut self.backend else { return true };
        channel.request_stop();
        session.take().is_none_or(|mut session| {
            let ok = session.shutdown();
            if !ok {
                eprintln!("vault-host did not end cleanly");
            }
            ok
        })
    }
}

fn make_noise(samples: usize, level: f32) -> Vec<f32> {
    let mut state: u32 = 0x1234_5678;
    (0..samples)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state as i32 as f32 / 2_147_483_648.0 * level
        })
        .collect()
}

fn fill_block(noise: &[f32], index: u64, out: &mut [f32]) {
    let offset = (index.wrapping_mul(61) % NOISE_SAMPLES as u64) as usize;
    out.copy_from_slice(&noise[offset..offset + out.len()]);
}

fn tag_block(index: u64, block: usize, out: &mut [f32]) {
    let tag = f32::from_bits(index as u32);
    for start in (0..out.len()).step_by(block) {
        out[start] = tag;
    }
}

fn same_bits(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[(p * (sorted.len() - 1) as f64) as usize]
}

fn start_hosted(o: &Options) -> Result<Slot, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut session = HostSession::spawn(&exe.with_file_name("vault-host.exe"), None, |_| {})?;
    let plugin = o.plugin.as_ref().map(|path| path.to_string_lossy().into_owned());
    session.load(plugin.as_deref())?;
    let (input_channels, output_channels) = if plugin.is_some() { stereo_main_over_session(&mut session)? } else { (CHANNELS, CHANNELS) };
    let name = session.next_channel_name();
    let channel = Channel::create(&name, o.block, input_channels, output_channels)
        .map_err(|e| format!("cannot create channel: {e}"))?;
    session.activate(&name, f64::from(o.rate), o.block, REALTIME_MODE)?;
    Ok(Slot::new(Backend::Hosted { channel, session: Some(session) }, o.block))
}

fn stereo_main_over_session(session: &mut HostSession) -> Result<(u32, u32), String> {
    const STEREO: u64 = 3;
    let layout = session.buses()?;
    let wanted = |buses: &[BusInfo]| -> Vec<u64> {
        buses.iter().enumerate().map(|(index, bus)| if index == 0 { STEREO } else { bus.arrangement }).collect()
    };
    session.set_arrangements(wanted(&layout.inputs), wanted(&layout.outputs))?;
    let layout = session.buses()?;
    for (is_output, buses) in [(false, &layout.inputs), (true, &layout.outputs)] {
        for index in 0..buses.len() {
            session.activate_bus(is_output, index as i32, index == 0)?;
        }
    }
    Ok((layout.input_channels(), layout.output_channels()))
}

fn start_direct(path: &Path, o: &Options) -> Result<Slot, String> {
    audio_slot(Plugin::load(path, no_events())?, o)
}

fn audio_slot(plugin: Plugin, o: &Options) -> Result<Slot, String> {
    plugin.use_stereo_main()?;
    let audio = plugin.activate(f64::from(o.rate), o.block, REALTIME_MODE)?;
    Ok(Slot::new(Backend::Direct { audio, plugin }, o.block))
}

fn thread_cycles() -> u64 {
    let mut cycles = 0;
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
    cycles
}

fn process_cycles(process: &Child) -> u64 {
    let mut cycles = 0;
    unsafe { QueryProcessCycleTime(process.as_raw_handle(), &mut cycles) };
    cycles
}

fn tsc() -> u64 {
    unsafe { std::arch::x86_64::_rdtsc() }
}

fn plugin_label(path: &Path) -> String {
    path.file_stem().unwrap_or_default().to_string_lossy().replace(',', " ")
}

fn notes_for_block(index: u64, block: u32, rate: u32) -> Vec<vault_plugin::Event> {
    let (period, length) = (u64::from(rate) / 4, u64::from(rate) / 5);
    let (start, end) = (index * u64::from(block), (index + 1) * u64::from(block));
    let pitch = |k: u64| 48 + (k * 7 % 24) as i16;
    let mut events = Vec::new();
    for k in start.saturating_sub(length) / period..=end / period {
        let (on, off) = (k * period, k * period + length);
        if (start..end).contains(&on) {
            events.push(note_on((on - start) as i32, pitch(k), 0.8));
        }
        if (start..end).contains(&off) {
            events.push(note_off((off - start) as i32, pitch(k)));
        }
    }
    events.sort_by_key(|event| event.sampleOffset);
    events
}

fn midi_assignments(slot: &Slot) -> Vec<vault_protocol::control::MidiAssignment> {
    match &slot.backend {
        Backend::Direct { plugin, .. } => plugin.midi_assignments(),
        Backend::Hosted { .. } => Vec::new(),
    }
}

fn null_test(o: &Options, path: &Path) -> Result<bool, String> {
    let mut first = start_direct(path, o)?;
    let (mut second, against) = match &o.compare_with {
        Some(reference) => (start_direct(reference, o)?, plugin_label(reference)),
        None => (start_hosted(o)?, "vault-host".to_string()),
    };
    let latency = match &first.backend {
        Backend::Direct { plugin, .. } => plugin.latency_samples(),
        Backend::Hosted { .. } => 0,
    };

    let noise = make_noise(NOISE_SAMPLES + first.sent.len(), PLUGIN_SIGNAL_LEVEL);
    let total = u64::from(o.seconds) * u64::from(o.rate) / u64::from(o.block);
    let (mut mismatched, mut first_mismatch, mut changed, mut failed) = (0u64, None, 0u64, 0u64);
    let mut peak = 0.0f32;
    for index in 0..total {
        fill_block(&noise, index, &mut first.sent);
        fill_block(&noise, index, &mut second.sent);
        if o.notes {
            first.input_events = notes_for_block(index, o.block, o.rate);
            second.input_events.clone_from(&first.input_events);
        }
        first.run_block(o.block, 0, 0);
        if second.run_block(o.block, 0, NULL_TEST_TIMEOUT_MS) != RoundtripStatus::Ok {
            failed += 1;
            continue;
        }
        if !same_bits(&first.received, &second.received) {
            mismatched += 1;
            first_mismatch.get_or_insert(index);
        }
        if !same_bits(&first.sent, &first.received) {
            changed += 1;
        }
        peak = first.received.iter().fold(peak, |p, s| p.max(s.abs()));
    }
    let (assignments, midi_map_same) = if o.notes {
        let assignments = midi_assignments(&first);
        let same = matches!(second.backend, Backend::Hosted { .. }) || assignments == midi_assignments(&second);
        (assignments.len(), same)
    } else {
        (0, true)
    };
    let host_ok = second.stop();

    let passed = mismatched == 0 && failed == 0 && changed > 0 && peak > 0.0 && host_ok && midi_map_same;
    o.emit(&format!(
        "{} vs {against}: {}{} blocks={total} block_size={} mismatched={mismatched} first_mismatch={} failed={failed} \
         changed_by_plugin={changed} peak={peak:.3} reported_latency={latency} host_exit_ok={host_ok}{}",
        plugin_label(path),
        if o.notes { "notes " } else { "" },
        if passed { "PASS" } else { "FAIL" },
        o.block,
        first_mismatch.map_or("-".to_string(), |i: u64| i.to_string()),
        if o.notes { format!(" midi_controllers={assignments} midi_map_same={midi_map_same}") } else { String::new() },
    ))?;
    Ok(passed)
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(offset) => offset.to_string(),
        None if a.len() != b.len() => format!("length {} vs {}", a.len(), b.len()),
        None => "-".to_string(),
    }
}

fn state_test(o: &Options, bridge_path: &Path, original_path: &Path) -> Result<bool, String> {
    let original = Plugin::load(original_path, no_events())?;
    let component = original.component_state()?;
    let controller = original.controller_state()?;

    let control = Plugin::load(original_path, no_events())?;
    let fresh_state = control.component_state()?;
    control.set_component_state(component.clone())?;
    control.set_controller_state(controller.clone())?;
    let control_saved = control.component_state()?;

    let bridge = Plugin::load(bridge_path, no_events())?;
    bridge.set_component_state(component.clone())?;
    bridge.set_controller_state(controller.clone())?;
    let bridge_saved = bridge.component_state()?;
    let controller_saved = bridge.controller_state()?;

    let reopened = Plugin::load(original_path, no_events())?;
    reopened.set_component_state(bridge_saved.clone())?;

    let (mut with_control, mut with_bridge, mut with_reopened) = (audio_slot(control, o)?, audio_slot(bridge, o)?, audio_slot(reopened, o)?);
    let noise = make_noise(NOISE_SAMPLES + with_control.sent.len(), PLUGIN_SIGNAL_LEVEL);
    let total = u64::from(o.seconds) * u64::from(o.rate) / u64::from(o.block);
    let (mut bridge_differs, mut reopened_differs) = (0u64, 0u64);
    for index in 0..total {
        for slot in [&mut with_control, &mut with_bridge, &mut with_reopened] {
            fill_block(&noise, index, &mut slot.sent);
            slot.run_block(o.block, 0, 0);
        }
        bridge_differs += u64::from(!same_bits(&with_control.received, &with_bridge.received));
        reopened_differs += u64::from(!same_bits(&with_control.received, &with_reopened.received));
    }

    let bytes_stable = component == fresh_state;
    let bytes_match = !bytes_stable || (control_saved == bridge_saved && controller == controller_saved);
    let passed = !component.is_empty() && bridge_differs == 0 && reopened_differs == 0 && bytes_match;
    o.emit(&format!(
        "{}: state {} blocks={total} component_bytes={} controller_bytes={} audio_differs_via_bridge={bridge_differs} \
         audio_differs_reopened={reopened_differs} bytes_compared={bytes_stable} bridge_saved_vs_control_saved={} \
         original_vs_control_saved={} two_fresh_loads={}",
        plugin_label(original_path),
        if passed { "PASS" } else { "FAIL" },
        component.len(),
        controller.len(),
        first_difference(&control_saved, &bridge_saved),
        first_difference(&component, &control_saved),
        first_difference(&component, &fresh_state),
    ))?;
    Ok(passed)
}

fn can_be_automated(parameter: &ParamInfo) -> bool {
    parameter.flags & PARAM_CAN_AUTOMATE != 0
        && parameter.flags & (PARAM_IS_READ_ONLY | PARAM_IS_PROGRAM_CHANGE | PARAM_IS_BYPASS) == 0
}

fn automation_points(ids: &[u32], index: u64, block: u32) -> Vec<ParamPoint> {
    let mut points = Vec::with_capacity(ids.len() * 2);
    for (position, &id) in ids.iter().enumerate() {
        let phase = index as f64 * 0.02 + position as f64;
        for (offset, shift) in [(0, 0.0), (block as i32 / 2, 0.01)] {
            points.push(ParamPoint { id, sample_offset: offset, value: 0.5 + 0.45 * (phase + shift).sin() });
        }
    }
    points
}

fn param_test(o: &Options, bridge_path: &Path, original_path: &Path) -> Result<bool, String> {
    let original = Plugin::load(original_path, no_events())?;
    let bridge = Plugin::load(bridge_path, no_events())?;
    let default_state = original.component_state()?;

    let parameters = original.parameters();
    let list_same = parameters == bridge.parameters();
    let (mut text_differs, mut plain_differs, mut values_read) = (0u32, 0u32, 0u32);
    for parameter in parameters.iter().take(COMPARED_TEXT_PARAMS) {
        for value in [0.0, 0.25, 0.5, 0.75, 1.0] {
            values_read += 1;
            text_differs += u32::from(original.param_to_text(parameter.id, value).ok() != bridge.param_to_text(parameter.id, value).ok());
            let (a, b) = (original.normalized_to_plain(parameter.id, value), bridge.normalized_to_plain(parameter.id, value));
            plain_differs += u32::from(a.to_bits() != b.to_bits());
        }
    }
    let ids: Vec<u32> = parameters.iter().filter(|p| can_be_automated(p)).take(AUTOMATED_PARAMS).map(|p| p.id).collect();
    let mut value_mismatches = 0u32;
    let mut first_value_mismatch = "-".to_string();
    for &id in &ids {
        original.set_param(id, 0.3);
        bridge.set_param(id, 0.3);
        let (from_original, from_bridge) = (original.param(id), bridge.param(id));
        if from_original.to_bits() != from_bridge.to_bits() {
            if value_mismatches == 0 {
                first_value_mismatch = format!("{id}:{from_original}/{from_bridge}");
            }
            value_mismatches += 1;
        }
        original.set_param(id, original.param(id));
    }

    let control = Plugin::load(original_path, no_events())?;
    let (mut with_original, mut with_bridge, mut without_automation) = (audio_slot(original, o)?, audio_slot(bridge, o)?, audio_slot(control, o)?);
    let noise = make_noise(NOISE_SAMPLES + with_original.sent.len(), PLUGIN_SIGNAL_LEVEL);
    let total = u64::from(o.seconds) * u64::from(o.rate) / u64::from(o.block);
    let (mut audio_differs, mut automation_audible) = (0u64, 0u64);
    for index in 0..total {
        let points = automation_points(&ids, index, o.block);
        for slot in [&mut with_original, &mut with_bridge, &mut without_automation] {
            fill_block(&noise, index, &mut slot.sent);
        }
        with_original.input_params.clone_from(&points);
        with_bridge.input_params.clone_from(&points);
        for slot in [&mut with_original, &mut with_bridge, &mut without_automation] {
            slot.run_block(o.block, 0, 0);
        }
        audio_differs += u64::from(!same_bits(&with_original.received, &with_bridge.received));
        automation_audible += u64::from(!same_bits(&with_original.received, &without_automation.received));
    }

    let Backend::Direct { plugin: automated, .. } = &with_original.backend else { return Err("unreachable".into()) };
    let changed_state = automated.component_state()?;
    let state_changed = changed_state != default_state;
    let fresh_original = Plugin::load(original_path, no_events())?;
    let fresh_bridge = Plugin::load(bridge_path, no_events())?;
    fresh_original.set_component_state(changed_state.clone())?;
    fresh_bridge.set_component_state(changed_state)?;
    let (mut restored_original, mut restored_bridge) = (audio_slot(fresh_original, o)?, audio_slot(fresh_bridge, o)?);
    let mut restored_differs = 0u64;
    for index in 0..total {
        for slot in [&mut restored_original, &mut restored_bridge] {
            fill_block(&noise, index, &mut slot.sent);
            slot.run_block(o.block, 0, 0);
        }
        restored_differs += u64::from(!same_bits(&restored_original.received, &restored_bridge.received));
    }

    let passed = list_same
        && text_differs == 0
        && plain_differs == 0
        && value_mismatches == 0
        && audio_differs == 0
        && restored_differs == 0;
    o.emit(&format!(
        "{}: params {} count={} list_same={list_same} values_compared={values_read} text_differs={text_differs} \
         plain_differs={plain_differs} automated={} value_mismatches={value_mismatches} \
         first_value_mismatch={first_value_mismatch} blocks={total} \
         audio_differs={audio_differs} automation_changed_sound_in={automation_audible} state_changed={state_changed} \
         restored_differs={restored_differs}",
        plugin_label(original_path),
        if passed { "PASS" } else { "FAIL" },
        parameters.len(),
        ids.len(),
    ))?;
    Ok(passed)
}

struct SidechainSlot {
    audio: PluginAudio,
    _plugin: Plugin,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
}

impl SidechainSlot {
    fn start(plugin: Plugin, o: &Options) -> Result<Self, String> {
        plugin.use_stereo_main()?;
        plugin.activate_bus(false, 1, true);
        let audio = plugin.activate(f64::from(o.rate), o.block, REALTIME_MODE)?;
        let (input_count, output_count) = audio.channel_counts();
        let channel = || vec![0.0f32; o.block as usize];
        Ok(Self {
            audio,
            _plugin: plugin,
            inputs: (0..input_count).map(|_| channel()).collect(),
            outputs: (0..output_count).map(|_| channel()).collect(),
        })
    }

    fn run_block(&mut self, block: u32) {
        let inputs: Vec<*mut f32> = self.inputs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let outputs: Vec<*mut f32> = self.outputs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let mut output_params = Vec::new();
        let mut output_silence = [0u64; MAX_BUSES];
        unsafe {
            self.audio.process_all(&inputs, &outputs, block, None, &[], &mut output_params, &[0; MAX_BUSES], &mut output_silence);
        }
    }

    fn same_output_as(&self, other: &Self) -> bool {
        self.outputs.len() == other.outputs.len() && self.outputs.iter().zip(&other.outputs).all(|(a, b)| same_bits(a, b))
    }
}

fn sidechain_test(o: &Options, bridge_path: &Path, original_path: &Path) -> Result<bool, String> {
    let original = Plugin::load(original_path, no_events())?;
    let bridge = Plugin::load(bridge_path, no_events())?;
    let layout = original.buses()?;
    let layout_same = layout == bridge.buses()?;
    let describe: Vec<String> = layout.inputs.iter().map(|bus| format!("{}/{}ch", bus.name, bus.channels())).collect();
    if layout.inputs.len() < 2 {
        o.emit(&format!(
            "{}: sidechain {} no second input bus, inputs=[{}] layout_same={layout_same}",
            plugin_label(original_path),
            if layout_same { "PASS" } else { "FAIL" },
            describe.join(" "),
        ))?;
        return Ok(layout_same);
    }

    let control = Plugin::load(original_path, no_events())?;
    let (mut through_original, mut through_bridge, mut silent_sidechain) =
        (SidechainSlot::start(original, o)?, SidechainSlot::start(bridge, o)?, SidechainSlot::start(control, o)?);
    let main_channels = through_original.inputs[0].len() * 2;
    let noise = make_noise(NOISE_SAMPLES + main_channels * 2, PLUGIN_SIGNAL_LEVEL);
    let total = u64::from(o.seconds) * u64::from(o.rate) / u64::from(o.block);
    let (mut differs, mut sidechain_audible) = (0u64, 0u64);
    for index in 0..total {
        for slot in [&mut through_original, &mut through_bridge, &mut silent_sidechain] {
            for (channel, samples) in slot.inputs.iter_mut().enumerate() {
                fill_block(&noise, index + channel as u64 * 1000, samples);
            }
        }
        for samples in silent_sidechain.inputs.iter_mut().skip(2) {
            samples.fill(0.0);
        }
        through_original.run_block(o.block);
        through_bridge.run_block(o.block);
        silent_sidechain.run_block(o.block);
        differs += u64::from(!through_original.same_output_as(&through_bridge));
        sidechain_audible += u64::from(!through_original.same_output_as(&silent_sidechain));
    }

    let passed = layout_same && differs == 0;
    o.emit(&format!(
        "{}: sidechain {} inputs=[{}] layout_same={layout_same} blocks={total} audio_differs={differs} \
         sidechain_changed_sound_in={sidechain_audible}",
        plugin_label(original_path),
        if passed { "PASS" } else { "FAIL" },
        describe.join(" "),
    ))?;
    Ok(passed)
}

fn pump_for(duration: Duration) {
    let started = Instant::now();
    while started.elapsed() < duration {
        unsafe {
            let mut message: MSG = std::mem::zeroed();
            while PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}

unsafe extern "system" fn collect_window(window: HWND, list: LPARAM) -> i32 {
    unsafe { (*(list as *mut Vec<HWND>)).push(window) };
    1
}

fn child_windows(parent: HWND) -> Vec<HWND> {
    let mut windows: Vec<HWND> = Vec::new();
    unsafe { EnumChildWindows(parent, Some(collect_window), &raw mut windows as LPARAM) };
    windows
}

fn window_process(window: HWND) -> u32 {
    let mut process = 0;
    unsafe { GetWindowThreadProcessId(window, &mut process) };
    process
}

fn drag_over_window(window: HWND) {
    let mut area = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetClientRect(window, &mut area) };
    let position = |x: i32, y: i32| ((y as isize) << 16 | (x as isize & 0xFFFF)) as LPARAM;
    for row in 1..6 {
        for column in 1..6 {
            let x = area.right * column / 6;
            let y = area.bottom * row / 6;
            unsafe {
                PostMessageW(window, WM_MOUSEMOVE, 0, position(x, y));
                PostMessageW(window, WM_LBUTTONDOWN, MK_LBUTTON, position(x, y));
                for step in 1..6 {
                    PostMessageW(window, WM_MOUSEMOVE, MK_LBUTTON, position(x, y - step * 4));
                }
                PostMessageW(window, WM_LBUTTONUP, 0, position(x, y - 20));
            }
            pump_for(Duration::from_millis(30));
        }
    }
}

fn keys_returned_to(top: HWND, plugin_window: HWND) -> u32 {
    const VK_SPACE: usize = 0x20;
    const SPACE_DOWN: LPARAM = 0x0039_0001;
    const SPACE_UP: LPARAM = 0xC039_0001_u32 as LPARAM;
    unsafe {
        PostMessageW(plugin_window, WM_KEYDOWN, VK_SPACE, SPACE_DOWN);
        PostMessageW(plugin_window, WM_KEYUP, VK_SPACE, SPACE_UP);
    }
    let mut returned = 0;
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(500) {
        unsafe {
            let mut message: MSG = std::mem::zeroed();
            while PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if message.message == WM_KEYDOWN && message.hwnd == top && message.wParam == VK_SPACE {
                    returned += 1;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    returned
}

fn editor_test(o: &Options, path: &Path) -> Result<bool, String> {
    let edits = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&edits);
    let sink: EventSink = Arc::new(move |event| {
        if matches!(event, Event::PerformEdit { .. }) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    });
    let plugin = Plugin::load(path, sink)?;
    let editor = plugin.create_editor()?;
    let size = editor.info();

    let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "PluginVault editor test".encode_utf16().chain(Some(0)).collect();
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            size.width + 32,
            size.height + 64,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        )
    };
    if window.is_null() {
        return Err(format!("cannot create the test window: {}", std::io::Error::last_os_error()));
    }
    unsafe {
        ShowWindow(window, SW_SHOWNORMAL);
        ShowWindow(window, SW_SHOWNORMAL);
    }
    let attached = editor.attach(window as u64)?;
    pump_for(Duration::from_millis(500));

    let me = std::process::id();
    let opened = child_windows(window);
    let in_other_process = opened.iter().filter(|w| window_process(**w) != me).count();
    let mut keys_returned = 0;
    if let Some(&target) = opened.last().filter(|_| !o.no_input) {
        drag_over_window(target);
        keys_returned = keys_returned_to(window, target);
    }
    pump_for(Duration::from_secs(u64::from(o.seconds)));

    let mut resized = "-".to_string();
    if attached.resizable {
        let (width, height) = editor.check_size(attached.width + 40, attached.height + 40);
        editor.resize(width, height);
        pump_for(Duration::from_millis(500));
        let reported = editor.info();
        resized = format!("{}x{}", reported.width, reported.height);
    }

    editor.detach();
    pump_for(Duration::from_millis(300));
    let left = child_windows(window).len();
    drop(editor);
    unsafe { DestroyWindow(window) };
    let alive_after_close = plugin.component_state().is_ok();

    let keys_ok = o.no_input || in_other_process == 0 || keys_returned == 1;
    let passed = !opened.is_empty() && left == 0 && alive_after_close && keys_ok;
    o.emit(&format!(
        "{}: editor {} size={}x{} resizable={} windows={} in_other_process={in_other_process} left_after_close={left} \
         alive_after_close={alive_after_close} edits_received={} keys_returned={keys_returned} resized_to={resized}",
        plugin_label(path),
        if passed { "PASS" } else { "FAIL" },
        attached.width,
        attached.height,
        attached.resizable,
        opened.len(),
        edits.load(Ordering::Relaxed),
    ))?;
    Ok(passed)
}

fn client_size(window: HWND) -> (i32, i32) {
    let mut area = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetClientRect(window, &mut area) };
    (area.right - area.left, area.bottom - area.top)
}

fn resize_steps(path: &Path, follow: bool) -> Result<Vec<String>, String> {
    let requested = Arc::new(std::sync::Mutex::new(None));
    let latest = Arc::clone(&requested);
    let sink: EventSink = Arc::new(move |event| {
        if let Event::ResizeView { width, height } = event {
            if let Ok(mut latest) = latest.lock() {
                *latest = Some((width, height));
            }
        }
    });
    let plugin = Plugin::load(path, sink)?;
    let editor = plugin.create_editor()?;
    let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "PluginVault resize test".encode_utf16().chain(Some(0)).collect();
    let start = editor.info();
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            50,
            50,
            start.width + 400,
            start.height + 400,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        )
    };
    if window.is_null() {
        return Err(format!("cannot create the test window: {}", std::io::Error::last_os_error()));
    }
    unsafe {
        ShowWindow(window, SW_SHOWNORMAL);
        ShowWindow(window, SW_SHOWNORMAL);
    }
    let attached = editor.attach(window as u64)?;
    pump_for(Duration::from_millis(500));
    let mut lines = vec![format!("attached {}x{} resizable={}", attached.width, attached.height, attached.resizable)];
    let (w, h) = (attached.width, attached.height);
    for (asked_w, asked_h) in [(w / 2, h / 2), (w * 3 / 2, h * 3 / 2), (w, h * 3 / 2), (w * 3 / 2, h), (w, h)] {
        let (agreed_w, agreed_h) = editor.check_size(asked_w, asked_h);
        editor.resize(agreed_w, agreed_h);
        pump_for(Duration::from_millis(400));
        let mut followed = 0;
        while let Some((plugin_w, plugin_h)) = requested.lock().ok().and_then(|mut latest| latest.take()) {
            followed += 1;
            if !follow {
                continue;
            }
            editor.resize(plugin_w, plugin_h);
            pump_for(Duration::from_millis(300));
            if followed > 5 {
                break;
            }
        }
        let frame = editor.info();
        let windows: Vec<String> = child_windows(window)
            .iter()
            .map(|child| {
                let (cw, ch) = client_size(*child);
                format!("{cw}x{ch}")
            })
            .collect();
        lines.push(format!(
            "asked {asked_w}x{asked_h} agreed {agreed_w}x{agreed_h} plugin_asked_after={followed} view={}x{} windows=[{}]",
            frame.width,
            frame.height,
            windows.join(" ")
        ));
    }
    editor.detach();
    drop(editor);
    unsafe { DestroyWindow(window) };
    Ok(lines)
}

fn resize_test(o: &Options, path: &Path, reference: Option<&Path>) -> Result<bool, String> {
    let through = resize_steps(path, true)?;
    let original = match reference {
        Some(reference) => Some(resize_steps(reference, true)?),
        None => None,
    };
    for line in resize_steps(path, false)? {
        o.emit(&format!("{}: resize, daw ignores the plugin: {line}", plugin_label(path)))?;
    }
    for (index, line) in through.iter().enumerate() {
        o.emit(&format!("{}: resize {line}", plugin_label(path)))?;
        if let Some(original) = &original {
            o.emit(&format!("{}: resize {}", plugin_label(reference.unwrap_or(path)), original.get(index).map_or("-", String::as_str)))?;
        }
    }
    Ok(true)
}

fn start_test(o: &Options, path: &Path) -> Result<bool, String> {
    const RUNS: u32 = 5;
    let label = plugin_label(path);
    let report = |mode: &str, run: u32, stages: [Duration; 4]| {
        let ms: Vec<u128> = stages.iter().map(Duration::as_millis).collect();
        o.emit(&format!(
            "{label}: start mode={mode} run={run} host_ms={} load_ms={} describe_ms={} editor_ms={} total_ms={}",
            ms[0], ms[1], ms[2], ms[3], ms.iter().sum::<u128>(),
        ))
    };

    if o.in_process {
        for run in 1..=RUNS {
            let started = Instant::now();
            let plugin = Plugin::load(path, no_events())?;
            let loaded = started.elapsed();
            plugin.buses()?;
            plugin.parameters();
            let described = started.elapsed();
            let editor = plugin.create_editor()?;
            let with_editor = started.elapsed();
            drop(editor);
            report("in-process", run, [Duration::ZERO, loaded, described - loaded, with_editor - described])?;
        }
        return Ok(true);
    }

    let host = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("vault-host.exe");
    let mode = if o.environment.is_some() { "environment" } else { "host" };
    let plugin = path.to_string_lossy();
    for run in 1..=RUNS {
        let started = Instant::now();
        let mut session = HostSession::spawn(&host, o.environment.as_deref(), |_| {})?;
        let spawned = started.elapsed();
        session.load(Some(&plugin))?;
        let loaded = started.elapsed();
        session.buses()?;
        session.parameters()?;
        let described = started.elapsed();
        session.create_editor()?;
        let with_editor = started.elapsed();
        session.shutdown();
        report(mode, run, [spawned, loaded - spawned, described - loaded, with_editor - described])?;
    }
    Ok(true)
}

fn run_paced(o: &Options) -> Result<bool, String> {
    let prefix = o.plugin.as_ref().map(|path| {
        format!("{},{},", if o.in_process { "in-process" } else { "hosted" }, plugin_label(path))
    });
    if o.csv_header {
        let columns = if prefix.is_some() { "mode,plugin," } else { "" };
        o.emit(&format!("{columns}{CSV_HEADER}"))?;
    }
    realtime::disable_power_throttling();

    let mut slots = Vec::with_capacity(o.instances as usize);
    for _ in 0..o.instances {
        slots.push(match (&o.plugin, o.in_process) {
            (Some(path), true) => start_direct(path, o)?,
            _ => start_hosted(o)?,
        });
    }

    let stop_load = Arc::new(AtomicBool::new(false));
    let load: Vec<_> = (0..o.load)
        .map(|_| {
            let stop = Arc::clone(&stop_load);
            thread::spawn(move || {
                let mut x = 1.0f64;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..10_000 {
                        x = std::hint::black_box(x * 1.000_000_1 + 0.000_000_1);
                    }
                    if x > 1e6 {
                        x = 1.0;
                    }
                }
            })
        })
        .collect();

    if !realtime::enter_pro_audio_thread() {
        eprintln!("warning: MMCSS unavailable: {}", std::io::Error::last_os_error());
    }

    let block = o.block as usize;
    let verify = o.plugin.is_none();
    let level = if verify { 1.0 } else { PLUGIN_SIGNAL_LEVEL };
    let noise = make_noise(NOISE_SAMPLES + block * CHANNELS as usize, level);
    let freq = qpc_freq();
    let budget_us = 1e6 * f64::from(o.block) / f64::from(o.rate);
    let period = freq * i64::from(o.block) / i64::from(o.rate);
    let warmup = u64::from(o.rate / o.block);
    let total = warmup + u64::from(o.seconds) * u64::from(o.rate) / u64::from(o.block);

    let timer = unsafe {
        CreateWaitableTimerExW(ptr::null(), ptr::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS)
    };
    if timer.is_null() {
        return Err(format!("cannot create high-resolution timer: {}", std::io::Error::last_os_error()));
    }

    let mut callback_us = Vec::with_capacity(total as usize);
    let (mut timeouts, mut busy, mut mismatches, mut misses) = (0u64, 0u64, 0u64, 0u64);
    let mut start_late_max_us = 0.0f64;
    let (mut client_begin, mut tsc_begin) = (0, 0);
    let mut hosts_begin = vec![0u64; slots.len()];
    let mut next = qpc_now() + period;

    for index in 0..total {
        let measuring = index >= warmup;
        if index == warmup {
            client_begin = thread_cycles();
            for (begin, slot) in hosts_begin.iter_mut().zip(&slots) {
                *begin = slot.host_cycles();
            }
            tsc_begin = tsc();
        }

        let now = qpc_now();
        if now < next {
            let due = -((next - now) * 10_000_000 / freq);
            unsafe {
                SetWaitableTimer(timer, &due, 0, None, ptr::null(), 0);
                WaitForSingleObject(timer, INFINITE);
            }
        }

        let started = qpc_now();
        if measuring {
            start_late_max_us = start_late_max_us.max(1e6 * (started - next) as f64 / freq as f64);
        }
        for slot in &mut slots {
            fill_block(&noise, index, &mut slot.sent);
            if verify {
                tag_block(index, block, &mut slot.sent);
            }
        }

        let t0 = qpc_now();
        for slot in &mut slots {
            match slot.run_block(o.block, o.spin_us, o.timeout_ms) {
                RoundtripStatus::Ok => {
                    if verify && measuring && !same_bits(&slot.sent, &slot.received) {
                        mismatches += 1;
                    }
                }
                RoundtripStatus::Timeout if measuring => timeouts += 1,
                RoundtripStatus::Busy if measuring => busy += 1,
                _ => {}
            }
        }
        let t1 = qpc_now();

        if measuring {
            let us = 1e6 * (t1 - t0) as f64 / freq as f64;
            callback_us.push(us);
            if us > budget_us {
                misses += 1;
            }
        }

        next += period;
        let after = qpc_now();
        if after > next + 2 * period {
            next = after + period;
        }
    }

    let tsc_elapsed = tsc().wrapping_sub(tsc_begin) as f64;
    let client_cycles = thread_cycles() - client_begin;
    let host_cycles: u64 = slots.iter().zip(&hosts_begin).map(|(s, begin)| s.host_cycles() - begin).sum();

    stop_load.store(true, Ordering::Relaxed);
    for handle in load {
        let _ = handle.join();
    }
    let mut hosts_ok = true;
    for slot in &mut slots {
        hosts_ok &= slot.stop();
    }

    callback_us.sort_by(f64::total_cmp);
    let max_us = callback_us.last().copied().unwrap_or(0.0);
    o.emit(&format!(
        "{}{},{},{},{},{},{},{},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{},{},{},{},{:.2},{:.2},{:.1}",
        prefix.unwrap_or_default(),
        o.block,
        o.rate,
        o.instances,
        o.spin_us,
        0,
        o.load,
        callback_us.len(),
        budget_us,
        percentile(&callback_us, 0.50),
        percentile(&callback_us, 0.99),
        percentile(&callback_us, 0.999),
        max_us,
        100.0 * max_us / budget_us,
        misses,
        timeouts,
        busy,
        if verify { mismatches.to_string() } else { "-".to_string() },
        100.0 * client_cycles as f64 / tsc_elapsed,
        100.0 * host_cycles as f64 / tsc_elapsed,
        start_late_max_us,
    ))?;
    Ok(mismatches == 0 && hosts_ok)
}

fn main() -> ! {
    let options = match parse_options() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("ipc-bench: {message}");
            vault_plugin::end_process(2)
        }
    };
    let result = match (&options.plugin, &options.compare_with) {
        (Some(path), Some(reference)) if options.state_test => state_test(&options, path, reference),
        (Some(path), Some(reference)) if options.param_test => param_test(&options, path, reference),
        (Some(path), Some(reference)) if options.sidechain_test => sidechain_test(&options, path, reference),
        (Some(path), _) if options.editor_test => editor_test(&options, path),
        (Some(path), reference) if options.resize_test => resize_test(&options, path, reference.as_deref()),
        (Some(path), _) if options.start_test => start_test(&options, path),
        (Some(path), _) if options.null_test => null_test(&options, path),
        _ => run_paced(&options),
    };
    match result {
        Ok(true) => vault_plugin::end_process(0),
        Ok(false) => vault_plugin::end_process(3),
        Err(message) => {
            eprintln!("ipc-bench: {message}");
            vault_plugin::end_process(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_catches_a_single_flipped_bit() {
        let a = [1.0f32, f32::from_bits(0x7FC0_0001), 3.0];
        let mut b = a;
        assert!(same_bits(&a, &b), "identical NaN patterns must compare equal");
        b[2] = f32::from_bits(b[2].to_bits() ^ 1);
        assert!(!same_bits(&a, &b));
    }

    #[test]
    fn check_tells_zero_from_negative_zero() {
        assert!(!same_bits(&[0.0], &[-0.0]));
    }

    #[test]
    fn a_stale_block_never_passes_for_the_current_one() {
        let noise = make_noise(NOISE_SAMPLES + 256, 1.0);
        let (mut current, mut stale) = (vec![0.0; 256], vec![0.0; 256]);
        for index in 1..5000u64 {
            fill_block(&noise, index, &mut current);
            tag_block(index, 128, &mut current);
            fill_block(&noise, index - 1, &mut stale);
            tag_block(index - 1, 128, &mut stale);
            assert!(!same_bits(&current, &stale));
        }
    }

    #[test]
    fn every_channel_of_a_block_carries_the_tag() {
        let mut block = vec![0.0; 256];
        tag_block(7, 128, &mut block);
        assert_eq!(block[0].to_bits(), 7);
        assert_eq!(block[128].to_bits(), 7);
    }

    #[test]
    fn automation_stays_in_range_and_changes_over_time() {
        let first = automation_points(&[1, 2], 0, 128);
        let later = automation_points(&[1, 2], 200, 128);
        assert_eq!(first.len(), 4);
        assert!(first.iter().chain(&later).all(|p| (0.0..=1.0).contains(&p.value)));
        assert_ne!(first, later);
        assert_eq!(first[1].sample_offset, 64, "the second point sits in the middle of the block");
    }

    #[test]
    fn only_ordinary_parameters_are_automated() {
        let with_flags = |flags| ParamInfo { flags, ..ParamInfo::default() };
        assert!(can_be_automated(&with_flags(PARAM_CAN_AUTOMATE)));
        assert!(!can_be_automated(&with_flags(0)));
        assert!(!can_be_automated(&with_flags(PARAM_CAN_AUTOMATE | PARAM_IS_BYPASS)));
        assert!(!can_be_automated(&with_flags(PARAM_CAN_AUTOMATE | PARAM_IS_READ_ONLY)));
        assert!(!can_be_automated(&with_flags(PARAM_CAN_AUTOMATE | PARAM_IS_PROGRAM_CHANGE)));
    }

    #[test]
    fn plugin_signal_stays_at_the_requested_level() {
        let noise = make_noise(4096, PLUGIN_SIGNAL_LEVEL);
        assert!(noise.iter().all(|s| s.abs() <= PLUGIN_SIGNAL_LEVEL));
        assert!(noise.iter().any(|s| s.abs() > PLUGIN_SIGNAL_LEVEL / 2.0));
    }
}

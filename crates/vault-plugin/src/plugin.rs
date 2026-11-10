use std::cell::Cell;
use std::sync::Arc;
use std::path::Path;
use std::ptr;

use vault_protocol::ParamPoint;
use vault_protocol::control::{BusInfo as VaultBusInfo, BusLayout, MidiAssignment, ParamInfo};
use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper, Interface};

use crate::editor::Editor;
pub use crate::host_objects::EventSink;
use crate::host_objects::{ChangeList, ComponentHandler, EventList, HostContext, MemoryStream, PlugFrame};
use crate::module::Module;

const AUDIO: MediaType = MediaTypes_::kAudio as MediaType;
const EVENT: MediaType = MediaTypes_::kEvent as MediaType;
const INPUT: BusDirection = BusDirections_::kInput as BusDirection;
const OUTPUT: BusDirection = BusDirections_::kOutput as BusDirection;
const AUDIO_MODULE_CLASS: &[u8] = b"Audio Module Class";
const TEMPO: f64 = 120.0;

struct Controller {
    edit: ComPtr<IEditController>,
    separate: bool,
    links: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
}

pub struct Plugin {
    controller: Option<Controller>,
    processor: ComPtr<IAudioProcessor>,
    component: ComPtr<IComponent>,
    host_context: ComPtr<IHostApplication>,
    handler: ComPtr<IComponentHandler>,
    sink: EventSink,
    module: Module,

    initialized: bool,
    active: Cell<bool>,
}

unsafe impl Send for Plugin {}

impl Plugin {
    pub fn load(path: &Path, sink: EventSink) -> Result<Self, String> {
        let module = Module::load(path)?;
        let handler = ComWrapper::new(ComponentHandler { sink: sink.clone() })
            .to_com_ptr::<IComponentHandler>()
            .ok_or("host object")?;
        let host_context = ComWrapper::new(HostContext).to_com_ptr::<IHostApplication>().ok_or("host object")?;
        if let Some(factory3) = module.factory().cast::<IPluginFactory3>() {
            unsafe { factory3.setHostContext(host_context.as_ptr().cast()) };
        }
        let component = create_component(module.factory())?;
        let processor = component.cast::<IAudioProcessor>().ok_or("the class is not an audio processor")?;
        let mut plugin = Self {
            controller: None,
            processor,
            component,
            host_context,
            handler,
            sink,
            module,
            initialized: false,
            active: Cell::new(false),
        };
        plugin.configure()?;
        Ok(plugin)
    }

    pub fn buses(&self) -> Result<BusLayout, String> {
        Ok(BusLayout {
            inputs: self.buses_of(INPUT)?,
            outputs: self.buses_of(OUTPUT)?,
            event_inputs: self.event_buses_of(INPUT),
            event_outputs: self.event_buses_of(OUTPUT),
        })
    }

    fn event_buses_of(&self, direction: BusDirection) -> Vec<VaultBusInfo> {
        unsafe {
            (0..self.component.getBusCount(EVENT, direction).max(0))
                .filter_map(|index| {
                    let mut info: BusInfo = std::mem::zeroed();
                    (self.component.getBusInfo(EVENT, direction, index, &mut info) == kResultOk).then(|| VaultBusInfo {
                        name: from_utf16(&info.name),
                        bus_type: info.busType,
                        flags: info.flags,
                        arrangement: (1u64 << info.channelCount.clamp(0, 63)) - 1,
                    })
                })
                .collect()
        }
    }

    pub fn midi_assignments(&self) -> Vec<MidiAssignment> {
        let Some(mapping) = self.controller.as_ref().and_then(|controller| controller.edit.cast::<IMidiMapping>()) else {
            return Vec::new();
        };
        let mut assignments = Vec::new();
        for (bus, info) in self.event_buses_of(INPUT).iter().enumerate() {
            for channel in 0..info.channels().min(16) as i16 {
                for controller in 0..ControllerNumbers_::kCountCtrlNumber as i16 {
                    let mut id = 0;
                    if unsafe { mapping.getMidiControllerAssignment(bus as int32, channel, controller, &mut id) } == kResultOk {
                        assignments.push(MidiAssignment { bus: bus as i32, channel, controller, id });
                    }
                }
            }
        }
        assignments
    }

    pub fn set_arrangements(&self, inputs: &[u64], outputs: &[u64]) -> bool {
        let (mut inputs, mut outputs) = (inputs.to_vec(), outputs.to_vec());
        unsafe {
            self.processor.setBusArrangements(
                inputs.as_mut_ptr(),
                inputs.len() as int32,
                outputs.as_mut_ptr(),
                outputs.len() as int32,
            ) == kResultOk
        }
    }

    pub fn activate_bus(&self, is_output: bool, index: i32, active: bool) {
        let direction = if is_output { OUTPUT } else { INPUT };
        unsafe { self.component.activateBus(AUDIO, direction, index, u8::from(active)) };
    }

    pub fn use_stereo_main(&self) -> Result<(), String> {
        let layout = self.buses()?;
        let arrangements = |buses: &[VaultBusInfo]| -> Vec<u64> {
            let main_is_stereo = |(index, bus): (usize, &VaultBusInfo)| if index == 0 { SpeakerArr::kStereo } else { bus.arrangement };
            buses.iter().enumerate().map(main_is_stereo).collect()
        };
        if layout.outputs.is_empty() {
            return Err("the plugin has no audio output".into());
        }
        self.set_arrangements(&arrangements(&layout.inputs), &arrangements(&layout.outputs));
        let layout = self.buses()?;
        let stereo = |bus: &VaultBusInfo| bus.arrangement == SpeakerArr::kStereo;
        if !layout.inputs.first().is_none_or(stereo) || !layout.outputs.first().is_some_and(stereo) {
            return Err("the plugin refused a stereo main bus".into());
        }
        for (is_output, buses) in [(false, &layout.inputs), (true, &layout.outputs)] {
            for index in 0..buses.len() {
                self.activate_bus(is_output, index as i32, index == 0);
            }
        }
        Ok(())
    }

    fn buses_of(&self, direction: BusDirection) -> Result<Vec<VaultBusInfo>, String> {
        unsafe {
            let arrangements = self.bus_arrangements(direction)?;
            arrangements
                .into_iter()
                .enumerate()
                .map(|(index, arrangement)| {
                    let mut info: BusInfo = std::mem::zeroed();
                    check(self.component.getBusInfo(AUDIO, direction, index as int32, &mut info), "getBusInfo")?;
                    Ok(VaultBusInfo { name: from_utf16(&info.name), bus_type: info.busType, flags: info.flags, arrangement })
                })
                .collect()
        }
    }

    pub fn latency_samples(&self) -> u32 {
        unsafe { self.processor.getLatencySamples() }
    }

    pub fn tail_samples(&self) -> u32 {
        unsafe { self.processor.getTailSamples() }
    }

    pub fn activate(&self, sample_rate: f64, max_frames: u32, process_mode: i32) -> Result<PluginAudio, String> {
        let mut setup = ProcessSetup {
            processMode: process_mode,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: max_frames as int32,
            sampleRate: sample_rate,
        };
        unsafe {
            for direction in [INPUT, OUTPUT] {
                for index in 0..self.component.getBusCount(EVENT, direction).max(0) {
                    self.component.activateBus(EVENT, direction, index, 1);
                }
            }
            check(self.processor.setupProcessing(&mut setup), "setupProcessing")?;
            check(self.component.setActive(1), "setActive")?;
        }
        self.active.set(true);
        let (inputs, outputs) = unsafe { (self.bus_arrangements(INPUT)?, self.bus_arrangements(OUTPUT)?) };
        PluginAudio::new(self.processor.clone(), sample_rate, max_frames, process_mode, &inputs, &outputs)
    }

    pub fn deactivate(&self) {
        if self.active.replace(false) {
            unsafe { self.component.setActive(0) };
        }
    }

    pub fn component_state(&self) -> Result<Vec<u8>, String> {
        let stream = ComWrapper::new(MemoryStream::new(Vec::new()));
        let pointer = stream.to_com_ptr::<IBStream>().ok_or("host object")?;
        check(unsafe { self.component.getState(pointer.as_ptr()) }, "getState")?;
        Ok(stream.take())
    }

    pub fn set_component_state(&self, state: Vec<u8>) -> Result<(), String> {
        let stream = ComWrapper::new(MemoryStream::new(state));
        let pointer = stream.to_com_ptr::<IBStream>().ok_or("host object")?;
        unsafe {
            check(self.component.setState(pointer.as_ptr()), "setState")?;
            if let Some(controller) = &self.controller {
                stream.rewind();
                controller.edit.setComponentState(pointer.as_ptr());
            }
        }
        Ok(())
    }

    pub fn controller_state(&self) -> Result<Vec<u8>, String> {
        let Some(controller) = &self.controller else { return Ok(Vec::new()) };
        let stream = ComWrapper::new(MemoryStream::new(Vec::new()));
        let pointer = stream.to_com_ptr::<IBStream>().ok_or("host object")?;
        let result = unsafe { controller.edit.getState(pointer.as_ptr()) };
        Ok(if result == kResultOk { stream.take() } else { Vec::new() })
    }

    pub fn set_controller_state(&self, state: Vec<u8>) -> Result<(), String> {
        let Some(controller) = &self.controller else { return Ok(()) };
        if state.is_empty() {
            return Ok(());
        }
        let stream = ComWrapper::new(MemoryStream::new(state));
        let pointer = stream.to_com_ptr::<IBStream>().ok_or("host object")?;
        unsafe { controller.edit.setState(pointer.as_ptr()) };
        Ok(())
    }

    pub fn parameters(&self) -> Vec<ParamInfo> {
        let Some(controller) = &self.controller else { return Vec::new() };
        unsafe {
            (0..controller.edit.getParameterCount())
                .filter_map(|index| {
                    let mut info: ParameterInfo = std::mem::zeroed();
                    (controller.edit.getParameterInfo(index, &mut info) == kResultOk).then(|| ParamInfo {
                        id: info.id,
                        title: from_utf16(&info.title),
                        short_title: from_utf16(&info.shortTitle),
                        units: from_utf16(&info.units),
                        step_count: info.stepCount,
                        default_value: info.defaultNormalizedValue,
                        unit_id: info.unitId,
                        flags: info.flags,
                    })
                })
                .collect()
        }
    }

    pub fn param_to_text(&self, id: u32, value: f64) -> Result<String, String> {
        let controller = self.controller.as_ref().ok_or("the plugin has no controller")?;
        let mut text: String128 = [0; 128];
        check(unsafe { controller.edit.getParamStringByValue(id, value, &mut text) }, "getParamStringByValue")?;
        Ok(from_utf16(&text))
    }

    pub fn text_to_param(&self, id: u32, text: &str) -> Result<f64, String> {
        let controller = self.controller.as_ref().ok_or("the plugin has no controller")?;
        let mut wide: Vec<TChar> = text.encode_utf16().chain(Some(0)).collect();
        let mut value = 0.0;
        check(unsafe { controller.edit.getParamValueByString(id, wide.as_mut_ptr(), &mut value) }, "getParamValueByString")?;
        Ok(value)
    }

    pub fn normalized_to_plain(&self, id: u32, value: f64) -> f64 {
        self.controller.as_ref().map_or(value, |c| unsafe { c.edit.normalizedParamToPlain(id, value) })
    }

    pub fn plain_to_normalized(&self, id: u32, value: f64) -> f64 {
        self.controller.as_ref().map_or(value, |c| unsafe { c.edit.plainParamToNormalized(id, value) })
    }

    pub fn param(&self, id: u32) -> f64 {
        self.controller.as_ref().map_or(0.0, |c| unsafe { c.edit.getParamNormalized(id) })
    }

    pub fn set_param(&self, id: u32, value: f64) {
        if let Some(controller) = &self.controller {
            unsafe { controller.edit.setParamNormalized(id, value) };
        }
    }

    pub fn create_editor(&self) -> Result<Editor, String> {
        let controller = self.controller.as_ref().ok_or("the plugin has no controller")?;
        let window = Arc::new(std::sync::atomic::AtomicIsize::new(0));
        let frame = ComWrapper::new(PlugFrame { sink: self.sink.clone(), window: Arc::clone(&window) });
        let frame = frame.to_com_ptr::<IPlugFrame>().ok_or("host object")?;
        Editor::create(&controller.edit, frame, window)
    }

    fn configure(&mut self) -> Result<(), String> {
        unsafe {
            check(self.component.initialize(self.host_context.as_ptr().cast::<FUnknown>()), "initialize")?;
            self.initialized = true;

            if self.processor.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as int32) != kResultOk {
                return Err("the plugin cannot process 32-bit samples".into());
            }
            self.controller = self.create_controller();
            if let Some(controller) = &self.controller {
                controller.edit.setComponentHandler(self.handler.as_ptr());
            }
        }
        Ok(())
    }

    unsafe fn bus_arrangements(&self, direction: BusDirection) -> Result<Vec<SpeakerArrangement>, String> {
        unsafe {
            let count = self.component.getBusCount(AUDIO, direction).max(0);
            let mut arrangements = vec![0; count as usize];
            for (index, arrangement) in arrangements.iter_mut().enumerate() {
                check(self.processor.getBusArrangement(direction, index as int32, arrangement), "getBusArrangement")?;
            }
            Ok(arrangements)
        }
    }

    unsafe fn create_controller(&self) -> Option<Controller> {
        if let Some(edit) = self.component.cast::<IEditController>() {
            return Some(Controller { edit, separate: false, links: None });
        }
        unsafe {
            let mut class_id: TUID = [0; 16];
            if self.component.getControllerClassId(&mut class_id) != kResultOk {
                return None;
            }
            let mut object = ptr::null_mut();
            let iid = IEditController::IID;
            let created = self.module.factory().createInstance(class_id.as_ptr(), iid.as_ptr().cast(), &mut object);
            if created != kResultOk {
                return None;
            }
            let edit = ComPtr::from_raw(object.cast::<IEditController>())?;
            if edit.initialize(self.host_context.as_ptr().cast::<FUnknown>()) != kResultOk {
                return None;
            }
            let links = match (self.component.cast::<IConnectionPoint>(), edit.cast::<IConnectionPoint>()) {
                (Some(a), Some(b)) => {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                    Some((a, b))
                }
                _ => None,
            };
            let controller = Controller { edit, separate: true, links };
            if let Ok(state) = self.component_state() {
                let stream = ComWrapper::new(MemoryStream::new(state));
                if let Some(pointer) = stream.to_com_ptr::<IBStream>() {
                    controller.edit.setComponentState(pointer.as_ptr());
                }
            }
            Some(controller)
        }
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        self.deactivate();
        unsafe {
            if let Some(controller) = self.controller.take() {
                controller.edit.setComponentHandler(ptr::null_mut());
                if let Some((a, b)) = &controller.links {
                    a.disconnect(b.as_ptr());
                    b.disconnect(a.as_ptr());
                }
                if controller.separate {
                    controller.edit.terminate();
                }
            }
            if self.initialized {
                self.component.terminate();
            }
        }
    }
}

pub struct PluginAudio {
    processor: ComPtr<IAudioProcessor>,
    input_changes: ComWrapper<ChangeList>,
    input_changes_pointer: ComPtr<IParameterChanges>,
    output_changes: ComWrapper<ChangeList>,
    output_changes_pointer: ComPtr<IParameterChanges>,
    input_events: ComWrapper<EventList>,
    input_events_pointer: ComPtr<IEventList>,
    output_events: ComWrapper<EventList>,
    output_events_pointer: ComPtr<IEventList>,
    sample_rate: f64,
    max_frames: u32,
    process_mode: i32,
    processing: bool,
    input_buses: Vec<AudioBusBuffers>,
    output_buses: Vec<AudioBusBuffers>,
    input_channels: Vec<*mut f32>,
    output_channels: Vec<*mut f32>,
    _scratch: Vec<f32>,
    context: ProcessContext,
}

unsafe impl Send for PluginAudio {}

impl PluginAudio {
    fn new(
        processor: ComPtr<IAudioProcessor>,
        sample_rate: f64,
        max_frames: u32,
        process_mode: i32,
        inputs: &[SpeakerArrangement],
        outputs: &[SpeakerArrangement],
    ) -> Result<Self, String> {
        let frames = max_frames as usize;
        let total_channels: usize = inputs.iter().chain(outputs).map(|a| a.count_ones() as usize).sum();
        let mut scratch = vec![0.0f32; total_channels * frames];
        let base = scratch.as_mut_ptr();
        let mut used = 0;
        let mut assign = |arrangements: &[SpeakerArrangement]| {
            let mut channels = Vec::new();
            for arrangement in arrangements {
                for _ in 0..arrangement.count_ones() {
                    channels.push(unsafe { base.add(used * frames) });
                    used += 1;
                }
            }
            channels
        };
        let mut input_channels = assign(inputs);
        let mut output_channels = assign(outputs);
        let input_buses = make_buses(inputs, &mut input_channels);
        let output_buses = make_buses(outputs, &mut output_channels);

        let mut context: ProcessContext = unsafe { std::mem::zeroed() };
        context.state = StatesAndFlags_::kPlaying as u32
            | StatesAndFlags_::kTempoValid as u32
            | StatesAndFlags_::kTimeSigValid as u32
            | StatesAndFlags_::kContTimeValid as u32
            | StatesAndFlags_::kProjectTimeMusicValid as u32;
        context.sampleRate = sample_rate;
        context.tempo = TEMPO;
        context.timeSigNumerator = 4;
        context.timeSigDenominator = 4;

        let input_changes = ComWrapper::new(ChangeList::new());
        let output_changes = ComWrapper::new(ChangeList::new());
        let input_events = ComWrapper::new(EventList::new());
        let output_events = ComWrapper::new(EventList::new());
        Ok(Self {
            processor,
            input_changes_pointer: input_changes.to_com_ptr().ok_or("host object")?,
            input_changes,
            output_changes_pointer: output_changes.to_com_ptr().ok_or("host object")?,
            output_changes,
            input_events_pointer: input_events.to_com_ptr().ok_or("host object")?,
            input_events,
            output_events_pointer: output_events.to_com_ptr().ok_or("host object")?,
            output_events,
            sample_rate,
            max_frames,
            process_mode,
            processing: false,
            input_buses,
            output_buses,
            input_channels,
            output_channels,
            _scratch: scratch,
            context,
        })
    }

    pub fn set_input_events(&mut self, events: impl IntoIterator<Item = Event>) {
        self.input_events.fill(events);
    }

    pub fn output_events(&self, out: &mut Vec<Event>) {
        self.output_events.collect(out);
    }

    pub fn channel_counts(&self) -> (usize, usize) {
        (self.input_channels.len(), self.output_channels.len())
    }

    pub unsafe fn process_stereo(
        &mut self,
        inputs: [*mut f32; 2],
        outputs: [*mut f32; 2],
        frames: u32,
        context: Option<&ProcessContext>,
        input_params: &[ParamPoint],
        output_params: &mut Vec<ParamPoint>,
    ) -> bool {
        if !self.input_buses.is_empty() {
            self.input_channels[..2].copy_from_slice(&inputs);
        }
        self.output_channels[..2].copy_from_slice(&outputs);
        unsafe { self.run(frames, context, input_params, output_params) }
    }

    #[allow(clippy::too_many_arguments)]
    pub unsafe fn process_all(
        &mut self,
        inputs: &[*mut f32],
        outputs: &[*mut f32],
        frames: u32,
        context: Option<&ProcessContext>,
        input_params: &[ParamPoint],
        output_params: &mut Vec<ParamPoint>,
        input_silence: &[u64],
        output_silence: &mut [u64],
    ) -> bool {
        assert!(inputs.len() == self.input_channels.len() && outputs.len() == self.output_channels.len());
        self.input_channels.copy_from_slice(inputs);
        self.output_channels.copy_from_slice(outputs);
        for (bus, flags) in self.input_buses.iter_mut().zip(input_silence) {
            bus.silenceFlags = *flags;
        }
        let result = unsafe { self.run(frames, context, input_params, output_params) };
        for (bus, flags) in self.output_buses.iter().zip(output_silence) {
            *flags = bus.silenceFlags;
        }
        result
    }

    unsafe fn run(
        &mut self,
        frames: u32,
        context: Option<&ProcessContext>,
        input_params: &[ParamPoint],
        output_params: &mut Vec<ParamPoint>,
    ) -> bool {
        assert!(frames <= self.max_frames);
        if let Some(context) = context {
            self.context = *context;
        }
        self.input_changes.fill(input_params);
        self.output_changes.clear();
        self.output_events.clear();
        unsafe {
            if !self.processing {
                self.processor.setProcessing(1);
                self.processing = true;
            }

            let mut data = ProcessData {
                processMode: self.process_mode,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
                numSamples: frames as int32,
                numInputs: self.input_buses.len() as int32,
                numOutputs: self.output_buses.len() as int32,
                inputs: self.input_buses.as_mut_ptr(),
                outputs: self.output_buses.as_mut_ptr(),
                inputParameterChanges: self.input_changes_pointer.as_ptr(),
                outputParameterChanges: self.output_changes_pointer.as_ptr(),
                inputEvents: self.input_events_pointer.as_ptr(),
                outputEvents: self.output_events_pointer.as_ptr(),
                processContext: &mut self.context,
            };
            let result = self.processor.process(&mut data);
            self.output_changes.collect(output_params);
            self.input_events.clear();

            if context.is_none() {
                self.context.projectTimeSamples += i64::from(frames);
                self.context.continousTimeSamples = self.context.projectTimeSamples;
                self.context.projectTimeMusic =
                    self.context.projectTimeSamples as f64 / self.sample_rate * TEMPO / 60.0;
            }
            result == kResultOk
        }
    }
}

impl Drop for PluginAudio {
    fn drop(&mut self) {
        if self.processing {
            unsafe { self.processor.setProcessing(0) };
        }
    }
}

fn from_utf16(text: &[TChar]) -> String {
    let length = text.iter().position(|unit| *unit == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..length])
}

fn make_buses(arrangements: &[SpeakerArrangement], channels: &mut [*mut f32]) -> Vec<AudioBusBuffers> {
    let mut offset = 0;
    arrangements
        .iter()
        .enumerate()
        .map(|(index, arrangement)| {
            let count = arrangement.count_ones() as usize;
            let first = unsafe { channels.as_mut_ptr().add(offset) };
            offset += count;
            AudioBusBuffers {
                numChannels: count as int32,
                silenceFlags: if index == 0 { 0 } else { (1u64 << count.min(63)) - 1 },
                __field0: AudioBusBuffers__type0 { channelBuffers32: first },
            }
        })
        .collect()
}

fn create_component(factory: &ComPtr<IPluginFactory>) -> Result<ComPtr<IComponent>, String> {
    unsafe {
        for index in 0..factory.countClasses() {
            let mut class: PClassInfo = std::mem::zeroed();
            if factory.getClassInfo(index, &mut class) != kResultOk {
                continue;
            }
            let category: Vec<u8> = class.category.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
            if category != AUDIO_MODULE_CLASS {
                continue;
            }
            let mut object = ptr::null_mut();
            let iid = IComponent::IID;
            check(factory.createInstance(class.cid.as_ptr(), iid.as_ptr().cast(), &mut object), "createInstance")?;
            return ComPtr::from_raw(object.cast::<IComponent>()).ok_or_else(|| "createInstance returned null".into());
        }
    }
    Err("the module has no audio class".into())
}

fn check(result: tresult, what: &str) -> Result<(), String> {
    if result == kResultOk { Ok(()) } else { Err(format!("{what} failed: {result:#x}")) }
}

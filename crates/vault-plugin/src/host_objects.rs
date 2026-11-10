use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::Arc;

use vault_protocol::ParamPoint;
use vault_protocol::control::Event as PluginEvent;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

const HOST_NAME: &str = "PluginVault";
const PREALLOCATED_QUEUES: usize = 64;
const PREALLOCATED_POINTS: usize = 16;

pub type EventSink = Arc<dyn Fn(PluginEvent) + Send + Sync>;

pub struct HostContext;

impl Class for HostContext {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostContext {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        let name = unsafe { &mut *name };
        let mut length = 0;
        for unit in HOST_NAME.encode_utf16() {
            name[length] = unit as TChar;
            length += 1;
        }
        name[length] = 0;
        kResultOk
    }

    unsafe fn createInstance(&self, _cid: *mut TUID, _iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        unsafe { *obj = std::ptr::null_mut() };
        kResultFalse
    }
}

pub struct ComponentHandler {
    pub sink: EventSink,
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        (self.sink)(PluginEvent::BeginEdit { id });
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        (self.sink)(PluginEvent::PerformEdit { id, value });
        kResultOk
    }

    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        (self.sink)(PluginEvent::EndEdit { id });
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        (self.sink)(PluginEvent::Restart { flags });
        kResultOk
    }
}

pub struct PlugFrame {
    pub sink: EventSink,
    pub window: Arc<std::sync::atomic::AtomicIsize>,
}

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        let size = unsafe { &*size };
        let (width, height) = (size.right - size.left, size.bottom - size.top);
        let window = self.window.load(std::sync::atomic::Ordering::Relaxed);
        if window != 0 {
            unsafe { PostMessageW(window as HWND, crate::editor::WM_RESIZE_TO, width as usize, height as isize) };
        }
        (self.sink)(PluginEvent::ResizeView { width, height });
        kResultOk
    }
}

pub struct MemoryStream {
    data: RefCell<Vec<u8>>,
    position: Cell<usize>,
}

impl MemoryStream {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data: RefCell::new(data), position: Cell::new(0) }
    }

    pub fn rewind(&self) {
        self.position.set(0);
    }

    pub fn take(&self) -> Vec<u8> {
        self.data.take()
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, count: int32, read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let position = self.position.get().min(data.len());
        let length = usize::try_from(count).unwrap_or(0).min(data.len() - position);
        unsafe {
            std::ptr::copy_nonoverlapping(data[position..].as_ptr(), buffer.cast::<u8>(), length);
            if !read.is_null() {
                *read = length as int32;
            }
        }
        self.position.set(position + length);
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, count: int32, written: *mut int32) -> tresult {
        let length = usize::try_from(count).unwrap_or(0);
        let mut data = self.data.borrow_mut();
        let position = self.position.get();
        if data.len() < position + length {
            data.resize(position + length, 0);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(buffer.cast::<u8>(), data[position..].as_mut_ptr(), length);
            if !written.is_null() {
                *written = length as int32;
            }
        }
        self.position.set(position + length);
        kResultOk
    }

    unsafe fn seek(&self, position: int64, mode: int32, result: *mut int64) -> tresult {
        let base = match mode as IBStream_::IStreamSeekMode {
            IBStream_::IStreamSeekMode_::kIBSeekSet => 0,
            IBStream_::IStreamSeekMode_::kIBSeekCur => self.position.get() as i64,
            IBStream_::IStreamSeekMode_::kIBSeekEnd => self.data.borrow().len() as i64,
            _ => return kInvalidArgument,
        };
        let Some(target) = base.checked_add(position).filter(|t| *t >= 0) else { return kInvalidArgument };
        self.position.set(target as usize);
        if !result.is_null() {
            unsafe { *result = target };
        }
        kResultOk
    }

    unsafe fn tell(&self, position: *mut int64) -> tresult {
        if position.is_null() {
            return kInvalidArgument;
        }
        unsafe { *position = self.position.get() as i64 };
        kResultOk
    }
}

pub struct ChangeQueue {
    id: Cell<ParamID>,
    points: RefCell<Vec<(int32, ParamValue)>>,
}

impl Class for ChangeQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ChangeQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.points.borrow().len() as int32
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        let points = self.points.borrow();
        let Some(&(point_offset, point_value)) = usize::try_from(index).ok().and_then(|i| points.get(i)) else {
            return kInvalidArgument;
        };
        unsafe {
            *offset = point_offset;
            *value = point_value;
        }
        kResultOk
    }

    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        let mut points = self.points.borrow_mut();
        if !index.is_null() {
            unsafe { *index = points.len() as int32 };
        }
        points.push((offset, value));
        kResultOk
    }
}

pub struct ChangeList {
    queues: RefCell<Vec<(ComWrapper<ChangeQueue>, ComPtr<IParamValueQueue>)>>,
    used: Cell<usize>,
}

impl ChangeList {
    pub fn new() -> Self {
        let list = Self { queues: RefCell::new(Vec::new()), used: Cell::new(0) };
        for _ in 0..PREALLOCATED_QUEUES {
            list.grow();
        }
        list
    }

    fn grow(&self) -> Option<()> {
        let queue = ComWrapper::new(ChangeQueue {
            id: Cell::new(0),
            points: RefCell::new(Vec::with_capacity(PREALLOCATED_POINTS)),
        });
        let pointer = queue.to_com_ptr::<IParamValueQueue>()?;
        self.queues.borrow_mut().push((queue, pointer));
        Some(())
    }

    pub fn clear(&self) {
        for (queue, _) in self.queues.borrow().iter().take(self.used.get()) {
            queue.points.borrow_mut().clear();
        }
        self.used.set(0);
    }

    fn queue_for(&self, id: ParamID) -> Option<usize> {
        let queues = self.queues.borrow();
        let used = self.used.get();
        if let Some(index) = queues.iter().take(used).position(|(queue, _)| queue.id.get() == id) {
            return Some(index);
        }
        let have_room = used < queues.len();
        drop(queues);
        if !have_room {
            self.grow()?;
        }
        self.queues.borrow()[used].0.id.set(id);
        self.used.set(used + 1);
        Some(used)
    }

    pub fn fill(&self, points: &[ParamPoint]) {
        self.clear();
        for point in points {
            if let Some(index) = self.queue_for(point.id) {
                self.queues.borrow()[index].0.points.borrow_mut().push((point.sample_offset, point.value));
            }
        }
    }

    pub fn collect(&self, out: &mut Vec<ParamPoint>) {
        out.clear();
        for (queue, _) in self.queues.borrow().iter().take(self.used.get()) {
            let id = queue.id.get();
            for &(sample_offset, value) in queue.points.borrow().iter() {
                out.push(ParamPoint { id, sample_offset, value });
            }
        }
    }
}

impl Class for ChangeList {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ChangeList {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        let queues = self.queues.borrow();
        match usize::try_from(index).ok().filter(|i| *i < self.used.get()).and_then(|i| queues.get(i)) {
            Some((_, pointer)) => pointer.as_ptr(),
            None => std::ptr::null_mut(),
        }
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        let Some(found) = self.queue_for(unsafe { *id }) else { return std::ptr::null_mut() };
        if !index.is_null() {
            unsafe { *index = found as int32 };
        }
        self.queues.borrow()[found].1.as_ptr()
    }
}

pub struct EventList {
    events: RefCell<Vec<Event>>,
}

impl EventList {
    pub fn new() -> Self {
        Self { events: RefCell::new(Vec::with_capacity(vault_protocol::MAX_EVENTS)) }
    }

    pub fn fill(&self, events: impl IntoIterator<Item = Event>) {
        let mut list = self.events.borrow_mut();
        list.clear();
        let room = list.capacity();
        list.extend(events.into_iter().take(room));
    }

    pub fn clear(&self) {
        self.events.borrow_mut().clear();
    }

    pub fn collect(&self, out: &mut Vec<Event>) {
        out.clear();
        out.extend_from_slice(&self.events.borrow());
    }
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        self.events.borrow().len() as int32
    }

    unsafe fn getEvent(&self, index: int32, event: *mut Event) -> tresult {
        let events = self.events.borrow();
        match usize::try_from(index).ok().and_then(|index| events.get(index)) {
            Some(found) if !event.is_null() => {
                unsafe { *event = *found };
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn addEvent(&self, event: *mut Event) -> tresult {
        let mut events = self.events.borrow_mut();
        if event.is_null() || events.len() == events.capacity() {
            return kResultFalse;
        }
        events.push(unsafe { *event });
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(id: u32, sample_offset: i32, value: f64) -> ParamPoint {
        ParamPoint { id, sample_offset, value }
    }

    fn call_add(list: &ChangeList, id: ParamID) -> *mut IParamValueQueue {
        let mut index = -1;
        unsafe { list.addParameterData(&id, &mut index) }
    }

    #[test]
    fn points_are_grouped_by_parameter_and_come_back_unchanged() {
        let list = ChangeList::new();
        let points = [point(1, 0, 0.1), point(2, 0, 0.2), point(1, 64, 0.3)];
        list.fill(&points);
        assert_eq!(unsafe { list.getParameterCount() }, 2);

        let mut out = Vec::new();
        list.collect(&mut out);
        assert_eq!(out, [point(1, 0, 0.1), point(1, 64, 0.3), point(2, 0, 0.2)]);
    }

    #[test]
    fn a_new_block_does_not_keep_old_points() {
        let list = ChangeList::new();
        list.fill(&[point(1, 0, 0.1)]);
        list.fill(&[point(2, 0, 0.2)]);
        let mut out = Vec::new();
        list.collect(&mut out);
        assert_eq!(out, [point(2, 0, 0.2)]);
    }

    #[test]
    fn more_parameters_than_preallocated_still_work() {
        let list = ChangeList::new();
        let points: Vec<_> = (0..PREALLOCATED_QUEUES as u32 + 5).map(|id| point(id, 0, 0.5)).collect();
        list.fill(&points);
        let mut out = Vec::new();
        list.collect(&mut out);
        assert_eq!(out, points);
    }

    #[test]
    fn plugin_side_calls_see_the_same_data() {
        let list = ChangeList::new();
        let queue = call_add(&list, 42);
        assert!(!queue.is_null());
        assert_eq!(call_add(&list, 42), queue, "the same parameter must reuse its queue");
        unsafe {
            let queue = vst3::ComRef::from_raw(queue).unwrap();
            let mut index = -1;
            assert_eq!(queue.addPoint(10, 0.75, &mut index), kResultOk);
            assert_eq!(index, 0);
            let (mut offset, mut value) = (0, 0.0);
            assert_eq!(queue.getPoint(0, &mut offset, &mut value), kResultOk);
            assert_eq!((offset, value), (10, 0.75));
            assert_eq!(queue.getPoint(1, &mut offset, &mut value), kInvalidArgument);
            assert_eq!(queue.getPoint(-1, &mut offset, &mut value), kInvalidArgument);
            assert!(list.getParameterData(99).is_null());
            assert!(list.getParameterData(-1).is_null());
        }
    }

    #[test]
    fn memory_stream_reads_what_was_written_and_stops_at_the_end() {
        let stream = MemoryStream::new(Vec::new());
        let mut bytes = [1u8, 2, 3];
        let (mut written, mut read) = (0, 0);
        unsafe {
            stream.write(bytes.as_mut_ptr().cast(), 3, &mut written);
            assert_eq!(written, 3);
            stream.rewind();
            let mut target = [0u8; 8];
            stream.read(target.as_mut_ptr().cast(), 8, &mut read);
            assert_eq!(read, 3);
            assert_eq!(target[..3], [1, 2, 3]);
            stream.read(target.as_mut_ptr().cast(), 8, &mut read);
            assert_eq!(read, 0);
        }
    }

    #[test]
    fn memory_stream_refuses_a_seek_before_the_start() {
        let stream = MemoryStream::new(vec![0; 4]);
        let mode = IBStream_::IStreamSeekMode_::kIBSeekCur as int32;
        let mut result = 0;
        unsafe {
            assert_eq!(stream.seek(-1, mode, &mut result), kInvalidArgument);
            assert_eq!(stream.seek(2, mode, &mut result), kResultOk);
            assert_eq!(result, 2);
        }
    }
}

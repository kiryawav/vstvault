use std::collections::VecDeque;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use vault_protocol::control::Event;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HWND_MESSAGE, PostMessageW,
    RegisterClassExW, SetWindowLongPtrW, WM_APP, WNDCLASSEXW,
};

use crate::instance::Inner;

#[derive(Default)]
pub struct EventQueue {
    events: Mutex<VecDeque<Event>>,
    window: AtomicPtr<core::ffi::c_void>,
}

impl EventQueue {
    pub fn push(&self, event: Event) {
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
        let window = self.window.load(Ordering::Acquire);
        if !window.is_null() {
            unsafe { PostMessageW(window, WM_APP, 0, 0) };
        }
    }

    pub fn next(&self) -> Option<Event> {
        self.events.lock().ok()?.pop_front()
    }
}

fn own_module() -> *mut core::ffi::c_void {
    let mut module = std::ptr::null_mut();
    let address = own_module as *const () as *const u16;
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    unsafe { GetModuleHandleExW(flags, address, &mut module) };
    module
}

fn window_class() -> *const u16 {
    static CLASS: OnceLock<Vec<u16>> = OnceLock::new();
    CLASS
        .get_or_init(|| {
            let name: Vec<u16> = "VaultBridgeDispatch".encode_utf16().chain(Some(0)).collect();
            unsafe {
                let mut class: WNDCLASSEXW = std::mem::zeroed();
                class.cbSize = size_of::<WNDCLASSEXW>() as u32;
                class.lpfnWndProc = Some(window_proc);
                class.hInstance = own_module();
                class.lpszClassName = name.as_ptr();
                RegisterClassExW(&class);
            }
            name
        })
        .as_ptr()
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message != WM_APP {
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    }
    let inner = unsafe {
        let target = GetWindowLongPtrW(window, GWLP_USERDATA) as *const Weak<Inner>;
        target.as_ref().and_then(Weak::upgrade)
    };
    if let Some(inner) = inner {
        inner.drain_events();
    }
    0
}

pub struct Dispatcher {
    window: HWND,
    queue: Arc<EventQueue>,
    _target: Box<Weak<Inner>>,
}

impl Dispatcher {
    pub fn new(inner: &Arc<Inner>, queue: Arc<EventQueue>) -> Result<Self, String> {
        let target = Box::new(Arc::downgrade(inner));
        unsafe {
            let window = CreateWindowExW(
                0,
                window_class(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                own_module(),
                std::ptr::null(),
            );
            if window.is_null() {
                return Err(format!("cannot create the dispatch window: {}", std::io::Error::last_os_error()));
            }
            SetWindowLongPtrW(window, GWLP_USERDATA, std::ptr::from_ref::<Weak<Inner>>(&target) as isize);
            queue.window.store(window, Ordering::Release);
            Ok(Self { window, queue, _target: target })
        }
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.queue.window.store(std::ptr::null_mut(), Ordering::Release);
        unsafe { DestroyWindow(self.window) };
    }
}

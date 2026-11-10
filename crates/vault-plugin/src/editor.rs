use std::cell::Cell;
use std::ptr;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, OnceLock};

use vault_protocol::control::EditorInfo;
use vst3::ComPtr;
use vst3::Steinberg::Vst::{IEditController, IEditControllerTrait};
use vst3::Steinberg::*;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetWindowDpiAwarenessContext, SetThreadDpiAwarenessContext};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GUITHREADINFO, GWLP_USERDATA, GetClassNameW, GetGUIThreadInfo, GetParent,
    GetWindowLongPtrW, IsChild, MSG, PM_NOREMOVE, PeekMessageW, PostMessageW, RegisterClassExW, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WM_APP, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN, WS_VISIBLE,
};

use crate::foreground;

const EDITOR_VIEW: &[u8] = b"editor\0";
pub(crate) const WM_RESIZE_TO: u32 = WM_APP + 1;
const PLATFORM_HWND: &[u8] = b"HWND\0";

fn check(result: tresult, what: &str) -> Result<(), String> {
    if result == kResultOk { Ok(()) } else { Err(format!("{what} failed: {result:#x}")) }
}

fn window_class() -> *const u16 {
    static CLASS: OnceLock<Vec<u16>> = OnceLock::new();
    CLASS
        .get_or_init(|| {
            let name: Vec<u16> = "VaultEditorWindow".encode_utf16().chain(Some(0)).collect();
            unsafe {
                let mut class: WNDCLASSEXW = std::mem::zeroed();
                class.cbSize = size_of::<WNDCLASSEXW>() as u32;
                class.lpfnWndProc = Some(window_proc);
                class.hInstance = GetModuleHandleW(ptr::null());
                class.lpszClassName = name.as_ptr();
                RegisterClassExW(&class);
            }
            name
        })
        .as_ptr()
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_SIZE => {
            unsafe {
                let view = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut IPlugView;
                if let Some(view) = vst3::ComRef::from_raw(view) {
                    let mut rect = ViewRect {
                        left: 0,
                        top: 0,
                        right: (lparam & 0xFFFF) as i32,
                        bottom: ((lparam >> 16) & 0xFFFF) as i32,
                    };
                    view.onSize(&mut rect);
                }
            }
            0
        }
        WM_RESIZE_TO => {
            unsafe { SetWindowPos(window, ptr::null_mut(), 0, 0, wparam as i32, lparam as i32, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) };
            0
        }
        WM_ERASEBKGND => 1,
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            unsafe { PostMessageW(GetParent(window), message, wparam, lparam) };
            0
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe fn hands_keys_back(window: HWND) -> bool {
    let mut class = [0u16; 8];
    let length = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) } as usize;
    class[..length.min(5)].iter().copied().eq("JUCE_".encode_utf16())
}

pub struct Editor {
    view: ComPtr<IPlugView>,
    _frame: ComPtr<IPlugFrame>,
    window: Cell<HWND>,
    frame_window: Arc<AtomicIsize>,
}

impl Editor {
    pub(crate) fn create(controller: &ComPtr<IEditController>, frame: ComPtr<IPlugFrame>, frame_window: Arc<AtomicIsize>) -> Result<Self, String> {
        let view = unsafe {
            let view = controller.createView(EDITOR_VIEW.as_ptr().cast());
            ComPtr::from_raw(view).ok_or("the plugin has no editor")?
        };
        if unsafe { view.isPlatformTypeSupported(PLATFORM_HWND.as_ptr().cast()) } != kResultOk {
            return Err("the plugin's editor does not support Windows".into());
        }
        unsafe { view.setFrame(frame.as_ptr()) };
        Ok(Self { view, _frame: frame, window: Cell::new(ptr::null_mut()), frame_window })
    }

    pub fn info(&self) -> EditorInfo {
        let mut rect = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        let resizable = unsafe {
            self.view.getSize(&mut rect);
            self.view.canResize() == kResultOk
        };
        EditorInfo {
            window: self.window.get() as u64,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
            resizable,
        }
    }

    pub fn attach(&self, parent: u64) -> Result<EditorInfo, String> {
        if !self.window.get().is_null() {
            return Err("the editor is already attached".into());
        }
        let parent = parent as HWND;
        let size = self.info();
        unsafe {
            let context = GetWindowDpiAwarenessContext(parent);
            if !context.is_null() {
                SetThreadDpiAwarenessContext(context);
            }
            let window = CreateWindowExW(
                0,
                window_class(),
                ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
                0,
                0,
                size.width,
                size.height,
                parent,
                ptr::null_mut(),
                GetModuleHandleW(ptr::null()),
                ptr::null(),
            );
            if window.is_null() {
                return Err(format!("cannot create the editor window: {}", std::io::Error::last_os_error()));
            }
            self.window.set(window);
            foreground::editor_attached(window, parent);
            if let Err(message) = check(self.view.attached(window.cast(), PLATFORM_HWND.as_ptr().cast()), "attached") {
                foreground::editor_detached();
                self.window.set(ptr::null_mut());
                DestroyWindow(window);
                return Err(message);
            }
            let attached = self.info();
            SetWindowPos(window, ptr::null_mut(), 0, 0, attached.width, attached.height, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            SetWindowLongPtrW(window, GWLP_USERDATA, self.view.as_ptr() as isize);
            self.frame_window.store(window as isize, Ordering::Relaxed);
            Ok(attached)
        }
    }

    pub fn detach(&self) {
        let window = self.window.replace(ptr::null_mut());
        if window.is_null() {
            return;
        }
        self.frame_window.store(0, Ordering::Relaxed);
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            self.view.removed();
            foreground::editor_detached();
            DestroyWindow(window);
        }
    }

    pub fn pass_on_key(&self, message: &MSG) {
        let wrapper = self.window.get();
        if wrapper.is_null() || !matches!(message.message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP) {
            return;
        }
        unsafe {
            if message.hwnd == wrapper || IsChild(wrapper, message.hwnd) == 0 || hands_keys_back(message.hwnd) {
                return;
            }
            let mut info: GUITHREADINFO = std::mem::zeroed();
            info.cbSize = size_of::<GUITHREADINFO>() as u32;
            let typing = GetGUIThreadInfo(0, &mut info) != 0 && !info.hwndCaret.is_null();
            let mut handed_back: MSG = std::mem::zeroed();
            if typing || PeekMessageW(&mut handed_back, wrapper, message.message, message.message, PM_NOREMOVE) != 0 {
                return;
            }
            PostMessageW(GetParent(wrapper), message.message, message.wParam, message.lParam);
        }
    }

    pub fn check_size(&self, width: i32, height: i32) -> (i32, i32) {
        let mut rect = ViewRect { left: 0, top: 0, right: width, bottom: height };
        if unsafe { self.view.checkSizeConstraint(&mut rect) } == kResultOk {
            (rect.right - rect.left, rect.bottom - rect.top)
        } else {
            (width, height)
        }
    }

    pub fn resize(&self, width: i32, height: i32) {
        let window = self.window.get();
        if !window.is_null() {
            unsafe { SetWindowPos(window, ptr::null_mut(), 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) };
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        self.detach();
        unsafe { self.view.setFrame(ptr::null_mut()) };
    }
}

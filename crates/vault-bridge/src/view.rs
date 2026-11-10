use std::cell::Cell;
use std::ffi::CStr;
use std::sync::Arc;

use vault_protocol::control::EditorInfo;
use vst3::ComRef;
use vst3::Class;
use vst3::Steinberg::*;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos,
};

use crate::instance::Inner;
use crate::log_error;

pub struct BridgeView {
    pub inner: Arc<Inner>,
    pub this: Cell<*mut IPlugView>,
    pub info: Cell<EditorInfo>,
}

impl Class for BridgeView {
    type Interfaces = (IPlugView,);
}

impl Drop for BridgeView {
    fn drop(&mut self) {
        self.inner.forget_view(self.this.get());
        if let Err(message) = self.inner.with_session(|session| session.destroy_editor()) {
            log_error(&format!("closing the editor failed: {message}"));
        }
    }
}

fn is_hwnd(platform: FIDString) -> bool {
    if platform.is_null() {
        return false;
    }
    unsafe { CStr::from_ptr(platform) }.to_bytes() == b"HWND"
}

impl IPlugViewTrait for BridgeView {
    unsafe fn isPlatformTypeSupported(&self, platform: FIDString) -> tresult {
        if is_hwnd(platform) { kResultTrue } else { kResultFalse }
    }

    unsafe fn attached(&self, parent: *mut core::ffi::c_void, platform: FIDString) -> tresult {
        if !is_hwnd(platform) {
            return kResultFalse;
        }
        match self.inner.with_session(|session| session.attach_editor(parent as u64)) {
            Ok(info) => {
                self.info.set(info);
                self.inner.set_view_size(info.width, info.height);
                self.inner.remember_view(self.this.get());
                kResultOk
            }
            Err(message) => {
                log_error(&format!("opening the editor failed: {message}"));
                kResultFalse
            }
        }
    }

    unsafe fn removed(&self) -> tresult {
        let mut info = self.info.get();
        info.window = 0;
        self.info.set(info);
        self.inner.forget_view(self.this.get());
        if let Err(message) = self.inner.with_session(|session| session.detach_editor()) {
            log_error(&format!("detaching the editor failed: {message}"));
        }
        kResultOk
    }

    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyDown(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyUp(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }

    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        let (width, height) = self.inner.view_size();
        unsafe { *size = ViewRect { left: 0, top: 0, right: width, bottom: height } };
        kResultOk
    }

    unsafe fn onSize(&self, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        let size = unsafe { &*size };
        let (width, height) = (size.right - size.left, size.bottom - size.top);
        let window = self.info.get().window;
        if window == 0 {
            self.inner.set_view_size(width, height);
            return kResultOk;
        }
        match self.inner.with_session(|session| session.set_editor_size(width, height)) {
            Ok((taken_width, taken_height)) => {
                self.inner.set_view_size(taken_width, taken_height);
                if (taken_width, taken_height) != (width, height) {
                    self.inner.ask_for_view_size(taken_width, taken_height);
                }
            }
            Err(_) => {
                self.inner.set_view_size(width, height);
                let flags = SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
                unsafe { SetWindowPos(window as HWND, std::ptr::null_mut(), 0, 0, width, height, flags) };
            }
        }
        kResultOk
    }

    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
        self.inner.set_frame(unsafe { ComRef::from_raw(frame) }.map(|frame| frame.to_com_ptr()));
        kResultOk
    }

    unsafe fn canResize(&self) -> tresult {
        if self.info.get().resizable { kResultTrue } else { kResultFalse }
    }

    unsafe fn checkSizeConstraint(&self, rect: *mut ViewRect) -> tresult {
        if rect.is_null() {
            return kInvalidArgument;
        }
        let rect = unsafe { &mut *rect };
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        if let Ok((width, height)) = self.inner.with_session(|session| session.check_editor_size(width, height)) {
            rect.right = rect.left + width;
            rect.bottom = rect.top + height;
        }
        kResultOk
    }
}

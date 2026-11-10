use std::cell::Cell;
use std::ffi::c_void;

pub type Handle = *mut c_void;
pub type Status = i32;

pub const STATUS_SUCCESS: Status = 0;
pub const STATUS_OBJECT_NAME_NOT_FOUND: Status = 0xC000_0034_u32 as i32;
pub const STATUS_OBJECT_PATH_NOT_FOUND: Status = 0xC000_003A_u32 as i32;
pub const STATUS_OBJECT_NAME_COLLISION: Status = 0xC000_0035_u32 as i32;
pub const STATUS_NO_SUCH_FILE: Status = 0xC000_000F_u32 as i32;
pub const STATUS_NO_MORE_FILES: Status = 0x8000_0006_u32 as i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ObjectAttributes {
    pub length: u32,
    pub root_directory: Handle,
    pub object_name: *mut UnicodeString,
    pub attributes: u32,
    pub security_descriptor: *mut c_void,
    pub security_quality_of_service: *mut c_void,
}

pub struct OwnedName {
    wide: Vec<u16>,
    string: UnicodeString,
    attributes: ObjectAttributes,
}

impl OwnedName {
    pub fn new(path: &str, template: &ObjectAttributes, keep_security: bool) -> Box<Self> {
        let wide: Vec<u16> = path.encode_utf16().collect();
        let bytes = (wide.len() * 2) as u16;
        let mut name = Box::new(Self {
            wide,
            string: UnicodeString { length: bytes, maximum_length: bytes, buffer: std::ptr::null_mut() },
            attributes: *template,
        });
        name.string.buffer = name.wide.as_mut_ptr();
        name.attributes.root_directory = std::ptr::null_mut();
        name.attributes.object_name = &raw mut name.string;
        if !keep_security {
            name.attributes.security_descriptor = std::ptr::null_mut();
        }
        name
    }

    pub fn attributes(&mut self) -> *mut ObjectAttributes {
        &raw mut self.attributes
    }

    pub fn set_root(&mut self, root: Handle) {
        self.attributes.root_directory = root;
    }

    pub fn add_attributes(&mut self, flags: u32) {
        self.attributes.attributes |= flags;
    }
}

pub unsafe fn text_of(string: *const UnicodeString) -> Option<String> {
    unsafe {
        let string = string.as_ref()?;
        if string.buffer.is_null() {
            return None;
        }
        String::from_utf16(std::slice::from_raw_parts(string.buffer, usize::from(string.length) / 2)).ok()
    }
}

thread_local! {
    static INSIDE_HOOK: Cell<bool> = const { Cell::new(false) };
}

pub struct OwnWork;

impl OwnWork {
    pub fn enter() -> Option<Self> {
        INSIDE_HOOK.with(|inside| (!inside.replace(true)).then(|| Self))
    }
}

impl Drop for OwnWork {
    fn drop(&mut self) {
        INSIDE_HOOK.with(|inside| inside.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_entry_leaves_the_outer_one_in_force() {
        let outer = OwnWork::enter().expect("nothing is active yet");
        assert!(OwnWork::enter().is_none());
        assert!(OwnWork::enter().is_none(), "the refusal above must not have ended the outer one");
        drop(outer);
        assert!(OwnWork::enter().is_some());
    }
}

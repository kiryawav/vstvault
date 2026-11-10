use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualProtect,
};

#[cfg(target_arch = "x86_64")]
const STUB_START: &[u8] = &[0x4C, 0x8B, 0xD1, 0xB8];
#[cfg(target_arch = "x86_64")]
const STUB_BYTES: usize = 32;
#[cfg(target_arch = "x86")]
const STUB_START: &[u8] = &[0xB8];
#[cfg(target_arch = "x86")]
const STUB_BYTES: usize = 16;

const COPIES_PER_PAGE_BYTES: usize = 4096;

pub struct Patcher {
    ntdll: *mut c_void,
    copies: *mut u8,
    used: usize,
}

impl Patcher {
    pub fn new() -> Result<Self, String> {
        let name: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
        unsafe {
            let ntdll = GetModuleHandleW(name.as_ptr());
            let copies = VirtualAlloc(ptr::null(), COPIES_PER_PAGE_BYTES, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE).cast::<u8>();
            if ntdll.is_null() || copies.is_null() {
                return Err("cannot prepare to patch ntdll".into());
            }
            Ok(Self { ntdll, copies, used: 0 })
        }
    }

    pub fn hook(&mut self, name: &str, hook: usize) -> Result<usize, String> {
        let name_z = format!("{name}\0");
        let target = unsafe { GetProcAddress(self.ntdll, name_z.as_ptr()) }.ok_or_else(|| format!("ntdll has no {name}"))? as *mut u8;
        unsafe {
            if std::slice::from_raw_parts(target, STUB_START.len()) != STUB_START {
                return Err(format!("{name} is not a plain system call stub, so something else has patched it"));
            }
            if self.used + STUB_BYTES > COPIES_PER_PAGE_BYTES {
                return Err("too many hooks".into());
            }
            let copy = self.copies.add(self.used);
            ptr::copy_nonoverlapping(target, copy, STUB_BYTES);
            self.used += STUB_BYTES;

            let patch = jump_to(target as usize, hook);
            let mut protection = 0;
            if VirtualProtect(target.cast(), patch.len(), PAGE_EXECUTE_READWRITE, &mut protection) == 0 {
                return Err(format!("cannot patch {name}: {}", std::io::Error::last_os_error()));
            }
            ptr::copy_nonoverlapping(patch.as_ptr(), target, patch.len());
            VirtualProtect(target.cast(), patch.len(), protection, &mut protection);
            Ok(copy as usize)
        }
    }

    pub fn replace(&mut self, module: &str, name: &str, replacement: usize) -> Result<(), String> {
        let module_z: Vec<u16> = module.encode_utf16().chain(Some(0)).collect();
        let name_z = format!("{name}\0");
        unsafe {
            let handle = GetModuleHandleW(module_z.as_ptr());
            if handle.is_null() {
                return Err(format!("{module} is not loaded"));
            }
            let target = GetProcAddress(handle, name_z.as_ptr()).ok_or_else(|| format!("{module} has no {name}"))? as *mut u8;
            let patch = jump_to(target as usize, replacement);
            let mut protection = 0;
            if VirtualProtect(target.cast(), patch.len(), PAGE_EXECUTE_READWRITE, &mut protection) == 0 {
                return Err(format!("cannot patch {name}: {}", std::io::Error::last_os_error()));
            }
            ptr::copy_nonoverlapping(patch.as_ptr(), target, patch.len());
            VirtualProtect(target.cast(), patch.len(), protection, &mut protection);
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), String> {
        let mut protection = 0;
        if unsafe { VirtualProtect(self.copies.cast(), COPIES_PER_PAGE_BYTES, PAGE_EXECUTE_READ, &mut protection) } == 0 {
            return Err(format!("cannot protect the original calls: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
fn jump_to(_from: usize, to: usize) -> Vec<u8> {
    let mut patch = vec![0xFF, 0x25, 0, 0, 0, 0];
    patch.extend_from_slice(&(to as u64).to_le_bytes());
    patch
}

#[cfg(target_arch = "x86")]
fn jump_to(from: usize, to: usize) -> Vec<u8> {
    let mut patch = vec![0xE9];
    patch.extend_from_slice(&(to.wrapping_sub(from + 5) as u32).to_le_bytes());
    patch
}

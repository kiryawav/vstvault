use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{HMODULE, HWND};
use windows_sys::Win32::System::Diagnostics::Debug::{IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_NT_HEADERS64};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::SystemServices::{IMAGE_DOS_HEADER, IMAGE_IMPORT_DESCRIPTOR};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

static EMBEDDED_WINDOW: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
static DAW_PROCESS: AtomicU32 = AtomicU32::new(0);

fn process_of(window: HWND) -> u32 {
    let mut process = 0;
    unsafe { GetWindowThreadProcessId(window, &mut process) };
    process
}

unsafe extern "system" fn foreground_window() -> HWND {
    let foreground = unsafe { GetForegroundWindow() };
    let embedded = EMBEDDED_WINDOW.load(Ordering::Acquire);
    if !embedded.is_null() && process_of(foreground) == DAW_PROCESS.load(Ordering::Relaxed) {
        return embedded;
    }
    foreground
}

pub(crate) fn redirect_imports(module: HMODULE) {
    unsafe {
        let user32: Vec<u16> = "user32.dll\0".encode_utf16().collect();
        let Some(real) = GetProcAddress(GetModuleHandleW(user32.as_ptr()), c"GetForegroundWindow".as_ptr().cast()) else {
            return;
        };
        replace_import(module as *const u8, real as usize, foreground_window as *const () as usize);
    }
}

pub(crate) fn editor_attached(window: HWND, parent: HWND) {
    let daw = process_of(parent);
    if daw != 0 && daw != unsafe { GetCurrentProcessId() } {
        DAW_PROCESS.store(daw, Ordering::Relaxed);
        EMBEDDED_WINDOW.store(window, Ordering::Release);
    }
}

pub(crate) fn editor_detached() {
    EMBEDDED_WINDOW.store(ptr::null_mut(), Ordering::Release);
}

unsafe fn replace_import(base: *const u8, old: usize, new: usize) -> usize {
    let mut replaced = 0;
    unsafe {
        let nt_at = (*base.cast::<IMAGE_DOS_HEADER>()).e_lfanew as usize;
        let nt = base.add(nt_at).cast::<IMAGE_NT_HEADERS64>();
        let imports = (*nt).OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT as usize];
        if imports.VirtualAddress == 0 {
            return 0;
        }
        let mut descriptor = base.add(imports.VirtualAddress as usize).cast::<IMAGE_IMPORT_DESCRIPTOR>();
        while (*descriptor).Name != 0 {
            let mut slot = base.add((*descriptor).FirstThunk as usize).cast::<usize>().cast_mut();
            while *slot != 0 {
                if *slot == old {
                    let mut protection = 0;
                    if VirtualProtect(slot.cast(), size_of::<usize>(), PAGE_READWRITE, &mut protection) != 0 {
                        slot.write_volatile(new);
                        VirtualProtect(slot.cast(), size_of::<usize>(), protection, &mut protection);
                        replaced += 1;
                    }
                }
                slot = slot.add(1);
            }
            descriptor = descriptor.add(1);
        }
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: usize = 0x1111;
    const NEW: usize = 0x2222;
    const OTHER: usize = 0x3333;

    fn image(first: &[usize], second: &[usize]) -> Vec<u64> {
        let mut image = vec![0u64; 0x2000 / 8];
        let base = image.as_mut_ptr().cast::<u8>();
        unsafe {
            (*base.cast::<IMAGE_DOS_HEADER>()).e_lfanew = 0x40;
            let nt = base.add(0x40).cast::<IMAGE_NT_HEADERS64>();
            (*nt).OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT as usize].VirtualAddress = 0x400;
            let descriptors = base.add(0x400).cast::<IMAGE_IMPORT_DESCRIPTOR>();
            for (index, (slots, at)) in [(first, 0x800usize), (second, 0xC00)].into_iter().enumerate() {
                (*descriptors.add(index)).Name = 1;
                (*descriptors.add(index)).FirstThunk = at as u32;
                for (i, slot) in slots.iter().enumerate() {
                    base.add(at).cast::<usize>().add(i).write(*slot);
                }
            }
        }
        image
    }

    fn slots(image: &[u64], at: usize, count: usize) -> Vec<usize> {
        image[at / 8..at / 8 + count].iter().map(|slot| *slot as usize).collect()
    }

    #[test]
    fn replaces_the_function_in_every_imported_dll_and_nothing_else() {
        let image = image(&[OTHER, OLD, OTHER], &[OLD]);
        let replaced = unsafe { replace_import(image.as_ptr().cast(), OLD, NEW) };
        assert_eq!(replaced, 2);
        assert_eq!(slots(&image, 0x800, 4), [OTHER, NEW, OTHER, 0]);
        assert_eq!(slots(&image, 0xC00, 2), [NEW, 0]);
    }

    #[test]
    fn a_plugin_without_the_import_is_left_alone() {
        let image = image(&[OTHER], &[]);
        let replaced = unsafe { replace_import(image.as_ptr().cast(), OLD, NEW) };
        assert_eq!(replaced, 0);
        assert_eq!(slots(&image, 0x800, 2), [OTHER, 0]);
    }

    #[test]
    fn an_invalid_parent_changes_nothing() {
        editor_attached(ptr::null_mut(), ptr::null_mut());
        assert!(EMBEDDED_WINDOW.load(Ordering::Acquire).is_null());
    }
}

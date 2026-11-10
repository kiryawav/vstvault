use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use vst3::ComPtr;
use vst3::Steinberg::IPluginFactory;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW};

type ModuleEntry = unsafe extern "system" fn() -> bool;
type GetPluginFactory = unsafe extern "system" fn() -> *mut IPluginFactory;

pub struct Module {
    library: HMODULE,
    factory: Option<ComPtr<IPluginFactory>>,
}

impl Module {
    pub fn load(path: &Path) -> Result<Self, String> {
        let dll = dll_path(path);
        let wide: Vec<u16> = OsStr::new(&dll).encode_wide().chain(Some(0)).collect();
        let library = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
        if library.is_null() {
            return Err(format!("cannot load {}: {}", dll.display(), std::io::Error::last_os_error()));
        }
        crate::foreground::redirect_imports(library);
        let factory = unsafe {
            if let Some(init) = GetProcAddress(library, c"InitDll".as_ptr().cast()) {
                let init: ModuleEntry = std::mem::transmute(init);
                if !init() {
                    return Err("InitDll returned false".into());
                }
            }
            let get_factory = GetProcAddress(library, c"GetPluginFactory".as_ptr().cast())
                .ok_or("module has no GetPluginFactory export")?;
            let get_factory: GetPluginFactory = std::mem::transmute(get_factory);
            ComPtr::from_raw(get_factory()).ok_or("GetPluginFactory returned null")?
        };
        Ok(Self { library, factory: Some(factory) })
    }

    pub fn factory(&self) -> &ComPtr<IPluginFactory> {
        self.factory.as_ref().expect("the factory lives until drop")
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        self.factory = None;
        unsafe {
            if let Some(exit) = GetProcAddress(self.library, c"ExitDll".as_ptr().cast()) {
                let exit: ModuleEntry = std::mem::transmute(exit);
                exit();
            }
        }
    }
}

fn dll_path(path: &Path) -> PathBuf {
    if !path.is_dir() {
        return path.to_path_buf();
    }
    path.join("Contents").join("x86_64-win").join(path.file_name().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_module_is_an_error_not_a_crash() {
        let error = Module::load(Path::new("Z:\\no\\such\\plugin.vst3")).err().expect("must fail");
        assert!(error.contains("cannot load"));
    }

    #[test]
    fn bundle_folder_resolves_to_the_dll_inside() {
        let dir = std::env::temp_dir().join(format!("vault-bundle-{}.vst3", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let resolved = dll_path(&dir);
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(resolved, dir.join("Contents").join("x86_64-win").join(dir.file_name().unwrap()));
    }
}

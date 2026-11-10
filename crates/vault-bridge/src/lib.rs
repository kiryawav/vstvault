#![allow(non_snake_case)]

mod dispatch;
mod events;
mod factory;
mod instance;
mod text;
mod view;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use vault_protocol::manifest::{self, Manifest};
use vst3::ComWrapper;
use vst3::Steinberg::IPluginFactory;
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};

use crate::factory::Factory;

static LOG_FOLDER: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn log_error(message: &str) {
    use std::io::Write;
    let folder = LOG_FOLDER.get().cloned().unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&folder);
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(folder.join("vault-bridge.log")) {
        let _ = writeln!(file, "[pid {}] {message}", std::process::id());
    }
}

fn own_dll_path() -> Option<PathBuf> {
    let mut module = std::ptr::null_mut();
    let address = own_dll_path as *const () as *const u16;
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    if unsafe { GetModuleHandleExW(flags, address, &mut module) } == 0 {
        return None;
    }
    let mut buffer = vec![0u16; 32 * 1024];
    let length = unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    (length > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

fn load_manifest() -> Result<Manifest, String> {
    let dll = own_dll_path().ok_or("cannot find the bridge's own path")?;
    let contents = dll.parent().and_then(|p| p.parent()).ok_or("the bridge is not inside a VST3 bundle")?;
    let path = contents.join("Resources").join(manifest::FILE_NAME);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut manifest = Manifest::parse(&text)?;
    let folder = contents.parent().and_then(Path::parent).ok_or("the bridge's bundle has no folder")?;
    for path in [&mut manifest.host, &mut manifest.environment] {
        if !path.is_empty() && Path::new(path.as_str()).is_relative() {
            let full = std::path::absolute(folder.join(path.as_str())).map_err(|e| format!("cannot resolve {path}: {e}"))?;
            *path = full.to_string_lossy().into_owned();
        }
    }
    Ok(manifest)
}

pub(crate) fn bridge_manifest() -> Option<&'static Manifest> {
    static MANIFEST: OnceLock<Option<Manifest>> = OnceLock::new();
    MANIFEST
        .get_or_init(|| match load_manifest() {
            Ok(manifest) => {
                if let Some(vault) = Path::new(&manifest.host).parent().and_then(Path::parent) {
                    let _ = LOG_FOLDER.set(vault.join("logs"));
                }
                Some(manifest)
            }
            Err(message) => {
                log_error(&message);
                None
            }
        })
        .as_ref()
}

#[unsafe(no_mangle)]
extern "system" fn InitDll() -> bool {
    let mut module = std::ptr::null_mut();
    let address = own_dll_path as *const () as *const u16;
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN;
    unsafe { GetModuleHandleExW(flags, address, &mut module) };
    true
}

#[unsafe(no_mangle)]
extern "system" fn ExitDll() -> bool {
    true
}

#[unsafe(no_mangle)]
extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    match ComWrapper::new(Factory).to_com_ptr::<IPluginFactory>() {
        Some(factory) => factory.into_raw(),
        None => std::ptr::null_mut(),
    }
}

mod admin;
mod files;
mod hook;
mod keys;
mod nt;
pub mod paths;
mod process;
mod registry;

use std::ffi::c_void;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

use paths::Paths;

pub const ENV_VARIABLE: &str = "VAULT_ENV";
pub const TOOLS_VARIABLE: &str = "VAULT_TOOLS";
pub const TEMP_FOLDER: &str = "temp";
const OVERLAY_FOLDER: &str = "C";
const PROFILES_FILE: &str = "profiles.txt";

static ACTIVE: AtomicBool = AtomicBool::new(false);

fn device_of(drive: &str) -> Option<String> {
    let name: Vec<u16> = drive.encode_utf16().chain(Some(0)).collect();
    let mut buffer = [0u16; 512];
    let length = unsafe { QueryDosDeviceW(name.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    buffer[..length].split(|c| *c == 0).next().filter(|first| !first.is_empty()).map(String::from_utf16_lossy)
}

fn without_drive(path: &str) -> String {
    path.get(3..).unwrap_or("").trim_end_matches('\\').to_owned()
}

fn on_c(path: &str) -> bool {
    path.len() > 3 && path[..3].eq_ignore_ascii_case("C:\\")
}

fn known_profiles(environment: &Path, current: &str) -> Vec<String> {
    let file = environment.join(PROFILES_FILE);
    let mut profiles: Vec<String> = fs::read_to_string(&file).unwrap_or_default().lines().map(str::to_owned).collect();
    if !current.is_empty() && !profiles.iter().any(|profile| profile.eq_ignore_ascii_case(current)) {
        profiles.push(current.to_owned());
        let _ = fs::write(&file, profiles.join("\n") + "\n");
    }
    profiles
}

fn paths_for(environment: &Path) -> Result<Paths, String> {
    let root = environment.to_string_lossy().trim_start_matches(r"\\?\").trim_end_matches('\\').to_owned();
    let native_environment = paths::native_name(&root, &device_of).ok_or_else(|| format!("{root} is not on a lettered drive"))?;
    let overlay = environment.join(OVERLAY_FOLDER).to_string_lossy().trim_start_matches(r"\\?\").to_owned();
    let native_overlay = paths::native_name(&overlay, &device_of).ok_or_else(|| format!("{overlay} is not on a lettered drive"))?;
    let profile = std::env::var("USERPROFILE").ok().filter(|profile| on_c(profile)).map(|profile| without_drive(&profile)).unwrap_or_default();
    let earlier_profiles = known_profiles(environment, &profile).into_iter().filter(|known| !known.eq_ignore_ascii_case(&profile)).collect();
    let c_device = device_of("C:").ok_or("this PC has no C: drive")?;
    Ok(Paths { overlay, native_overlay, environment: root, native_environment, c_device, profile, earlier_profiles })
}

pub fn enter(environment: &Path, tools: &str) -> Result<(), String> {
    let name: Vec<u16> = "vault_env.dll\0".encode_utf16().collect();
    if unsafe { GetModuleHandleW(name.as_ptr()) }.is_null() { activate(environment, tools) } else { Ok(()) }
}

fn activate(environment: &Path, tools: &str) -> Result<(), String> {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return Err("this process is already in an environment".into());
    }
    let paths = paths_for(environment)?;
    let temp = environment.join(TEMP_FOLDER);
    for folder in [Path::new(&paths.overlay), temp.as_path()] {
        fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    }
    unsafe {
        std::env::set_var(ENV_VARIABLE, environment);
        std::env::set_var(TOOLS_VARIABLE, tools);
        std::env::set_var("TEMP", &temp);
        std::env::set_var("TMP", &temp);
    }
    let mut patcher = hook::Patcher::new()?;
    files::install(&mut patcher, paths.clone(), environment)?;
    registry::install(&mut patcher, environment)?;
    admin::install(&mut patcher)?;
    process::install(&mut patcher, paths, tools.to_owned())?;
    patcher.finish()
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(_module: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason != DLL_PROCESS_ATTACH {
        return 1;
    }
    let (Some(environment), Some(tools)) = (std::env::var_os(ENV_VARIABLE), std::env::var(TOOLS_VARIABLE).ok()) else {
        return 0;
    };
    match activate(Path::new(&environment), &tools) {
        Ok(()) => 1,
        Err(message) => {
            eprintln!("vault-env: {message}");
            0
        }
    }
}

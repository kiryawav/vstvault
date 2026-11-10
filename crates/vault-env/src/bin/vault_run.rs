use std::ffi::{OsString, c_void};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateRemoteThread, GetExitCodeThread, IsWow64Process, OpenProcess, PROCESS_ALL_ACCESS, WaitForSingleObject,
};

const INJECT_TIMEOUT_MS: u32 = 30_000;
const USAGE: &str = "usage: vault-run --env DIR PROGRAM [ARGS...]";

fn tools_folder() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let folder = exe.parent().ok_or("this program has no folder")?;
    Ok(if folder.file_name().is_some_and(|name| name == "x86") { folder.parent().unwrap_or(folder).to_path_buf() } else { folder.to_path_buf() })
}

fn inject(process_id: u32) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dll = exe.with_file_name("vault_env.dll");
    if !dll.is_file() {
        return Err(format!("{} is missing", dll.display()));
    }
    let wide: Vec<u16> = dll.to_string_lossy().encode_utf16().chain(Some(0)).collect();
    let bytes = wide.len() * 2;
    let kernel32: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
    unsafe {
        let process = OpenProcess(PROCESS_ALL_ACCESS, FALSE, process_id);
        if process.is_null() {
            return Err(format!("cannot open process {process_id}: {}", std::io::Error::last_os_error()));
        }
        let load = GetProcAddress(GetModuleHandleW(kernel32.as_ptr()), c"LoadLibraryW".as_ptr().cast()).ok_or("kernel32 has no LoadLibraryW")?;
        let remote = VirtualAllocEx(process, ptr::null(), bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() || WriteProcessMemory(process, remote, wide.as_ptr().cast::<c_void>(), bytes, ptr::null_mut()) == 0 {
            return Err(format!("cannot write to process {process_id}: {}", std::io::Error::last_os_error()));
        }
        let start: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(load);
        let thread = CreateRemoteThread(process, ptr::null(), 0, Some(start), remote, 0, ptr::null_mut());
        if thread.is_null() {
            return Err(format!("cannot start a thread in process {process_id}: {}", std::io::Error::last_os_error()));
        }
        if WaitForSingleObject(thread, INJECT_TIMEOUT_MS) != WAIT_OBJECT_0 {
            return Err("loading the environment's DLL took too long".into());
        }
        let mut result = 0;
        GetExitCodeThread(thread, &mut result);
        CloseHandle(thread);
        CloseHandle(process);
        if result == 0 {
            return Err("the process refused the environment's DLL".into());
        }
    }
    Ok(())
}

fn run_in(environment: &Path, program: &OsString, args: Vec<OsString>) -> Result<i32, String> {
    let tools = tools_folder()?;
    let mut child = Command::new(program)
        .args(args)
        .env(vault_env::ENV_VARIABLE, environment)
        .env(vault_env::TOOLS_VARIABLE, &tools)
        .env("__COMPAT_LAYER", "RunAsInvoker")
        .creation_flags(CREATE_SUSPENDED)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", program.to_string_lossy()))?;

    let mut is_wow64 = FALSE;
    if unsafe { IsWow64Process(child.as_raw_handle(), &mut is_wow64) } == 0 {
        let _ = child.kill();
        return Err(format!("cannot tell the program's bitness: {}", std::io::Error::last_os_error()));
    }
    let helper = if is_wow64 != FALSE { tools.join("x86").join("vault-run.exe") } else { tools.join("vault-run.exe") };
    let injected = Command::new(&helper).args(["--inject", &child.id().to_string()]).status().map_err(|e| format!("cannot start {}: {e}", helper.display()))?;
    if !injected.success() {
        let _ = child.kill();
        return Err(format!("{} failed: {injected}", helper.display()));
    }
    resume(&child)?;
    child.wait().map_err(|e| e.to_string())?.code().ok_or_else(|| "the program ended without an exit code".to_string())
}

fn resume(child: &std::process::Child) -> Result<(), String> {
    let ntdll: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
    unsafe {
        let resume = GetProcAddress(GetModuleHandleW(ntdll.as_ptr()), c"NtResumeProcess".as_ptr().cast()).ok_or("ntdll has no NtResumeProcess")?;
        let resume: unsafe extern "system" fn(*mut c_void) -> i32 = std::mem::transmute(resume);
        let status = resume(child.as_raw_handle());
        if status < 0 { Err(format!("cannot resume the program: {status:#x}")) } else { Ok(()) }
    }
}

fn run() -> Result<i32, String> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().ok_or(USAGE)?;
    let value = args.next().ok_or(USAGE)?;
    match mode.to_str() {
        Some("--inject") => {
            let id = value.to_string_lossy().parse().map_err(|_| USAGE.to_string())?;
            inject(id).map(|()| 0)
        }
        Some("--env") => {
            let program = args.next().ok_or(USAGE)?;
            run_in(Path::new(&value), &program, args.collect())
        }
        _ => Err(USAGE.into()),
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("vault-run: {message}");
            std::process::exit(125);
        }
    }
}

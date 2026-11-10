use std::ffi::c_void;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::FALSE;
use windows_sys::Win32::System::Threading::{GetProcessId, IsWow64Process, TerminateProcess};

use crate::hook::Patcher;
use crate::nt::{Handle, ObjectAttributes, OwnWork, Status};
use crate::paths::Paths;

const THREAD_CREATE_SUSPENDED: u32 = 1;
const ATTRIBUTE_IMAGE_NAME: usize = 0x2_0005;
const STATUS_DLL_INIT_FAILED: Status = 0xC000_0142_u32 as i32;

static ORIG_CREATE_PROCESS: AtomicUsize = AtomicUsize::new(0);
static ORIG_RESUME_THREAD: AtomicUsize = AtomicUsize::new(0);
static STATE: OnceLock<(Paths, String)> = OnceLock::new();

struct Pending {
    thread: usize,
    process: usize,
    id: u32,
    is_32_bit: bool,
}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());

#[repr(C)]
struct Attribute {
    attribute: usize,
    size: usize,
    value: *mut u16,
    return_length: *mut usize,
}

#[repr(C)]
struct AttributeList {
    total_length: usize,
    attributes: [Attribute; 1],
}

type CreateProcess = unsafe extern "system" fn(
    *mut Handle,
    *mut Handle,
    u32,
    u32,
    *mut ObjectAttributes,
    *mut ObjectAttributes,
    u32,
    u32,
    *mut c_void,
    *mut c_void,
    *mut AttributeList,
) -> Status;
type ResumeThread = unsafe extern "system" fn(Handle, *mut u32) -> Status;

pub fn install(patcher: &mut Patcher, paths: Paths, tools: String) -> Result<(), String> {
    STATE.set((paths, tools)).map_err(|_| "the environment is already active")?;
    ORIG_CREATE_PROCESS.store(patcher.hook("NtCreateUserProcess", nt_create_user_process as *const () as usize)?, Ordering::Relaxed);
    ORIG_RESUME_THREAD.store(patcher.hook("NtResumeThread", nt_resume_thread as *const () as usize)?, Ordering::Relaxed);
    Ok(())
}

fn enter_environment(pending: &Pending, tools: &str) -> Result<(), String> {
    let helper = if pending.is_32_bit { format!("{tools}\\x86\\vault-run.exe") } else { format!("{tools}\\vault-run.exe") };
    let status = Command::new(&helper)
        .args(["--inject", &pending.id.to_string()])
        .env_remove("__COMPAT_LAYER")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("cannot start {helper}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{helper} failed: {status}")) }
}

unsafe extern "system" fn nt_create_user_process(
    process: *mut Handle,
    thread: *mut Handle,
    process_access: u32,
    thread_access: u32,
    process_attributes: *mut ObjectAttributes,
    thread_attributes: *mut ObjectAttributes,
    process_flags: u32,
    thread_flags: u32,
    parameters: *mut c_void,
    create_info: *mut c_void,
    list: *mut AttributeList,
) -> Status {
    let call = |thread_flags| unsafe {
        let create: CreateProcess = std::mem::transmute(ORIG_CREATE_PROCESS.load(Ordering::Relaxed));
        create(process, thread, process_access, thread_access, process_attributes, thread_attributes, process_flags, thread_flags, parameters, create_info, list)
    };
    let (Some((paths, _)), Some(own_work)) = (STATE.get(), OwnWork::enter()) else { return call(thread_flags) };

    let mut image = Vec::new();
    unsafe {
        if let Some(list) = list.as_mut() {
            let count = (list.total_length - size_of::<usize>()) / size_of::<Attribute>();
            let attributes = std::slice::from_raw_parts_mut((&raw mut list.attributes).cast::<Attribute>(), count);
            for attribute in attributes.iter_mut().filter(|a| a.attribute == ATTRIBUTE_IMAGE_NAME) {
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(attribute.value, attribute.size / 2));
                if let Some(virtual_path) = paths.virtual_of(&name) {
                    if std::path::Path::new(&paths.overlay_win32(&virtual_path)).is_file() {
                        image = paths.overlay_nt(&virtual_path).encode_utf16().collect::<Vec<u16>>();
                        attribute.value = image.as_mut_ptr();
                        attribute.size = image.len() * 2;
                    }
                }
            }
        }
    }
    drop(own_work);

    let status = call(thread_flags | THREAD_CREATE_SUSPENDED);
    drop(image);
    if status < 0 {
        return status;
    }
    let pending = unsafe {
        let mut is_wow64 = FALSE;
        IsWow64Process(*process, &mut is_wow64);
        Pending { thread: *thread as usize, process: *process as usize, id: GetProcessId(*process), is_32_bit: is_wow64 != FALSE }
    };
    if let Ok(mut list) = PENDING.lock() {
        list.push(pending);
    }
    if thread_flags & THREAD_CREATE_SUSPENDED == 0 {
        let resumed = unsafe { nt_resume_thread(*thread, std::ptr::null_mut()) };
        if resumed < 0 {
            return resumed;
        }
    }
    status
}

unsafe extern "system" fn nt_resume_thread(thread: Handle, previous: *mut u32) -> Status {
    let call = || unsafe {
        let resume: ResumeThread = std::mem::transmute(ORIG_RESUME_THREAD.load(Ordering::Relaxed));
        resume(thread, previous)
    };
    let found = PENDING.lock().ok().and_then(|mut list| {
        let index = list.iter().position(|pending| pending.thread == thread as usize)?;
        Some(list.swap_remove(index))
    });
    let (Some(pending), Some((_, tools)), Some(own_work)) = (found, STATE.get(), OwnWork::enter()) else { return call() };
    let entered = enter_environment(&pending, tools);
    drop(own_work);
    if let Err(message) = entered {
        unsafe { TerminateProcess(pending.process as Handle, 1) };
        eprintln!("vault-env: {message}");
        return STATUS_DLL_INIT_FAILED;
    }
    call()
}

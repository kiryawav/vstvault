use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::hook::Patcher;
use crate::nt::{
    Handle, ObjectAttributes, OwnWork, OwnedName, STATUS_NO_MORE_FILES, STATUS_NO_SUCH_FILE, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SUCCESS, Status, UnicodeString, text_of,
};
use crate::paths::{Paths, parent};

const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OPEN_IF: u32 = 3;
const FILE_OVERWRITE: u32 = 4;
const FILE_DELETE_ON_CLOSE: u32 = 0x1000;
const WRITE_ACCESS: u32 = 0x0002 | 0x0004 | 0x0010 | 0x0040 | 0x0100 | 0x0001_0000 | 0x0004_0000 | 0x0008_0000 | 0x1000_0000 | 0x4000_0000;

const INFO_RENAME: u32 = 10;
const INFO_LINK: u32 = 11;
const INFO_DISPOSITION: u32 = 13;
const INFO_DISPOSITION_EX: u32 = 64;
const INFO_RENAME_EX: u32 = 65;
const INFO_LINK_EX: u32 = 72;
const OBJECT_NAME_INFORMATION: u32 = 1;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

struct State {
    paths: Paths,
    deleted: Mutex<HashSet<String>>,
    deleted_file: String,
}

static STATE: OnceLock<State> = OnceLock::new();

macro_rules! originals {
    ($($name:ident),* $(,)?) => { $( static $name: AtomicUsize = AtomicUsize::new(0); )* };
}
originals!(ORIG_CREATE, ORIG_OPEN, ORIG_ATTRIBUTES, ORIG_FULL_ATTRIBUTES, ORIG_DELETE, ORIG_SET_INFO, ORIG_QUERY_OBJECT, ORIG_CLOSE, ORIG_QUERY_DIRECTORY, ORIG_QUERY_DIRECTORY_EX);

#[repr(C)]
struct IoStatus {
    status: usize,
    information: usize,
}

type Close = unsafe extern "system" fn(Handle) -> Status;
type QueryDirectory = unsafe extern "system" fn(Handle, Handle, *mut c_void, *mut c_void, *mut IoStatus, *mut u8, u32, u32, u8, *const UnicodeString, u8) -> Status;
type QueryDirectoryEx = unsafe extern "system" fn(Handle, Handle, *mut c_void, *mut c_void, *mut IoStatus, *mut u8, u32, u32, u32, *const UnicodeString) -> Status;

type CreateFile = unsafe extern "system" fn(*mut Handle, u32, *mut ObjectAttributes, *mut c_void, *mut c_void, u32, u32, u32, u32, *mut c_void, u32) -> Status;
type OpenFile = unsafe extern "system" fn(*mut Handle, u32, *mut ObjectAttributes, *mut c_void, u32, u32) -> Status;
type Attributes = unsafe extern "system" fn(*mut ObjectAttributes, *mut c_void) -> Status;
type DeleteFile = unsafe extern "system" fn(*mut ObjectAttributes) -> Status;
type SetInfo = unsafe extern "system" fn(Handle, *mut c_void, *mut c_void, u32, u32) -> Status;
type QueryObject = unsafe extern "system" fn(Handle, u32, *mut c_void, u32, *mut u32) -> Status;

unsafe fn original<F: Copy>(slot: &AtomicUsize) -> F {
    let address = slot.load(Ordering::Relaxed);
    unsafe { std::mem::transmute_copy(&address) }
}

pub fn install(patcher: &mut Patcher, paths: Paths, environment: &Path) -> Result<(), String> {
    let deleted_file = environment.join("deleted.txt").to_string_lossy().into_owned();
    let deleted = fs::read_to_string(&deleted_file).unwrap_or_default().lines().map(str::to_owned).collect();
    STATE.set(State { paths, deleted: Mutex::new(deleted), deleted_file }).map_err(|_| "the environment is already active")?;

    ORIG_CREATE.store(patcher.hook("NtCreateFile", nt_create_file as *const () as usize)?, Ordering::Relaxed);
    ORIG_OPEN.store(patcher.hook("NtOpenFile", nt_open_file as *const () as usize)?, Ordering::Relaxed);
    ORIG_ATTRIBUTES.store(patcher.hook("NtQueryAttributesFile", nt_query_attributes as *const () as usize)?, Ordering::Relaxed);
    ORIG_FULL_ATTRIBUTES.store(patcher.hook("NtQueryFullAttributesFile", nt_query_full_attributes as *const () as usize)?, Ordering::Relaxed);
    ORIG_DELETE.store(patcher.hook("NtDeleteFile", nt_delete_file as *const () as usize)?, Ordering::Relaxed);
    ORIG_SET_INFO.store(patcher.hook("NtSetInformationFile", nt_set_information_file as *const () as usize)?, Ordering::Relaxed);
    ORIG_CLOSE.store(patcher.hook("NtClose", nt_close as *const () as usize)?, Ordering::Relaxed);
    ORIG_QUERY_DIRECTORY.store(patcher.hook("NtQueryDirectoryFile", nt_query_directory_file as *const () as usize)?, Ordering::Relaxed);
    ORIG_QUERY_DIRECTORY_EX.store(patcher.hook("NtQueryDirectoryFileEx", nt_query_directory_file_ex as *const () as usize)?, Ordering::Relaxed);
    let name: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
    let query = unsafe {
        windows_sys::Win32::System::LibraryLoader::GetProcAddress(
            windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(name.as_ptr()),
            c"NtQueryObject".as_ptr().cast(),
        )
    };
    ORIG_QUERY_OBJECT.store(query.ok_or("ntdll has no NtQueryObject")? as usize, Ordering::Relaxed);
    Ok(())
}

fn attributes_of(path: &str) -> Option<u32> {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let attributes = unsafe { windows_sys::Win32::Storage::FileSystem::GetFileAttributesW(wide.as_ptr()) };
    (attributes != INVALID_FILE_ATTRIBUTES).then_some(attributes)
}

fn is_directory(attributes: u32) -> bool {
    attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY != 0
}

impl State {
    fn is_deleted(&self, virtual_path: &str) -> bool {
        self.deleted.lock().is_ok_and(|deleted| deleted.contains(&virtual_path.to_lowercase()))
    }

    fn hide_real(&self, virtual_path: &str) {
        if attributes_of(&self.paths.real_win32(virtual_path)).is_none() {
            return;
        }
        let key = virtual_path.to_lowercase();
        if let Ok(mut deleted) = self.deleted.lock() {
            if deleted.insert(key.clone()) {
                if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&self.deleted_file) {
                    let _ = writeln!(file, "{key}");
                }
            }
        }
    }

    fn real_attributes(&self, virtual_path: &str) -> Option<u32> {
        if self.is_deleted(virtual_path) { None } else { attributes_of(&self.paths.real_win32(virtual_path)) }
    }

    fn exists_anywhere(&self, virtual_path: &str) -> bool {
        attributes_of(&self.paths.overlay_win32(virtual_path)).is_some() || self.real_attributes(virtual_path).is_some()
    }

    fn make_parent(&self, virtual_path: &str) {
        if let Some(folder) = parent(virtual_path) {
            let _ = fs::create_dir_all(self.paths.overlay_win32(folder));
        }
    }

    fn copy_up(&self, virtual_path: &str, attributes: u32) {
        self.make_parent(virtual_path);
        let target = self.paths.overlay_win32(virtual_path);
        if is_directory(attributes) {
            let _ = fs::create_dir(target);
        } else {
            let _ = fs::copy(self.paths.real_win32(virtual_path), target);
        }
    }
}

enum Plan {
    Real,
    Overlay,
    Fail(Status),
}

fn plan_open(state: &State, virtual_path: &str, access: u32, disposition: u32, options: u32) -> Plan {
    let in_overlay = attributes_of(&state.paths.overlay_win32(virtual_path)).is_some();
    let real = state.real_attributes(virtual_path);
    let deleted_before = real.is_none() && state.is_deleted(virtual_path);
    let writes = access & WRITE_ACCESS != 0 || disposition != FILE_OPEN;
    let result = if in_overlay {
        Plan::Overlay
    } else if !writes {
        if deleted_before { Plan::Fail(STATUS_OBJECT_NAME_NOT_FOUND) } else { Plan::Real }
    } else {
        match (disposition, real) {
            (FILE_CREATE, Some(_)) => Plan::Fail(STATUS_OBJECT_NAME_COLLISION),
            (FILE_OPEN | FILE_OPEN_IF, Some(attributes)) => {
                state.copy_up(virtual_path, attributes);
                Plan::Overlay
            }
            (FILE_OPEN | FILE_OVERWRITE, None) => Plan::Fail(STATUS_OBJECT_NAME_NOT_FOUND),
            _ => match parent(virtual_path) {
                Some(folder) if !state.exists_anywhere(folder) => Plan::Fail(STATUS_OBJECT_PATH_NOT_FOUND),
                _ => {
                    state.make_parent(virtual_path);
                    Plan::Overlay
                }
            },
        }
    };
    if matches!(result, Plan::Overlay) && (options & FILE_DELETE_ON_CLOSE != 0) {
        state.hide_real(virtual_path);
    }
    result
}

fn name_of_handle(handle: Handle) -> Option<String> {
    let mut buffer = vec![0u64; 512];
    let mut returned = 0;
    let status = unsafe {
        original::<QueryObject>(&ORIG_QUERY_OBJECT)(handle, OBJECT_NAME_INFORMATION, buffer.as_mut_ptr().cast(), (buffer.len() * 8) as u32, &mut returned)
    };
    if status < 0 {
        return None;
    }
    unsafe { text_of(buffer.as_ptr().cast::<UnicodeString>()) }
}

unsafe fn virtual_of_attributes(state: &State, attributes: *const ObjectAttributes) -> Option<String> {
    let (root, name) = unsafe {
        let attributes = attributes.as_ref()?;
        (attributes.root_directory, text_of(attributes.object_name)?)
    };
    if root.is_null() {
        return state.paths.virtual_of(&name);
    }
    let folder = name_of_handle(root)?;
    if name.is_empty() {
        return state.paths.virtual_of(&folder);
    }
    state.paths.virtual_of(&format!("{folder}\\{name}"))
}

unsafe extern "system" fn nt_create_file(
    handle: *mut Handle,
    access: u32,
    attributes: *mut ObjectAttributes,
    status_block: *mut c_void,
    allocation: *mut c_void,
    file_attributes: u32,
    share: u32,
    disposition: u32,
    options: u32,
    extended: *mut c_void,
    extended_length: u32,
) -> Status {
    let call = |attributes| unsafe {
        original::<CreateFile>(&ORIG_CREATE)(handle, access, attributes, status_block, allocation, file_attributes, share, disposition, options, extended, extended_length)
    };
    let (Some(state), Some(own_work)) = (STATE.get(), OwnWork::enter()) else { return call(attributes) };
    let Some(virtual_path) = (unsafe { virtual_of_attributes(state, attributes) }) else {
        drop(own_work);
        return call(attributes);
    };
    let plan = plan_open(state, &virtual_path, access, disposition, options);
    unsafe { carry_out(state, &virtual_path, plan, attributes, handle, call) }
}

unsafe fn carry_out(
    state: &State,
    virtual_path: &str,
    plan: Plan,
    attributes: *mut ObjectAttributes,
    handle: *mut Handle,
    call: impl Fn(*mut ObjectAttributes) -> Status,
) -> Status {
    match plan {
        Plan::Real => {
            let mut real = OwnedName::new(&state.paths.real_nt(virtual_path), unsafe { &*attributes }, true);
            call(real.attributes())
        }
        Plan::Overlay => {
            let mut overlay = OwnedName::new(&state.paths.overlay_nt(virtual_path), unsafe { &*attributes }, false);
            let status = call(overlay.attributes());
            if status >= 0 && attributes_of(&state.paths.overlay_win32(virtual_path)).is_some_and(is_directory) && state.real_attributes(virtual_path).is_some_and(is_directory) {
                remember_merged_folder(unsafe { *handle }, virtual_path);
            }
            status
        }
        Plan::Fail(status) => status,
    }
}

unsafe extern "system" fn nt_open_file(handle: *mut Handle, access: u32, attributes: *mut ObjectAttributes, status_block: *mut c_void, share: u32, options: u32) -> Status {
    let call = |attributes| unsafe { original::<OpenFile>(&ORIG_OPEN)(handle, access, attributes, status_block, share, options) };
    let (Some(state), Some(own_work)) = (STATE.get(), OwnWork::enter()) else { return call(attributes) };
    let Some(virtual_path) = (unsafe { virtual_of_attributes(state, attributes) }) else {
        drop(own_work);
        return call(attributes);
    };
    let plan = plan_open(state, &virtual_path, access, FILE_OPEN, options);
    unsafe { carry_out(state, &virtual_path, plan, attributes, handle, call) }
}

struct MergedFolder {
    virtual_path: String,
    real_handle: Handle,
    in_real_part: bool,
    pattern: Option<Vec<u16>>,
    returned_any: bool,
}

unsafe impl Send for MergedFolder {}

static MERGED: Mutex<Option<HashMap<usize, MergedFolder>>> = Mutex::new(None);
static MERGED_COUNT: AtomicUsize = AtomicUsize::new(0);

fn remember_merged_folder(handle: Handle, virtual_path: &str) {
    if let Ok(mut merged) = MERGED.lock() {
        let folder = MergedFolder { virtual_path: virtual_path.to_owned(), real_handle: std::ptr::null_mut(), in_real_part: false, pattern: None, returned_any: false };
        if merged.get_or_insert_with(HashMap::new).insert(handle as usize, folder).is_none() {
            MERGED_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe extern "system" fn nt_close(handle: Handle) -> Status {
    if MERGED_COUNT.load(Ordering::Relaxed) != 0 {
        let folder = MERGED.lock().ok().and_then(|mut merged| merged.as_mut()?.remove(&(handle as usize)));
        if let Some(folder) = folder {
            MERGED_COUNT.fetch_sub(1, Ordering::Relaxed);
            if !folder.real_handle.is_null() {
                unsafe { original::<Close>(&ORIG_CLOSE)(folder.real_handle) };
            }
        }
    }
    unsafe { original::<Close>(&ORIG_CLOSE)(handle) }
}

fn name_offsets(class: u32) -> Option<(usize, usize)> {
    match class {
        1 => Some((60, 64)),
        2 => Some((60, 68)),
        3 => Some((60, 94)),
        12 => Some((8, 12)),
        37 => Some((60, 104)),
        38 => Some((60, 80)),
        _ => None,
    }
}

unsafe fn filter_listing(buffer: *mut u8, used: usize, class: u32, hide: impl Fn(&str) -> bool) -> usize {
    let Some((length_at, name_at)) = name_offsets(class) else { return used };
    let mut kept: Vec<u8> = Vec::with_capacity(used);
    let mut previous_start = None;
    let mut entry = 0usize;
    unsafe {
        loop {
            let at = buffer.add(entry);
            let next = at.cast::<u32>().read_unaligned() as usize;
            let name_bytes = at.add(length_at).cast::<u32>().read_unaligned() as usize;
            let name: Vec<u16> = (0..name_bytes / 2).map(|i| at.add(name_at + i * 2).cast::<u16>().read_unaligned()).collect();
            if !hide(&String::from_utf16_lossy(&name)) {
                let start = kept.len().next_multiple_of(8);
                kept.resize(start, 0);
                kept.extend_from_slice(std::slice::from_raw_parts(at, name_at + name_bytes));
                kept[start..start + 4].copy_from_slice(&0u32.to_le_bytes());
                if let Some(previous) = previous_start {
                    kept[previous..previous + 4].copy_from_slice(&((start - previous) as u32).to_le_bytes());
                }
                previous_start = Some(start);
            }
            if next == 0 {
                break;
            }
            entry += next;
        }
        std::ptr::copy_nonoverlapping(kept.as_ptr(), buffer, kept.len());
    }
    kept.len()
}

unsafe fn merged_listing(
    handle: Handle,
    status_block: *mut IoStatus,
    buffer: *mut u8,
    class: u32,
    pattern: *const UnicodeString,
    restart: bool,
    query: impl Fn(Handle, bool, *const UnicodeString) -> Status,
) -> Status {
    let plain = || query(handle, restart, pattern);
    if MERGED_COUNT.load(Ordering::Relaxed) == 0 {
        return plain();
    }
    let Some(state) = STATE.get() else { return plain() };
    let Ok(mut merged) = MERGED.lock() else { return plain() };
    let Some(folder) = merged.as_mut().and_then(|merged| merged.get_mut(&(handle as usize))) else {
        drop(merged);
        return plain();
    };
    let Some(_own_work) = OwnWork::enter() else {
        drop(merged);
        return plain();
    };
    if restart {
        folder.in_real_part = false;
        folder.returned_any = false;
    }
    unsafe {
        if let Some(given) = pattern.as_ref().filter(|given| !given.buffer.is_null()) {
            folder.pattern = Some(std::slice::from_raw_parts(given.buffer, usize::from(given.length) / 2).to_vec());
        }
    }
    let mut first_of_part = restart;
    if !folder.in_real_part {
        let status = query(handle, first_of_part, pattern);
        if status != STATUS_NO_MORE_FILES && status != STATUS_NO_SUCH_FILE {
            folder.returned_any |= status >= 0;
            return status;
        }
        folder.in_real_part = true;
        first_of_part = true;
    }
    if folder.real_handle.is_null() {
        folder.real_handle = open_real_folder(state, &folder.virtual_path);
        if folder.real_handle.is_null() {
            return if folder.returned_any { STATUS_NO_MORE_FILES } else { STATUS_NO_SUCH_FILE };
        }
    }
    let mut pattern_string = folder.pattern.as_mut().map(|wide| UnicodeString {
        length: (wide.len() * 2) as u16,
        maximum_length: (wide.len() * 2) as u16,
        buffer: wide.as_mut_ptr(),
    });
    let pattern_pointer = pattern_string.as_mut().map_or(std::ptr::null(), |string| &raw const *string);
    loop {
        let status = query(folder.real_handle, first_of_part, if first_of_part { pattern_pointer } else { std::ptr::null() });
        first_of_part = false;
        if status < 0 {
            return if status == STATUS_NO_SUCH_FILE && folder.returned_any { STATUS_NO_MORE_FILES } else { status };
        }
        let folder_path = &folder.virtual_path;
        let hide = |name: &str| {
            let inside = if folder_path.is_empty() { name.to_owned() } else { format!("{folder_path}\\{name}") };
            attributes_of(&state.paths.overlay_win32(&inside)).is_some() || state.is_deleted(&inside)
        };
        let remaining = unsafe {
            let used = (*status_block).information;
            filter_listing(buffer, used, class, hide)
        };
        if remaining > 0 {
            unsafe { (*status_block).information = remaining };
            folder.returned_any = true;
            return status;
        }
    }
}

fn open_real_folder(state: &State, virtual_path: &str) -> Handle {
    const LIST_AND_SYNCHRONIZE: u32 = 0x0010_0001;
    const SHARE_ALL: u32 = 7;
    const DIRECTORY_AND_SYNCHRONOUS: u32 = 0x21;
    const CASE_INSENSITIVE: u32 = 0x40;
    let template = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: std::ptr::null_mut(),
        object_name: std::ptr::null_mut(),
        attributes: CASE_INSENSITIVE,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut name = OwnedName::new(&state.paths.real_nt(virtual_path), &template, false);
    let mut handle = std::ptr::null_mut();
    let mut status_block = IoStatus { status: 0, information: 0 };
    let status = unsafe {
        original::<OpenFile>(&ORIG_OPEN)(&mut handle, LIST_AND_SYNCHRONIZE, name.attributes(), (&raw mut status_block).cast(), SHARE_ALL, DIRECTORY_AND_SYNCHRONOUS)
    };
    if status < 0 { std::ptr::null_mut() } else { handle }
}

unsafe extern "system" fn nt_query_directory_file(
    handle: Handle,
    event: Handle,
    routine: *mut c_void,
    context: *mut c_void,
    status_block: *mut IoStatus,
    buffer: *mut u8,
    length: u32,
    class: u32,
    single: u8,
    pattern: *const UnicodeString,
    restart: u8,
) -> Status {
    unsafe {
        merged_listing(handle, status_block, buffer, class, pattern, restart != 0, |target, restart, pattern| {
            original::<QueryDirectory>(&ORIG_QUERY_DIRECTORY)(target, event, routine, context, status_block, buffer, length, class, single, pattern, u8::from(restart))
        })
    }
}

unsafe extern "system" fn nt_query_directory_file_ex(
    handle: Handle,
    event: Handle,
    routine: *mut c_void,
    context: *mut c_void,
    status_block: *mut IoStatus,
    buffer: *mut u8,
    length: u32,
    class: u32,
    flags: u32,
    pattern: *const UnicodeString,
) -> Status {
    const RESTART_SCAN: u32 = 1;
    unsafe {
        merged_listing(handle, status_block, buffer, class, pattern, flags & RESTART_SCAN != 0, |target, restart, pattern| {
            let flags = if restart { flags | RESTART_SCAN } else { flags & !RESTART_SCAN };
            original::<QueryDirectoryEx>(&ORIG_QUERY_DIRECTORY_EX)(target, event, routine, context, status_block, buffer, length, class, flags, pattern)
        })
    }
}

unsafe fn query_attributes(slot: &AtomicUsize, attributes: *mut ObjectAttributes, information: *mut c_void) -> Status {
    let call = |attributes| unsafe { original::<Attributes>(slot)(attributes, information) };
    let (Some(state), Some(own_work)) = (STATE.get(), OwnWork::enter()) else { return call(attributes) };
    let Some(virtual_path) = (unsafe { virtual_of_attributes(state, attributes) }) else {
        drop(own_work);
        return call(attributes);
    };
    let path = if attributes_of(&state.paths.overlay_win32(&virtual_path)).is_some() {
        state.paths.overlay_nt(&virtual_path)
    } else if state.is_deleted(&virtual_path) {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    } else {
        state.paths.real_nt(&virtual_path)
    };
    let mut name = OwnedName::new(&path, unsafe { &*attributes }, true);
    call(name.attributes())
}

unsafe extern "system" fn nt_query_attributes(attributes: *mut ObjectAttributes, information: *mut c_void) -> Status {
    unsafe { query_attributes(&ORIG_ATTRIBUTES, attributes, information) }
}

unsafe extern "system" fn nt_query_full_attributes(attributes: *mut ObjectAttributes, information: *mut c_void) -> Status {
    unsafe { query_attributes(&ORIG_FULL_ATTRIBUTES, attributes, information) }
}

unsafe extern "system" fn nt_delete_file(attributes: *mut ObjectAttributes) -> Status {
    let call = |attributes| unsafe { original::<DeleteFile>(&ORIG_DELETE)(attributes) };
    let (Some(state), Some(own_work)) = (STATE.get(), OwnWork::enter()) else { return call(attributes) };
    let Some(virtual_path) = (unsafe { virtual_of_attributes(state, attributes) }) else {
        drop(own_work);
        return call(attributes);
    };
    let in_overlay = attributes_of(&state.paths.overlay_win32(&virtual_path)).is_some();
    let real = state.real_attributes(&virtual_path).is_some();
    if real {
        state.hide_real(&virtual_path);
    }
    if in_overlay {
        let mut overlay = OwnedName::new(&state.paths.overlay_nt(&virtual_path), unsafe { &*attributes }, true);
        call(overlay.attributes())
    } else if real {
        STATUS_SUCCESS
    } else {
        STATUS_OBJECT_NAME_NOT_FOUND
    }
}

#[repr(C)]
struct RenameHeader {
    flags: usize,
    root_directory: Handle,
    name_length: u32,
    name: [u16; 1],
}

unsafe fn redirected_rename(state: &State, information: *const c_void, length: u32) -> Option<Vec<u64>> {
    let header = information.cast::<RenameHeader>();
    let (root, name) = unsafe {
        let count = ((*header).name_length / 2) as usize;
        if std::mem::offset_of!(RenameHeader, name) + count * 2 > length as usize {
            return None;
        }
        let name = String::from_utf16(std::slice::from_raw_parts((&raw const (*header).name).cast::<u16>(), count)).ok()?;
        ((*header).root_directory, name)
    };
    let full = if root.is_null() { name } else { format!("{}\\{name}", name_of_handle(root)?) };
    let virtual_path = state.paths.virtual_of(&full)?;
    state.make_parent(&virtual_path);
    let target: Vec<u16> = state.paths.overlay_nt(&virtual_path).encode_utf16().collect();
    let bytes = std::mem::offset_of!(RenameHeader, name) + target.len() * 2;
    let mut buffer = vec![0u64; bytes.div_ceil(8) + 1];
    let copy = buffer.as_mut_ptr().cast::<RenameHeader>();
    unsafe {
        (*copy).flags = (*header).flags;
        (*copy).root_directory = std::ptr::null_mut();
        (*copy).name_length = (target.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(target.as_ptr(), (&raw mut (*copy).name).cast::<u16>(), target.len());
    }
    Some(buffer)
}

unsafe extern "system" fn nt_set_information_file(handle: Handle, status_block: *mut c_void, information: *mut c_void, length: u32, class: u32) -> Status {
    let call = |information, length| unsafe { original::<SetInfo>(&ORIG_SET_INFO)(handle, status_block, information, length, class) };
    let (Some(state), Some(own_work)) = (STATE.get(), OwnWork::enter()) else { return call(information, length) };
    match class {
        INFO_RENAME | INFO_RENAME_EX | INFO_LINK | INFO_LINK_EX => {
            let Some(mut redirected) = (unsafe { redirected_rename(state, information, length) }) else {
                drop(own_work);
                return call(information, length);
            };
            if matches!(class, INFO_RENAME | INFO_RENAME_EX) {
                if let Some(source) = name_of_handle(handle).and_then(|name| state.paths.virtual_of(&name)) {
                    state.hide_real(&source);
                }
            }
            let bytes = (redirected.len() * 8) as u32;
            call(redirected.as_mut_ptr().cast(), bytes)
        }
        INFO_DISPOSITION | INFO_DISPOSITION_EX => {
            let deleting = length > 0 && unsafe { *information.cast::<u8>() } & 1 != 0;
            if deleting {
                if let Some(source) = name_of_handle(handle).and_then(|name| state.paths.virtual_of(&name)) {
                    state.hide_real(&source);
                }
            }
            call(information, length)
        }
        _ => {
            drop(own_work);
            call(information, length)
        }
    }
}

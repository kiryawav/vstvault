use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::hook::Patcher;
use crate::keys::KeyPaths;
use crate::nt::{Handle, ObjectAttributes, OwnWork, OwnedName, STATUS_OBJECT_NAME_NOT_FOUND, Status, UnicodeString, text_of};

const STATUS_NO_MORE_ENTRIES: Status = 0x8000_001A_u32 as i32;
const STATUS_BUFFER_OVERFLOW: Status = 0x8000_0005_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: Status = 0xC000_0023_u32 as i32;
const KEY_ALL_ACCESS: u32 = 0xF003F;
const KEY_READ: u32 = 0x20019;
const KEY_SET_VALUE: u32 = 0x0002;
const KEY_CREATE_SUB_KEY: u32 = 0x0004;
const KEY_CREATE_LINK: u32 = 0x0020;
const KEY_WOW64_64KEY: u32 = 0x100;
const VIEW_FLAGS: u32 = 0x300;
const WRITE_ACCESS: u32 = KEY_SET_VALUE | KEY_CREATE_SUB_KEY | KEY_CREATE_LINK | 0x0001_0000 | 0x0004_0000 | 0x0008_0000 | 0x1000_0000 | 0x4000_0000;
const CASE_INSENSITIVE: u32 = 0x40;
const CREATED_NEW_KEY: u32 = 1;
const OPENED_EXISTING_KEY: u32 = 2;

const KEY_BASIC_INFORMATION: u32 = 0;
const KEY_FULL_INFORMATION: u32 = 2;
const KEY_NAME_INFORMATION: u32 = 3;
const KEY_CACHED_INFORMATION: u32 = 4;
const KEY_VALUE_FULL_INFORMATION: u32 = 1;

macro_rules! originals {
    ($($name:ident),* $(,)?) => { $( static $name: AtomicUsize = AtomicUsize::new(0); )* };
}
originals!(ORIG_OPEN_KEY, ORIG_OPEN_KEY_EX, ORIG_CREATE_KEY, ORIG_DELETE_KEY, ORIG_ENUMERATE_KEY, ORIG_QUERY_KEY, ORIG_ENUMERATE_VALUE, ORIG_SET_VALUE, ORIG_CLOSE);

type OpenKey = unsafe extern "system" fn(*mut Handle, u32, *mut ObjectAttributes) -> Status;
type OpenKeyEx = unsafe extern "system" fn(*mut Handle, u32, *mut ObjectAttributes, u32) -> Status;
type CreateKey = unsafe extern "system" fn(*mut Handle, u32, *mut ObjectAttributes, u32, *mut UnicodeString, u32, *mut u32) -> Status;
type DeleteKey = unsafe extern "system" fn(Handle) -> Status;
type EnumerateKey = unsafe extern "system" fn(Handle, u32, u32, *mut u8, u32, *mut u32) -> Status;
type QueryKey = unsafe extern "system" fn(Handle, u32, *mut u8, u32, *mut u32) -> Status;
type EnumerateValue = unsafe extern "system" fn(Handle, u32, u32, *mut u8, u32, *mut u32) -> Status;
type SetValue = unsafe extern "system" fn(Handle, *mut UnicodeString, u32, u32, *const u8, u32) -> Status;
type Close = unsafe extern "system" fn(Handle) -> Status;

struct State {
    hive_file: String,
    hive: OnceLock<(usize, KeyPaths)>,
    base: KeyPaths,
    deleted: Mutex<HashSet<String>>,
    deleted_file: String,
}

static STATE: OnceLock<State> = OnceLock::new();

unsafe fn original<F: Copy>(slot: &AtomicUsize) -> F {
    let address = slot.load(Ordering::Relaxed);
    unsafe { std::mem::transmute_copy(&address) }
}

fn current_user_sid() -> Result<String, String> {
    let ntdll: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
    let mut name = UnicodeString { length: 0, maximum_length: 0, buffer: std::ptr::null_mut() };
    unsafe {
        let module = GetModuleHandleW(ntdll.as_ptr());
        let format = GetProcAddress(module, c"RtlFormatCurrentUserKeyPath".as_ptr().cast()).ok_or("ntdll has no RtlFormatCurrentUserKeyPath")?;
        let free = GetProcAddress(module, c"RtlFreeUnicodeString".as_ptr().cast()).ok_or("ntdll has no RtlFreeUnicodeString")?;
        let format: unsafe extern "system" fn(*mut UnicodeString) -> Status = std::mem::transmute(format);
        let free: unsafe extern "system" fn(*mut UnicodeString) = std::mem::transmute(free);
        let status = format(&mut name);
        if status < 0 {
            return Err(format!("cannot find the current user's registry key: {status:#x}"));
        }
        let text = text_of(&name);
        free(&mut name);
        text.and_then(|path| path.rsplit('\\').next().map(str::to_owned)).ok_or_else(|| "the current user's registry key has no name".to_string())
    }
}

pub fn install(patcher: &mut Patcher, environment: &Path) -> Result<(), String> {
    let base = KeyPaths { user_sid: current_user_sid()?, hive_root: String::new(), is_32_bit: cfg!(target_pointer_width = "32") };
    let deleted_file = environment.join("deleted-keys.txt").to_string_lossy().into_owned();
    let deleted = fs::read_to_string(&deleted_file).unwrap_or_default().lines().map(str::to_owned).collect();
    let hive_file = environment.join("registry.hive").to_string_lossy().into_owned();
    STATE.set(State { hive_file, hive: OnceLock::new(), base, deleted: Mutex::new(deleted), deleted_file }).map_err(|_| "the environment is already active")?;

    ORIG_OPEN_KEY.store(patcher.hook("NtOpenKey", nt_open_key as *const () as usize)?, Ordering::Relaxed);
    ORIG_OPEN_KEY_EX.store(patcher.hook("NtOpenKeyEx", nt_open_key_ex as *const () as usize)?, Ordering::Relaxed);
    ORIG_CREATE_KEY.store(patcher.hook("NtCreateKey", nt_create_key as *const () as usize)?, Ordering::Relaxed);
    ORIG_DELETE_KEY.store(patcher.hook("NtDeleteKey", nt_delete_key as *const () as usize)?, Ordering::Relaxed);
    ORIG_ENUMERATE_KEY.store(patcher.hook("NtEnumerateKey", nt_enumerate_key as *const () as usize)?, Ordering::Relaxed);
    ORIG_QUERY_KEY.store(patcher.hook("NtQueryKey", nt_query_key as *const () as usize)?, Ordering::Relaxed);
    let ntdll: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
    let (module, get) = unsafe { (GetModuleHandleW(ntdll.as_ptr()), GetProcAddress) };
    for (slot, name) in [(&ORIG_ENUMERATE_VALUE, c"NtEnumerateValueKey"), (&ORIG_SET_VALUE, c"NtSetValueKey"), (&ORIG_CLOSE, c"NtClose")] {
        let address = unsafe { get(module, name.as_ptr().cast()) }.ok_or("ntdll lacks a registry function")?;
        slot.store(address as usize, Ordering::Relaxed);
    }
    Ok(())
}

struct OwnedKey(Handle);

impl Drop for OwnedKey {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { original::<Close>(&ORIG_CLOSE)(self.0) };
        }
    }
}

fn name_of_key(key: Handle) -> Option<String> {
    let mut buffer = vec![0u32; 512];
    let mut returned = 0;
    let status = unsafe { original::<QueryKey>(&ORIG_QUERY_KEY)(key, KEY_NAME_INFORMATION, buffer.as_mut_ptr().cast(), (buffer.len() * 4) as u32, &mut returned) };
    if status < 0 {
        return None;
    }
    let bytes = buffer[0] as usize;
    let wide = buffer.as_ptr().wrapping_add(1).cast::<u16>();
    String::from_utf16(unsafe { std::slice::from_raw_parts(wide, bytes / 2) }).ok()
}

impl State {
    fn hive(&self) -> Option<&(usize, KeyPaths)> {
        if let Some(hive) = self.hive.get() {
            return Some(hive);
        }
        let wide: Vec<u16> = self.hive_file.encode_utf16().chain(Some(0)).collect();
        let mut root = std::ptr::null_mut();
        let error = unsafe { reg_load_app_key(wide.as_ptr(), &mut root, KEY_ALL_ACCESS, 0, 0) };
        if error != ERROR_SUCCESS || root.is_null() {
            eprintln!("vault-env: cannot load {}: error {error}", self.hive_file);
            return None;
        }
        let paths = KeyPaths { hive_root: name_of_key(root)?, ..self.base.clone() };
        Some(self.hive.get_or_init(|| (root as usize, paths)))
    }

    fn is_deleted(&self, virtual_path: &str) -> bool {
        self.deleted.lock().is_ok_and(|deleted| deleted.contains(&virtual_path.to_lowercase()))
    }

    fn remember_deleted(&self, virtual_path: &str) {
        let key = virtual_path.to_lowercase();
        if let Ok(mut deleted) = self.deleted.lock() {
            if deleted.insert(key.clone()) {
                if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&self.deleted_file) {
                    let _ = writeln!(file, "{key}");
                }
            }
        }
    }
}

#[link(name = "advapi32")]
unsafe extern "system" {
    #[link_name = "RegLoadAppKeyW"]
    fn reg_load_app_key(file: *const u16, key: *mut Handle, access: u32, options: u32, reserved: u32) -> u32;
}

fn template() -> ObjectAttributes {
    ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: std::ptr::null_mut(),
        object_name: std::ptr::null_mut(),
        attributes: CASE_INSENSITIVE,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    }
}

fn open_at(root: Handle, name: &str, access: u32) -> Result<OwnedKey, Status> {
    let mut named = OwnedName::new(name, &template(), false);
    named.set_root(root);
    let mut key = std::ptr::null_mut();
    let status = unsafe { original::<OpenKey>(&ORIG_OPEN_KEY)(&mut key, access, named.attributes()) };
    if status < 0 { Err(status) } else { Ok(OwnedKey(key)) }
}

fn open_in_hive(root: usize, virtual_path: &str, access: u32) -> Result<OwnedKey, Status> {
    open_at(root as Handle, virtual_path, access)
}

fn open_real(paths: &KeyPaths, virtual_path: &str, access: u32) -> Result<OwnedKey, Status> {
    open_at(std::ptr::null_mut(), &paths.real_name(virtual_path), access | KEY_WOW64_64KEY)
}

fn copy_values(from: Handle, to: Handle) {
    let mut buffer = vec![0u64; 16 * 1024];
    for index in 0.. {
        let mut returned = 0;
        let status = unsafe {
            original::<EnumerateValue>(&ORIG_ENUMERATE_VALUE)(from, index, KEY_VALUE_FULL_INFORMATION, buffer.as_mut_ptr().cast(), (buffer.len() * 8) as u32, &mut returned)
        };
        if status == STATUS_BUFFER_OVERFLOW || status == STATUS_BUFFER_TOO_SMALL {
            buffer.resize((returned as usize).div_ceil(8) + 1, 0);
            let retried = unsafe {
                original::<EnumerateValue>(&ORIG_ENUMERATE_VALUE)(from, index, KEY_VALUE_FULL_INFORMATION, buffer.as_mut_ptr().cast(), (buffer.len() * 8) as u32, &mut returned)
            };
            if retried < 0 {
                return;
            }
        } else if status < 0 {
            return;
        }
        let words = buffer.as_ptr().cast::<u32>();
        unsafe {
            let (value_type, data_offset, data_length, name_length) = (*words.add(1), *words.add(2) as usize, *words.add(3), *words.add(4));
            let mut name = UnicodeString { length: name_length as u16, maximum_length: name_length as u16, buffer: words.add(5).cast::<u16>().cast_mut() };
            original::<SetValue>(&ORIG_SET_VALUE)(to, &mut name, 0, value_type, buffer.as_ptr().cast::<u8>().add(data_offset), data_length);
        }
    }
}

fn make_chain(state: &State, root: usize, paths: &KeyPaths, virtual_path: &str) -> Result<bool, Status> {
    let mut parent = OwnedKey(std::ptr::null_mut());
    let mut so_far = String::new();
    let mut existed = false;
    for segment in virtual_path.split('\\').filter(|segment| !segment.is_empty()) {
        if !so_far.is_empty() {
            so_far.push('\\');
        }
        so_far.push_str(segment);
        let mut name = OwnedName::new(segment, &template(), false);
        name.set_root(if parent.0.is_null() { root as Handle } else { parent.0 });
        let mut key = std::ptr::null_mut();
        let mut disposition = 0;
        let status = unsafe { original::<CreateKey>(&ORIG_CREATE_KEY)(&mut key, KEY_ALL_ACCESS, name.attributes(), 0, std::ptr::null_mut(), 0, &mut disposition) };
        if status < 0 {
            return Err(status);
        }
        let key = OwnedKey(key);
        existed = disposition == OPENED_EXISTING_KEY;
        if !existed && !state.is_deleted(&so_far) {
            if let Ok(real) = open_real(paths, &so_far, KEY_READ) {
                copy_values(real.0, key.0);
                existed = true;
            }
        }
        parent = key;
    }
    Ok(existed)
}

unsafe fn virtual_of_attributes(paths: &KeyPaths, attributes: *const ObjectAttributes) -> Option<String> {
    let (root, name) = unsafe {
        let attributes = attributes.as_ref()?;
        (attributes.root_directory, text_of(attributes.object_name).unwrap_or_default())
    };
    if root.is_null() {
        return paths.virtual_of(&name);
    }
    let folder = name_of_key(root)?;
    if name.is_empty() {
        return paths.virtual_of(&folder);
    }
    paths.virtual_of(&format!("{folder}\\{name}"))
}

fn in_hive(virtual_path: &str, root: usize, caller: &ObjectAttributes) -> Box<OwnedName> {
    let mut name = OwnedName::new(virtual_path, caller, false);
    name.set_root(root as Handle);
    name.add_attributes(CASE_INSENSITIVE);
    name
}

unsafe fn open_key(access: u32, attributes: *mut ObjectAttributes, call: impl Fn(u32, *mut ObjectAttributes) -> Status) -> Status {
    let plain = || call(access, attributes);
    let (Some(state), Some(_own_work)) = (STATE.get(), OwnWork::enter()) else { return plain() };
    let Some((root, paths)) = state.hive() else { return plain() };
    let Some(as_named) = (unsafe { virtual_of_attributes(paths, attributes) }) else { return plain() };
    let virtual_path = paths.with_view(as_named.clone(), access);
    let writes = access & WRITE_ACCESS != 0;
    let in_file = open_in_hive(*root, &virtual_path, KEY_READ).is_ok();
    if !in_file {
        if state.is_deleted(&virtual_path) {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
        if !writes {
            let mut real = OwnedName::new(&paths.real_name(&as_named), unsafe { &*attributes }, true);
            real.add_attributes(CASE_INSENSITIVE);
            return call(access, real.attributes());
        }
        if let Err(status) = open_real(paths, &virtual_path, KEY_READ) {
            return status;
        }
        if let Err(status) = make_chain(state, *root, paths, &virtual_path) {
            return status;
        }
    }
    let mut name = in_hive(&virtual_path, *root, unsafe { &*attributes });
    call(access & !VIEW_FLAGS, name.attributes())
}

unsafe extern "system" fn nt_open_key(handle: *mut Handle, access: u32, attributes: *mut ObjectAttributes) -> Status {
    unsafe { open_key(access, attributes, |access, attributes| original::<OpenKey>(&ORIG_OPEN_KEY)(handle, access, attributes)) }
}

unsafe extern "system" fn nt_open_key_ex(handle: *mut Handle, access: u32, attributes: *mut ObjectAttributes, options: u32) -> Status {
    unsafe { open_key(access, attributes, |access, attributes| original::<OpenKeyEx>(&ORIG_OPEN_KEY_EX)(handle, access, attributes, options)) }
}

unsafe extern "system" fn nt_create_key(handle: *mut Handle, access: u32, attributes: *mut ObjectAttributes, title: u32, class: *mut UnicodeString, options: u32, disposition: *mut u32) -> Status {
    let call = |access, attributes, disposition| unsafe { original::<CreateKey>(&ORIG_CREATE_KEY)(handle, access, attributes, title, class, options, disposition) };
    let (Some(state), Some(_own_work)) = (STATE.get(), OwnWork::enter()) else { return call(access, attributes, disposition) };
    let Some((root, paths)) = state.hive() else { return call(access, attributes, disposition) };
    let Some(virtual_path) = (unsafe { virtual_of_attributes(paths, attributes) }) else { return call(access, attributes, disposition) };
    let virtual_path = paths.with_view(virtual_path, access);
    let existed = match make_chain(state, *root, paths, &virtual_path) {
        Ok(existed) => existed,
        Err(status) => return status,
    };
    let mut name = in_hive(&virtual_path, *root, unsafe { &*attributes });
    let status = call(access & !VIEW_FLAGS, name.attributes(), std::ptr::null_mut());
    if status >= 0 {
        if let Some(disposition) = unsafe { disposition.as_mut() } {
            *disposition = if existed { OPENED_EXISTING_KEY } else { CREATED_NEW_KEY };
        }
    }
    status
}

unsafe extern "system" fn nt_delete_key(key: Handle) -> Status {
    let call = || unsafe { original::<DeleteKey>(&ORIG_DELETE_KEY)(key) };
    let (Some(state), Some(_own_work)) = (STATE.get(), OwnWork::enter()) else { return call() };
    let Some((_, paths)) = state.hive() else { return call() };
    if let Some(virtual_path) = name_of_key(key).and_then(|name| paths.virtual_of(&name)) {
        if open_real(paths, &virtual_path, KEY_READ).is_ok() {
            state.remember_deleted(&virtual_path);
        }
    }
    call()
}

fn real_twin(state: &State, key: Handle) -> Option<(OwnedKey, String)> {
    let (_, paths) = state.hive()?;
    let name = name_of_key(key)?;
    if !name.to_lowercase().starts_with(&paths.hive_root.to_lowercase()) {
        return None;
    }
    let virtual_path = paths.virtual_of(&name)?;
    if virtual_path.is_empty() {
        return None;
    }
    Some((open_real(paths, &virtual_path, KEY_READ).ok()?, virtual_path))
}

fn subkey_name(key: Handle, index: u32) -> Result<String, Status> {
    let mut buffer = vec![0u64; 128];
    let mut returned = 0;
    let status = unsafe { original::<EnumerateKey>(&ORIG_ENUMERATE_KEY)(key, index, KEY_BASIC_INFORMATION, buffer.as_mut_ptr().cast(), (buffer.len() * 8) as u32, &mut returned) };
    if status < 0 {
        return Err(status);
    }
    let words = buffer.as_ptr().cast::<u32>();
    unsafe { Ok(String::from_utf16_lossy(std::slice::from_raw_parts(words.add(4).cast::<u16>(), (*words.add(3) / 2) as usize))) }
}

fn real_only_subkeys(state: &State, key: Handle, real: Handle, virtual_path: &str) -> Vec<u32> {
    let mut extra = Vec::new();
    for index in 0.. {
        let Ok(name) = subkey_name(real, index) else { break };
        if open_at(key, &name, KEY_READ).is_err() && !state.is_deleted(&format!("{virtual_path}\\{name}")) {
            extra.push(index);
        }
    }
    extra
}

fn own_subkey_count(key: Handle) -> u32 {
    let mut buffer = [0u32; 64];
    let mut returned = 0;
    let status = unsafe { original::<QueryKey>(&ORIG_QUERY_KEY)(key, KEY_FULL_INFORMATION, buffer.as_mut_ptr().cast(), 256, &mut returned) };
    if status < 0 { 0 } else { buffer[5] }
}

unsafe extern "system" fn nt_enumerate_key(key: Handle, index: u32, class: u32, buffer: *mut u8, length: u32, returned: *mut u32) -> Status {
    let call = |target, index| unsafe { original::<EnumerateKey>(&ORIG_ENUMERATE_KEY)(target, index, class, buffer, length, returned) };
    let (Some(state), Some(_own_work)) = (STATE.get(), OwnWork::enter()) else { return call(key, index) };
    let own = own_subkey_count(key);
    if index < own {
        return call(key, index);
    }
    let Some((real, virtual_path)) = real_twin(state, key) else { return call(key, index) };
    match real_only_subkeys(state, key, real.0, &virtual_path).get((index - own) as usize) {
        Some(real_index) => call(real.0, *real_index),
        None => STATUS_NO_MORE_ENTRIES,
    }
}

unsafe extern "system" fn nt_query_key(key: Handle, class: u32, buffer: *mut u8, length: u32, returned: *mut u32) -> Status {
    let status = unsafe { original::<QueryKey>(&ORIG_QUERY_KEY)(key, class, buffer, length, returned) };
    if status < 0 || !matches!(class, KEY_FULL_INFORMATION | KEY_CACHED_INFORMATION) {
        return status;
    }
    let (Some(state), Some(_own_work)) = (STATE.get(), OwnWork::enter()) else { return status };
    let Some((real, virtual_path)) = real_twin(state, key) else { return status };
    let extra = real_only_subkeys(state, key, real.0, &virtual_path);
    let mut real_info = [0u32; 64];
    let mut real_returned = 0;
    let real_status = unsafe { original::<QueryKey>(&ORIG_QUERY_KEY)(real.0, class, real_info.as_mut_ptr().cast(), 256, &mut real_returned) };
    let (count_at, longest_at) = if class == KEY_FULL_INFORMATION { (5, 6) } else { (3, 4) };
    unsafe {
        let words = buffer.cast::<u32>();
        words.add(count_at).write_unaligned(words.add(count_at).read_unaligned() + extra.len() as u32);
        if real_status >= 0 {
            let longest = words.add(longest_at).read_unaligned().max(real_info[longest_at]);
            words.add(longest_at).write_unaligned(longest);
        }
    }
    status
}

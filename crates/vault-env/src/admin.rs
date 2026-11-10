use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::hook::Patcher;
use crate::nt::{Handle, Status};

const TOKEN_ELEVATION_TYPE: u32 = 18;
const TOKEN_ELEVATION: u32 = 20;
const TOKEN_ELEVATION_TYPE_FULL: u32 = 2;
const ADMINISTRATORS_SID: [u8; 16] = [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x20, 0x02, 0, 0];

static ORIG_QUERY_TOKEN: AtomicUsize = AtomicUsize::new(0);
static RTL_CHECK_MEMBERSHIP: OnceLock<RtlCheck> = OnceLock::new();

type QueryToken = unsafe extern "system" fn(Handle, u32, *mut u8, u32, *mut u32) -> Status;
type RtlCheck = unsafe extern "system" fn(Handle, *const c_void, u32, *mut u8) -> Status;

pub fn install(patcher: &mut Patcher) -> Result<(), String> {
    let ntdll: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
    let check = unsafe {
        let check = GetProcAddress(GetModuleHandleW(ntdll.as_ptr()), c"RtlCheckTokenMembershipEx".as_ptr().cast()).ok_or("ntdll has no RtlCheckTokenMembershipEx")?;
        std::mem::transmute::<unsafe extern "system" fn() -> isize, RtlCheck>(check)
    };
    RTL_CHECK_MEMBERSHIP.set(check).map_err(|_| "the environment is already active")?;
    ORIG_QUERY_TOKEN.store(patcher.hook("NtQueryInformationToken", nt_query_information_token as *const () as usize)?, Ordering::Relaxed);
    patcher.replace("kernelbase.dll", "CheckTokenMembership", check_token_membership as *const () as usize)
}

unsafe extern "system" fn check_token_membership(token: Handle, sid: *const c_void, is_member: *mut i32) -> i32 {
    unsafe {
        let looks_like_ours = sid.cast::<u8>().add(1).read() == 2;
        if looks_like_ours && std::slice::from_raw_parts(sid.cast::<u8>(), ADMINISTRATORS_SID.len()) == ADMINISTRATORS_SID {
            *is_member = 1;
            return 1;
        }
        let Some(check) = RTL_CHECK_MEMBERSHIP.get() else { return 0 };
        let mut member = 0;
        if check(token, sid, 0, &mut member) < 0 {
            return 0;
        }
        *is_member = i32::from(member != 0);
        1
    }
}

unsafe extern "system" fn nt_query_information_token(token: Handle, class: u32, information: *mut u8, length: u32, returned: *mut u32) -> Status {
    let status = unsafe {
        let query: QueryToken = std::mem::transmute(ORIG_QUERY_TOKEN.load(Ordering::Relaxed));
        query(token, class, information, length, returned)
    };
    if status >= 0 {
        unsafe {
            match class {
                TOKEN_ELEVATION => information.cast::<u32>().write_unaligned(1),
                TOKEN_ELEVATION_TYPE => information.cast::<u32>().write_unaligned(TOKEN_ELEVATION_TYPE_FULL),
                _ => {}
            }
        }
    }
    status
}

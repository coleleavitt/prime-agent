//! Win32 security bindings for the named-pipe transport: the current user's SID, the owner-only
//! security descriptor new pipe instances are created with, and the pipe-owner check clients run
//! before trusting a server. Hand-declared like the crate's other Win32 bindings (no `windows-sys`
//! dependency); the policy itself is `pipe_security`.

use std::ffi::c_void;
use std::io;
use std::sync::OnceLock;

use super::pipe_security::{ensure_pipe_owner, owner_only_pipe_sddl};

type Handle = *mut c_void;
type Psid = *mut c_void;

/// `winnt.h` `TOKEN_QUERY`.
const TOKEN_QUERY: u32 = 0x0008;
/// `TOKEN_INFORMATION_CLASS::TokenUser`.
const TOKEN_USER_CLASS: u32 = 1;
/// `sddl.h` `SDDL_REVISION_1`.
const SDDL_REVISION_1: u32 = 1;
/// `SE_OBJECT_TYPE::SE_KERNEL_OBJECT` (named pipes).
const SE_KERNEL_OBJECT: u32 = 6;
/// `winnt.h` `OWNER_SECURITY_INFORMATION`.
const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;

/// `winnt.h` `SID_AND_ATTRIBUTES` (the head of `TOKEN_USER`).
#[repr(C)]
struct SidAndAttributes {
    sid: Psid,
    attributes: u32,
}

/// `minwinbase.h` `SECURITY_ATTRIBUTES`.
#[repr(C)]
struct SecurityAttributes {
    length: u32,
    security_descriptor: *mut c_void,
    inherit_handle: i32,
}

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: Handle, desired_access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        information: *mut c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: Psid, string_sid: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_descriptor: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        descriptor_size: *mut u32,
    ) -> i32;
    fn GetSecurityInfo(
        handle: Handle,
        object_type: u32,
        security_info: u32,
        owner: *mut Psid,
        group: *mut Psid,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        descriptor: *mut *mut c_void,
    ) -> u32;
}

extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}

/// The string form (`S-1-5-21-...`) of a SID.
fn sid_to_string(sid: Psid) -> io::Result<String> {
    let mut wide: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` is a valid SID owned by the caller's buffer; the out pointer is a local.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `wide` is a NUL-terminated LocalAlloc'd UTF-16 string, freed below.
    let text = unsafe {
        let mut len = 0;
        while *wide.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(wide, len))
    };
    // SAFETY: `wide` came from LocalAlloc inside ConvertSidToStringSidW.
    unsafe { LocalFree(wide.cast()) };
    Ok(text)
}

fn query_current_user_sid() -> io::Result<String> {
    let mut token: Handle = std::ptr::null_mut();
    // SAFETY: the pseudo-handle needs no closing; `token` is a local out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut needed = 0u32;
    // SAFETY: a size probe with a null buffer; only `needed` is written.
    unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
        )
    };
    // u64 cells keep the TOKEN_USER pointer field aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(2)];
    let capacity = u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX);
    // SAFETY: `buffer` holds `capacity` writable bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buffer.as_mut_ptr().cast(),
            capacity,
            &raw mut needed,
        )
    };
    let result = if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: on success the buffer starts with a TOKEN_USER whose SID points into it.
        let user = unsafe { &*buffer.as_ptr().cast::<SidAndAttributes>() };
        sid_to_string(user.sid)
    };
    // SAFETY: `token` was opened above and is closed exactly once.
    unsafe { CloseHandle(token) };
    result
}

/// The SID of the user this process runs as (memoized: it cannot change for a process).
///
/// # Errors
///
/// The token query's OS error.
pub fn current_user_sid() -> io::Result<String> {
    static SID: OnceLock<String> = OnceLock::new();
    if let Some(sid) = SID.get() {
        return Ok(sid.clone());
    }
    let sid = query_current_user_sid()?;
    Ok(SID.get_or_init(|| sid).clone())
}

/// An owner-only security descriptor (see [`owner_only_pipe_sddl`]) wrapped in the
/// `SECURITY_ATTRIBUTES` pipe creation takes; frees the descriptor on drop.
pub(crate) struct OwnerOnlySecurity {
    attributes: SecurityAttributes,
}

impl OwnerOnlySecurity {
    pub(crate) fn for_current_user() -> io::Result<Self> {
        let sddl: Vec<u16> = owner_only_pipe_sddl(&current_user_sid()?)
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor: *mut c_void = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; the descriptor is LocalAlloc'd and owned by `Self`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            attributes: SecurityAttributes {
                length: u32::try_from(std::mem::size_of::<SecurityAttributes>())
                    .expect("SECURITY_ATTRIBUTES fits u32"),
                security_descriptor: descriptor,
                inherit_handle: 0,
            },
        })
    }

    /// The `SECURITY_ATTRIBUTES` pointer for `CreateNamedPipeW`, valid while `self` lives.
    pub(crate) fn as_raw(&mut self) -> *mut c_void {
        (&raw mut self.attributes).cast()
    }
}

impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        // SAFETY: the descriptor was LocalAlloc'd by the SDDL conversion and is freed once.
        unsafe { LocalFree(self.attributes.security_descriptor) };
    }
}

/// Refuse a connected pipe whose owner is not this process's user.
///
/// # Errors
///
/// The security query's OS error, or `PermissionDenied` for a foreign owner.
pub(crate) fn verify_pipe_owner(handle: Handle) -> io::Result<()> {
    let mut owner: Psid = std::ptr::null_mut();
    let mut descriptor: *mut c_void = std::ptr::null_mut();
    // SAFETY: `handle` is a live pipe handle opened with read access (READ_CONTROL); the owner
    // SID points into `descriptor`, which is freed below.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(
            i32::try_from(status).unwrap_or(i32::MAX),
        ));
    }
    let owner_sid = sid_to_string(owner);
    // SAFETY: `descriptor` was allocated by GetSecurityInfo and is freed once.
    unsafe { LocalFree(descriptor) };
    ensure_pipe_owner(&owner_sid?, &current_user_sid()?)
}

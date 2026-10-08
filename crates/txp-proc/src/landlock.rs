//! Minimal raw Landlock binding (no crate dependency, so the pre-exec child
//! only calls async-signal-safe syscalls).

use std::ffi::CStr;
use std::io;

const SYS_CREATE_RULESET: libc::c_long = 444;
const SYS_ADD_RULE: libc::c_long = 445;
const SYS_RESTRICT_SELF: libc::c_long = 446;
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

/// Execute a file.
pub const FS_EXECUTE: u64 = 1 << 0;
/// Open a file for writing.
pub const FS_WRITE_FILE: u64 = 1 << 1;
/// Open a file for reading.
pub const FS_READ_FILE: u64 = 1 << 2;
/// List a directory.
pub const FS_READ_DIR: u64 = 1 << 3;
/// Remove an empty directory or rename one away.
pub const FS_REMOVE_DIR: u64 = 1 << 4;
/// Unlink a file or rename one away.
pub const FS_REMOVE_FILE: u64 = 1 << 5;
/// Create a character device.
pub const FS_MAKE_CHAR: u64 = 1 << 6;
/// Create a directory.
pub const FS_MAKE_DIR: u64 = 1 << 7;
/// Create a regular file.
pub const FS_MAKE_REG: u64 = 1 << 8;
/// Create a Unix socket.
pub const FS_MAKE_SOCK: u64 = 1 << 9;
/// Create a named pipe.
pub const FS_MAKE_FIFO: u64 = 1 << 10;
/// Create a block device.
pub const FS_MAKE_BLOCK: u64 = 1 << 11;
/// Create a symbolic link.
pub const FS_MAKE_SYM: u64 = 1 << 12;
/// Link or rename across directories (ABI 2).
pub const FS_REFER: u64 = 1 << 13; // ABI 2
/// Truncate a file (ABI 3).
pub const FS_TRUNCATE: u64 = 1 << 14; // ABI 3
/// Issue `ioctl` on a device file (ABI 5).
pub const FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Landlock ABI version supported by the running kernel, or <= 0 if absent.
pub fn abi_version() -> i32 {
    let r = unsafe { libc::syscall(SYS_CREATE_RULESET, std::ptr::null::<u8>(), 0usize, LANDLOCK_CREATE_RULESET_VERSION) };
    r as i32
}

/// All filesystem access rights the given ABI knows about.
pub fn fs_rights_for_abi(abi: i32) -> u64 {
    let mut r = FS_EXECUTE
        | FS_WRITE_FILE
        | FS_READ_FILE
        | FS_READ_DIR
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_CHAR
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_MAKE_SOCK
        | FS_MAKE_FIFO
        | FS_MAKE_BLOCK
        | FS_MAKE_SYM;
    if abi >= 2 {
        r |= FS_REFER;
    }
    if abi >= 3 {
        r |= FS_TRUNCATE;
    }
    if abi >= 5 {
        r |= FS_IOCTL_DEV;
    }
    r
}

/// Rights that allow reading and executing but no modification.
pub fn read_only_rights() -> u64 {
    FS_EXECUTE | FS_READ_FILE | FS_READ_DIR
}

/// A rule: `path` (NUL-terminated) gets `allowed` rights.
pub struct Rule<'a> {
    /// Directory (or file) the rule applies beneath.
    pub path: &'a CStr,
    /// Bitmask of `FS_*` rights granted under `path`.
    pub allowed: u64,
}

/// Apply the ruleset to the calling thread. Async-signal-safe: only
/// `open`, `syscall`, `close`, `prctl`.
///
/// # Safety
/// Intended to be called in a forked child before `exec`; the paths must be
/// valid NUL-terminated strings for the lifetime of the call.
pub unsafe fn restrict_self(abi: i32, rules: &[Rule<'_>]) -> io::Result<()> {
    let handled = fs_rights_for_abi(abi);
    let attr = RulesetAttr { handled_access_fs: handled };
    let fd = unsafe { libc::syscall(SYS_CREATE_RULESET, &attr as *const _, std::mem::size_of::<RulesetAttr>(), 0u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = fd as i32;
    for r in rules {
        let pfd = unsafe { libc::open(r.path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if pfd < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let pa = PathBeneathAttr { allowed_access: r.allowed & handled, parent_fd: pfd };
        let rc = unsafe { libc::syscall(SYS_ADD_RULE, fd, LANDLOCK_RULE_PATH_BENEATH, &pa as *const _, 0u32) };
        let e = io::Error::last_os_error();
        unsafe { libc::close(pfd) };
        if rc != 0 {
            unsafe { libc::close(fd) };
            return Err(e);
        }
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let rc = unsafe { libc::syscall(SYS_RESTRICT_SELF, fd, 0u32) };
    let e = io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 { Err(e) } else { Ok(()) }
}

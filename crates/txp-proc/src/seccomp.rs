//! A seccomp-bpf syscall denylist for sandboxed steps.
//!
//! This is defence in depth, additive to the mount/pid/net namespaces, the
//! Landlock filesystem ruleset, `no_new_privs` and the unprivileged uid. The
//! confined process already cannot gain privilege; the filter makes the
//! kernel attack surface it can reach explicit by refusing a fixed set of
//! administrative and exploit-primitive syscalls with `EPERM`.
//!
//! A denylist (default-allow) rather than an allowlist is deliberate: an
//! allowlist that misses one syscall breaks ordinary build tools, whereas the
//! denied calls here are never issued by a normal `process` step.
//!
//! The program is a classic cBPF filter built once in the parent (so the
//! post-`fork` child only performs async-signal-safe syscalls) and installed
//! with `seccomp(SECCOMP_SET_MODE_FILTER)`, falling back to the older
//! `prctl(PR_SET_SECCOMP)`. Syscall numbers come from `libc::SYS_*`, which is
//! correct for the target architecture, so the same source compiles to the
//! right numbers on x86_64 and aarch64.

use std::io;

// BPF instruction classes / addressing modes (uapi/linux/bpf_common.h).
const BPF_LD: u16 = 0x00;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;

// seccomp return actions and filter mode (uapi/linux/seccomp.h).
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_DATA: u32 = 0x0000_ffff;
const SECCOMP_SET_MODE_FILTER: u32 = 1;
const SECCOMP_MODE_FILTER: libc::c_int = 2; // prctl(PR_SET_SECCOMP, ...) fallback

// Offsets into `struct seccomp_data` (uapi/linux/seccomp.h).
const SECCOMP_DATA_NR_OFFSET: u32 = 0;
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;

// AUDIT_ARCH_* for the architectures we support (linux/audit.h).
#[cfg(target_arch = "x86_64")]
const TARGET_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const TARGET_ARCH: u32 = 0xc000_00b7;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}
fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// Syscalls a `process` step is never expected to make and that are classic
/// escape or tamper primitives. Each is cap-gated or namespace-sensitive; the
/// filter denies them regardless of the running uid's capabilities.
fn denied() -> Vec<libc::c_long> {
    vec![
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_bpf,
        libc::SYS_ptrace,
        libc::SYS_perf_event_open,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_quotactl,
        libc::SYS_clock_settime,
        libc::SYS_clock_adjtime,
        libc::SYS_adjtimex,
    ]
}

/// Build the filter program. Layout: check the architecture, load the syscall
/// number, compare it against each denied number (jump to the deny leaf on a
/// match), otherwise allow.
fn program() -> Vec<libc::sock_filter> {
    let nrs = denied();
    let n = nrs.len() as u8;
    let mut p = Vec::with_capacity(nrs.len() + 5);

    // If the architecture is not the one these syscall numbers belong to,
    // refuse everything rather than misinterpret the numbers.
    p.push(stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARCH_OFFSET));
    // jt skips the kill leaf (next-but-one) when the arch matches.
    p.push(jump(BPF_JMP | BPF_JEQ | BPF_K, TARGET_ARCH, 1, 0));
    p.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (libc::EPERM as u32 & SECCOMP_RET_DATA)));

    // Load the syscall number.
    p.push(stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET));
    // Compare against each denied number. For the i-th comparison the deny leaf
    // sits at index (len-1); a match jumps forward to it, a miss falls through.
    for (i, nr) in nrs.iter().enumerate() {
        let remaining = n - 1 - i as u8; // comparisons after this one
        let jt = remaining + 1; // skip the rest, land on the deny leaf
        p.push(jump(BPF_JMP | BPF_JEQ | BPF_K, *nr as u32, jt, 0));
    }
    // Fall-through: allowed.
    p.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    // Deny leaf.
    p.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (libc::EPERM as u32 & SECCOMP_RET_DATA)));
    p
}

/// A compiled filter, ready to install in the child. Holding the program in a
/// `Vec` built before `fork` keeps the child allocation-free.
pub struct Filter {
    prog: Vec<libc::sock_filter>,
}

impl Filter {
    /// Compile the denylist.
    pub fn compile() -> Filter {
        Filter { prog: program() }
    }

    /// Install the filter on the calling thread. Async-signal-safe: it only
    /// reads the pre-built program and issues syscalls. Requires
    /// `no_new_privs` to already be set (Landlock sets it; the non-Landlock
    /// path sets it explicitly). Returns an error if the kernel cannot apply a
    /// filter, so the caller can fail closed.
    ///
    /// # Safety
    /// Must run after `no_new_privs` is set and before `exec`, in the process
    /// being confined.
    pub unsafe fn install(&self) -> io::Result<()> {
        let fprog = libc::sock_fprog {
            len: self.prog.len() as u16,
            filter: self.prog.as_ptr() as *mut libc::sock_filter,
        };
        let fp = &fprog as *const libc::sock_fprog;
        // Prefer the seccomp() syscall; fall back to prctl() on kernels that
        // predate it (both need no_new_privs for an unprivileged caller).
        let r = unsafe { libc::syscall(libc::SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, fp) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ENOSYS) {
            if unsafe { libc::prctl(libc::PR_SET_SECCOMP, SECCOMP_MODE_FILTER, fp, 0, 0) } == 0 {
                return Ok(());
            }
            return Err(io::Error::last_os_error());
        }
        Err(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_is_well_formed() {
        let p = program();
        // arch load + arch check + arch-deny + nr load + one jump per denied
        // syscall + allow leaf + deny leaf.
        assert_eq!(p.len(), denied().len() + 6);
        // Last two leaves are returns.
        assert_eq!(p[p.len() - 2].code, BPF_RET | BPF_K);
        assert_eq!(p[p.len() - 1].code, BPF_RET | BPF_K);
        // No comparison jumps past the end of the program.
        for (i, insn) in p.iter().enumerate() {
            if insn.code == (BPF_JMP | BPF_JEQ | BPF_K) {
                assert!(i + 1 + insn.jt as usize <= p.len() - 1, "jump out of range at {i}");
            }
        }
    }
}

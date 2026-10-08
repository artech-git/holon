//! txp-proc: the OS-process participant (design §1.8).
//!
//! A step's process runs inside a private staged view and never touches the
//! real world until commit:
//! - new mount, PID, net (no interfaces ⇒ egress denied), IPC and UTS
//!   namespaces; a dedicated cgroup v2 leaf killed with `cgroup.kill`;
//! - each managed root is replaced by an overlayfs whose `lowerdir` is the
//!   real tree (plus the uppers of earlier steps in the same transaction) and
//!   whose `upperdir`/`workdir` live in the same-filesystem staging area;
//! - a Landlock ruleset makes everything outside the overlays, `/tmp` and
//!   `/dev` read-only (fail closed when the kernel cannot enforce it);
//! - the process is run as an unprivileged uid.
//!
//! Prepare translates the upperdir into a redo list; commit publishes it with
//! `txp-fs`'s idempotent machinery; abort is `cgroup.kill` + discard.
//!
//! Requires root (no user-namespace mode yet) and Linux ≥ 5.14.

#![warn(missing_docs)]

pub mod cgroup;
pub mod diff;
pub mod landlock;
pub mod participant;
pub mod sandbox;
pub mod seccomp;

pub use participant::{MountSpec, ProcConfig, ProcParticipant, RunAs};
pub use sandbox::{SandboxSpec, SandboxResult};

/// Startup self-test: what this host can enforce.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostCapabilities {
    /// Effective uid is 0 (required for namespaces and mounts).
    pub is_root: bool,
    /// A cgroup v2 hierarchy is mounted at `/sys/fs/cgroup`.
    pub cgroup_kill: bool,
    /// The kernel lists `overlay` in `/proc/filesystems`.
    pub overlayfs: bool,
    /// Landlock ABI version, or `<= 0` when unavailable.
    pub landlock_abi: i32,
}

/// Probe the host. Cheap; safe to call at every startup and from the
/// `self-test` command.
pub fn host_capabilities() -> HostCapabilities {
    HostCapabilities {
        is_root: unsafe { libc::geteuid() } == 0,
        cgroup_kill: std::path::Path::new("/sys/fs/cgroup/cgroup.kill").exists()
            || std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists(),
        overlayfs: std::fs::read_to_string("/proc/filesystems").map(|s| s.contains("overlay")).unwrap_or(false),
        landlock_abi: landlock::abi_version(),
    }
}

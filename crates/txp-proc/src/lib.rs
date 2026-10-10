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
//! - the process is run as an unprivileged uid, under a seccomp denylist.
//!
//! The confinement is set up by the `txp-sandbox` helper binary (see
//! [`sandbox`]), which must be installed next to the daemon.
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
    /// Landlock ABI version (capped at the newest one the sandbox uses), or
    /// `<= 0` when unavailable.
    pub landlock_abi: i32,
    /// Path of the `txp-sandbox` helper, if found.
    pub sandbox_helper: Option<std::path::PathBuf>,
}

impl HostCapabilities {
    /// What process steps will run into on this host, one line each.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if !self.is_root {
            w.push("not running as root: process steps will be refused (fs steps still work)".to_string());
        }
        if self.landlock_abi < 1 {
            w.push("Landlock unavailable: process steps will be refused (fail closed)".to_string());
        }
        if self.sandbox_helper.is_none() {
            w.push(format!("{} not found next to txpd (or via {}): process steps will fail", sandbox::HELPER_NAME, sandbox::HELPER_ENV));
        }
        w
    }
}

/// Probe the host. Cheap; safe to call at every startup and from the
/// `self-test` command.
pub fn host_capabilities() -> HostCapabilities {
    HostCapabilities {
        is_root: nix::unistd::geteuid().is_root(),
        cgroup_kill: std::path::Path::new("/sys/fs/cgroup/cgroup.kill").exists()
            || std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists(),
        overlayfs: std::fs::read_to_string("/proc/filesystems").map(|s| s.contains("overlay")).unwrap_or(false),
        landlock_abi: landlock::abi_version(),
        sandbox_helper: sandbox::helper_path(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_capabilities_describe_this_host() {
        let c = host_capabilities();
        assert_eq!(c.is_root, nix::unistd::geteuid().is_root());
        assert_eq!(c.landlock_abi, landlock::abi_version());
        assert_eq!(c.sandbox_helper, sandbox::helper_path());
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["overlayfs"], c.overlayfs);
        assert_eq!(v["cgroup_kill"], c.cgroup_kill);
    }

    #[test]
    fn warnings_name_each_missing_capability() {
        let ok = HostCapabilities { is_root: true, cgroup_kill: true, overlayfs: true, landlock_abi: 5, sandbox_helper: Some("/x".into()) };
        assert!(ok.warnings().is_empty());
        let bare = HostCapabilities { is_root: false, landlock_abi: 0, sandbox_helper: None, ..ok };
        let w = bare.warnings();
        assert_eq!(w.len(), 3);
        assert!(w[0].starts_with("not running as root") && w[1].starts_with("Landlock unavailable") && w[2].starts_with("txp-sandbox not found"), "{w:?}");
    }
}

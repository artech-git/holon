//! Landlock filesystem confinement, via the `landlock` crate.
//!
//! The sandbox handles every filesystem right up to [`TARGET_ABI`]. On an
//! older kernel the crate drops the rights it does not know (best effort),
//! but [`restrict_self`] fails closed when the kernel has no Landlock at all.

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr,
};
use std::io;
use std::path::Path;

/// Newest Landlock ABI whose filesystem rights the sandbox handles
/// (V5 adds `IoctlDev`).
pub const TARGET_ABI: ABI = ABI::V5;

/// Landlock ABI supported by the running kernel, capped at [`TARGET_ABI`];
/// `0` when Landlock is absent or disabled.
pub fn abi_version() -> i32 {
    // The crate keeps the kernel's version private, so ask for each ABI's
    // rights as a hard requirement, newest first. V4 added only network
    // rights, so those are part of the probe too.
    let supports = |abi: ABI| {
        let r = Ruleset::default().set_compatibility(CompatLevel::HardRequirement).handle_access(AccessFs::from_all(abi));
        let r = if abi >= ABI::V4 { r.and_then(|r| r.handle_access(AccessNet::from_all(abi))) } else { r };
        r.is_ok()
    };
    [ABI::V5, ABI::V4, ABI::V3, ABI::V2, ABI::V1].into_iter().find(|&abi| supports(abi)).map_or(0, |abi| abi as i32)
}

/// Every filesystem right the sandbox handles.
pub fn full_rights() -> BitFlags<AccessFs> {
    AccessFs::from_all(TARGET_ABI)
}

/// Rights that allow reading and executing but no modification.
pub fn read_only_rights() -> BitFlags<AccessFs> {
    AccessFs::from_read(TARGET_ABI)
}

/// Open and write existing files only (for `/dev`).
pub fn read_write_file_rights() -> BitFlags<AccessFs> {
    AccessFs::ReadFile | AccessFs::WriteFile
}

/// A rule: everything beneath `path` gets `allowed` rights.
pub struct Rule<'a> {
    /// Directory (or file) the rule applies beneath.
    pub path: &'a Path,
    /// Rights granted under `path`.
    pub allowed: BitFlags<AccessFs>,
}

/// Apply the ruleset to the calling process; this also sets `no_new_privs`
/// (without which an unprivileged caller's restriction fails). The ABI V1
/// rights are a hard requirement, so a kernel without Landlock is an error
/// rather than a silently unenforced ruleset; newer rights are best effort.
pub fn restrict_self(rules: &[Rule<'_>]) -> io::Result<()> {
    restrict_self_requiring(ABI::V1, rules)
}

/// [`restrict_self`], with the rights of `minimum` as the hard requirement.
fn restrict_self_requiring(minimum: ABI, rules: &[Rule<'_>]) -> io::Result<()> {
    let unavailable = |e| io::Error::other(format!("Landlock {minimum:?} is not available on this kernel ({e}); refusing to run unconfined"));
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(minimum))
        .map_err(unavailable)?
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(full_rights())
        .and_then(|r| r.create())
        .map_err(io::Error::other)?;
    for r in rules {
        let fd = PathFd::new(r.path).map_err(|e| io::Error::other(format!("{}: {e}", r.path.display())))?;
        ruleset = ruleset.add_rule(PathBeneath::new(fd, r.allowed)).map_err(io::Error::other)?;
    }
    ruleset.restrict_self().map(drop).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kernel_without_the_required_landlock_is_refused() {
        // A successful restriction would confine this very test binary, so
        // the attempt runs in a child process.
        if std::env::var_os("TXP_LANDLOCK_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "landlock::tests::a_kernel_without_the_required_landlock_is_refused", "--nocapture"])
                .env("TXP_LANDLOCK_CHILD", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            return;
        }
        // No kernel today has ABI 9; one that does would simply confine the child.
        let r = restrict_self_requiring(ABI::V9, &[]);
        let has_v9 = Ruleset::default().set_compatibility(CompatLevel::HardRequirement).handle_access(AccessFs::from_all(ABI::V9)).is_ok();
        assert!(has_v9 || r.is_err_and(|e| e.to_string().starts_with("Landlock V9 is not available on this kernel")));
    }

    #[test]
    fn a_rule_for_a_missing_path_fails_before_anything_is_restricted() {
        let r = restrict_self(&[Rule { path: Path::new("/nonexistent"), allowed: read_only_rights() }]);
        assert!(r.unwrap_err().to_string().starts_with("/nonexistent: "));
    }

    #[test]
    fn abi_and_rights() {
        assert!((0..=5).contains(&abi_version()));
        assert!(full_rights().contains(read_only_rights() | read_write_file_rights()));
    }
}

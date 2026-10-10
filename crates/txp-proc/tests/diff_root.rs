//! Root-only diff tests (run via `scripts/test-root.sh`): `trusted.overlay.*`
//! xattrs and whiteout devices need CAP_SYS_ADMIN / CAP_MKNOD to create.

use nix::sys::stat::{Mode, SFlag, makedev, mknod};
use std::path::Path;
use txp_fs::RedoOp;
use txp_proc::diff::translate;

fn upper() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::Builder::new().prefix("txp-diff-").tempdir_in("/var/tmp").unwrap();
    let u = d.path().join("upper");
    std::fs::create_dir(&u).unwrap();
    (d, u)
}

fn whiteout(p: &Path) {
    mknod(p, SFlag::S_IFCHR, Mode::empty(), makedev(0, 0)).unwrap();
}

#[test]
fn an_opaque_directory_is_published_whole_and_scrubbed() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let (d, u) = upper();
    let opq = u.join("opq");
    std::fs::create_dir(&opq).unwrap();
    xattr::set(&opq, "trusted.overlay.opaque", b"y").unwrap();
    whiteout(&opq.join("gone"));
    std::fs::write(opq.join("keep"), "k").unwrap();
    xattr::set(opq.join("keep"), "trusted.overlay.origin", b"o").unwrap();
    let ops = translate(&u, d.path()).unwrap();
    assert!(matches!(&ops[..], [RedoOp::SwapDir { staged, .. }] if staged == &opq), "{ops:?}");
    assert!(!opq.join("gone").exists(), "whiteouts inside are meaningless once published");
    assert_eq!(xattr::get(opq.join("keep"), "trusted.overlay.origin").unwrap(), None);
    assert_eq!(xattr::get(&opq, "trusted.overlay.opaque").unwrap(), None);
}

#[test]
fn renamed_directories_and_metacopy_files_are_refused() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let (d, u) = upper();
    std::fs::create_dir(u.join("moved")).unwrap();
    xattr::set(u.join("moved"), "trusted.overlay.redirect", b"/old").unwrap();
    assert!(translate(&u, d.path()).unwrap_err().contains("directory rename (redirect) is not supported"));

    let (d, u) = upper();
    std::fs::write(u.join("meta"), "").unwrap();
    xattr::set(u.join("meta"), "trusted.overlay.metacopy", b"").unwrap();
    assert!(translate(&u, d.path()).unwrap_err().contains("unsupported overlay feature trusted.overlay.metacopy"));
}

//! Root-only disk tests (run via `scripts/test-root.sh`): they mount
//! filesystems to reach behaviour a normal user cannot.

use nix::mount::{MsFlags, mount, umount};
use txp_wal::{Disk, RealDisk};

#[test]
fn preallocate_falls_back_to_extending_without_fallocate() {
    if !nix::unistd::geteuid().is_root() {
        eprintln!("skipped: not root");
        return;
    }
    // ramfs implements no fallocate, so the kernel answers EOPNOTSUPP.
    let d = tempfile::tempdir().unwrap();
    mount(Some("ramfs"), d.path(), Some("ramfs"), MsFlags::empty(), None::<&str>).unwrap();
    let mut h = RealDisk.create(&d.path().join("seg")).unwrap();
    let r = h.preallocate(1 << 16);
    let len = h.len();
    drop(h);
    umount(d.path()).unwrap();
    r.unwrap();
    assert_eq!(len.unwrap(), 1 << 16);
}

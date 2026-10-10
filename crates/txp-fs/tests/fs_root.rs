//! Root-only fs tests (run via `scripts/test-root.sh`): they mount
//! filesystems to reach failures a normal user cannot set up.

use nix::mount::{MsFlags, mount, umount};
use std::path::Path;
use txp_core::{ParticipantId, TxId};
use txp_fs::{DirAttrs, FsConfig, FsParticipant, RedoOp};
use txp_participant::{Participant, PartError, StepSpec, TxCtx};

fn root() -> bool {
    let root = nix::unistd::geteuid().is_root();
    if !root {
        eprintln!("skipped: not root");
    }
    root
}

fn tmpfs(at: &Path, flags: MsFlags) {
    mount(Some("tmpfs"), at, Some("tmpfs"), flags, None::<&str>).unwrap();
}

#[tokio::test]
async fn staging_must_share_the_roots_filesystem() {
    if !root() {
        return;
    }
    // A root that is itself a mount point has its staging sibling elsewhere.
    let d = tempfile::tempdir().unwrap();
    let site = d.path().join("site");
    std::fs::create_dir(&site).unwrap();
    tmpfs(&site, MsFlags::empty());
    let p = FsParticipant::new(ParticipantId::new("fs:site"), FsConfig { root: site.clone() }, d.path()).unwrap();
    let ctx = TxCtx { txid: TxId(1), data_dir: d.path().into(), deadline: None, outputs: Default::default() };
    let step = StepSpec { id: "s".into(), kind: "fs.delete".into(), config: serde_json::json!({"path": "x"}) };
    let e = p.stage(&ctx, &step).await.unwrap_err();
    umount(&site).unwrap();
    assert!(matches!(&e, PartError::Fatal(m) if m.contains("same filesystem")), "{e}");
}

#[test]
fn chown_failures_other_than_eperm_are_errors() {
    if !root() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    tmpfs(d.path(), MsFlags::empty());
    let t = d.path().join("t");
    std::fs::create_dir(&t).unwrap();
    mount(None::<&str>, d.path(), None::<&str>, MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY, None::<&str>).unwrap();
    let attrs = DirAttrs { uid: 1, gid: 1, mode: 0o755 };
    let r = txp_fs::execute_redo(&[RedoOp::Mkdir { target: t, attrs: Some(attrs) }]);
    umount(d.path()).unwrap();
    assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::ReadOnlyFilesystem);
}

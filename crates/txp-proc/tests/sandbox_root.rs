//! Root-only integration tests. Run via `scripts/test-proc.sh` (builds as the
//! normal user, executes the test binary with sudo).

use std::path::Path;
use std::sync::Arc;
use txp_core::{ParticipantId, TxId};
use txp_participant::{Participant, StepSpec, TxCtx, Vote};
use txp_proc::{MountSpec, ProcConfig, ProcParticipant, RunAs};

fn is_root() -> bool {
    (unsafe { libc::geteuid() }) == 0
}

fn tmp() -> tempfile::TempDir {
    // /tmp may be tmpfs; that is fine for overlay upper/lower.
    tempfile::Builder::new().prefix("txp-proc-").tempdir_in("/var/tmp").unwrap()
}

fn cfg(step: &str, root: &Path, argv: &[&str], extra_lowers: Vec<std::path::PathBuf>) -> ProcConfig {
    ProcConfig {
        step: step.into(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: Some(root.to_path_buf()),
        env: Default::default(),
        mounts: vec![MountSpec { resource: "r".into(), root: root.to_path_buf(), at: None, extra_lowers }],
        run_as: RunAs { uid: 65534, gid: 65534 },
        timeout_secs: 20,
        landlock: true,
        seccomp: true,
        private_tmp: true,
    }
}

fn ctx(t: u128, d: &Path) -> TxCtx {
    TxCtx { txid: TxId(t), data_dir: d.to_path_buf(), deadline: None, outputs: Default::default() }
}

#[tokio::test]
async fn staged_writes_are_invisible_until_commit() {
    if !is_root() {
        eprintln!("skipped: not root");
        return;
    }
    let d = tmp();
    let root = d.path().join("site");
    std::fs::create_dir_all(root.join("keep")).unwrap();
    std::fs::write(root.join("old.txt"), "old").unwrap();
    std::fs::write(root.join("keep/k.txt"), "k").unwrap();
    // nobody must be able to traverse: relax perms on the tempdir
    std::fs::set_permissions(d.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    std::fs::set_permissions(root.join("keep"), std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();

    let script = "set -e; echo new > new.txt; rm old.txt; mkdir -p dir/sub; echo x > dir/sub/x.txt; rm -rf keep; mkdir keep; echo fresh > keep/f.txt; echo hello-stdout; ls /proc | head -1 >/dev/null; test $$ -lt 100";
    let p = ProcParticipant::new(ParticipantId::new("proc:s"), cfg("s", &root, &["/bin/sh", "-c", script], vec![]), d.path()).unwrap();
    let c = ctx(1, d.path());
    let rep = p.stage(&c, &StepSpec { id: "s".into(), kind: "process".into(), config: serde_json::Value::Null }).await.unwrap();
    assert!(rep.outputs["stdout"].as_str().unwrap().contains("hello-stdout"));
    // invisible
    assert!(!root.join("new.txt").exists());
    assert_eq!(std::fs::read_to_string(root.join("old.txt")).unwrap(), "old");
    assert_eq!(p.prepare(&c).await.unwrap(), Vote::Prepared);
    assert!(!root.join("new.txt").exists());
    p.commit(TxId(1)).await.unwrap();
    assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "new\n");
    assert!(!root.join("old.txt").exists());
    assert_eq!(std::fs::read_to_string(root.join("dir/sub/x.txt")).unwrap(), "x\n");
    assert!(!root.join("keep/k.txt").exists(), "opaque dir replaced");
    assert_eq!(std::fs::read_to_string(root.join("keep/f.txt")).unwrap(), "fresh\n");
    assert!(!txp_fs::stage_root_for(&root).join(TxId(1).to_string()).exists());
    assert!(p.recover().await.unwrap().is_empty());
}

#[tokio::test]
async fn failing_process_votes_no_and_abort_discards() {
    if !is_root() {
        return;
    }
    let d = tmp();
    let root = d.path().join("r");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(d.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    let p = ProcParticipant::new(ParticipantId::new("proc:f"), cfg("f", &root, &["/bin/sh", "-c", "echo junk > j; exit 3"], vec![]), d.path()).unwrap();
    let e = p.stage(&ctx(2, d.path()), &StepSpec { id: "f".into(), kind: "process".into(), config: serde_json::Value::Null }).await.unwrap_err();
    assert!(matches!(e, txp_participant::PartError::VoteNo(_)), "{e:?}");
    p.abort(TxId(2)).await.unwrap();
    assert!(!root.join("j").exists());
    assert!(!txp_fs::stage_root_for(&root).join(TxId(2).to_string()).exists());
}

#[tokio::test]
async fn timeout_kills_the_whole_process_tree() {
    if !is_root() {
        return;
    }
    let d = tmp();
    let root = d.path().join("r");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(d.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut c = cfg("t", &root, &["/bin/sh", "-c", "sleep 100 & sleep 100 & wait"], vec![]);
    c.timeout_secs = 1;
    let p = ProcParticipant::new(ParticipantId::new("proc:t"), c, d.path()).unwrap();
    let start = std::time::Instant::now();
    let e = p.stage(&ctx(3, d.path()), &StepSpec { id: "t".into(), kind: "process".into(), config: serde_json::Value::Null }).await.unwrap_err();
    assert!(start.elapsed() < std::time::Duration::from_secs(8));
    assert!(format!("{e}").contains("timed out"), "{e}");
    assert!(!Path::new("/sys/fs/cgroup/txp").join(format!("{}-t", TxId(3))).exists());
}

#[tokio::test]
async fn network_and_outside_writes_are_denied() {
    if !is_root() {
        return;
    }
    let d = tmp();
    let root = d.path().join("r");
    let outside = d.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    for p in [d.path(), &root, &outside] {
        std::fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    }
    let script = format!(
        "if ip link 2>/dev/null | grep -q eth; then exit 10; fi; (echo leak > {}/leak) 2>/dev/null && exit 11; echo ok > in.txt",
        outside.display()
    );
    let p = ProcParticipant::new(ParticipantId::new("proc:n"), cfg("n", &root, &["/bin/sh", "-c", &script], vec![]), d.path()).unwrap();
    let r = p.stage(&ctx(4, d.path()), &StepSpec { id: "n".into(), kind: "process".into(), config: serde_json::Value::Null }).await;
    assert!(r.is_ok(), "{r:?}");
    assert!(!outside.join("leak").exists());
}

#[tokio::test]
async fn second_step_sees_first_steps_staged_output() {
    if !is_root() {
        return;
    }
    let d = tmp();
    let root = d.path().join("r");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(d.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    let p1 = Arc::new(ProcParticipant::new(ParticipantId::new("proc:a"), cfg("a", &root, &["/bin/sh", "-c", "echo one > a.txt"], vec![]), d.path()).unwrap());
    let lower_a = ProcParticipant::upper_dir(&root, TxId(5), "a");
    let p2 = Arc::new(ProcParticipant::new(ParticipantId::new("proc:b"), cfg("b", &root, &["/bin/sh", "-c", "cat a.txt > b.txt"], vec![lower_a]), d.path()).unwrap());
    let c = ctx(5, d.path());
    let st = |id: &str| StepSpec { id: id.into(), kind: "process".into(), config: serde_json::Value::Null };
    p1.stage(&c, &st("a")).await.unwrap();
    p2.stage(&c, &st("b")).await.unwrap();
    assert_eq!(p1.prepare(&c).await.unwrap(), Vote::Prepared);
    assert_eq!(p2.prepare(&c).await.unwrap(), Vote::Prepared);
    p1.commit(TxId(5)).await.unwrap();
    p2.commit(TxId(5)).await.unwrap();
    assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "one\n");
}

#[tokio::test]
async fn seccomp_filter_is_active_on_the_step_process() {
    if !is_root() {
        return;
    }
    let d = tmp();
    let root = d.path().join("r");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(d.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    // The confined process reports its own seccomp state; mode 2 is
    // SECCOMP_MODE_FILTER and Seccomp_filters counts the installed programs.
    let p = ProcParticipant::new(
        ParticipantId::new("proc:sc"),
        cfg("sc", &root, &["/bin/sh", "-c", "grep -E '^Seccomp' /proc/self/status"], vec![]),
        d.path(),
    )
    .unwrap();
    let rep = p
        .stage(&ctx(6, d.path()), &StepSpec { id: "sc".into(), kind: "process".into(), config: serde_json::Value::Null })
        .await
        .unwrap();
    let out = rep.outputs["stdout"].as_str().unwrap();
    assert!(
        out.lines().any(|l| l.starts_with("Seccomp:") && l.split_whitespace().nth(1) == Some("2")),
        "expected seccomp filter mode 2, got: {out:?}"
    );
    assert!(
        out.lines().any(|l| l.starts_with("Seccomp_filters:") && l.split_whitespace().nth(1).and_then(|n| n.parse::<u32>().ok()).unwrap_or(0) >= 1),
        "expected at least one seccomp filter, got: {out:?}"
    );
}

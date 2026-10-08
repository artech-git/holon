use std::path::Path;
use txp_engine::{Engine, EngineConfig, TxOutcome};

fn manifest(roots: &[&Path], fail: bool) -> String {
    let mut m = String::from("[txn]\nname = \"t\"\ntimeout = \"30s\"\n");
    for (i, r) in roots.iter().enumerate() {
        m.push_str(&format!("[[resource]]\nid = \"r{i}\"\nkind = \"fs.tree\"\npath = \"{}\"\n", r.display()));
    }
    for i in 0..roots.len() {
        m.push_str(&format!("[[step]]\nid = \"put{i}\"\nkind = \"fs.put\"\nresource = \"r{i}\"\npath = \"out.txt\"\ncontent = \"v=$txid\"\n"));
    }
    if fail {
        m.push_str("[[step]]\nid = \"bad\"\nkind = \"fs.put\"\nresource = \"r0\"\npath = \"x\"\nsource = \"/nonexistent/file\"\nafter = [\"put0\"]\n");
    }
    m
}

async fn open(d: &Path) -> Engine {
    let (e, _) = Engine::open(EngineConfig::new(d.join("data"))).await.unwrap();
    e
}

#[tokio::test]
async fn two_participants_commit_atomically() {
    let d = tempfile::tempdir().unwrap();
    let r0 = d.path().join("a");
    let r1 = d.path().join("b");
    std::fs::create_dir_all(&r0).unwrap();
    std::fs::create_dir_all(&r1).unwrap();
    let e = open(d.path()).await;
    let (txid, out) = e.run(&manifest(&[&r0, &r1], false)).await.unwrap();
    assert_eq!(out, TxOutcome::Committed);
    assert_eq!(std::fs::read_to_string(r0.join("out.txt")).unwrap(), format!("v={txid}"));
    assert_eq!(std::fs::read_to_string(r1.join("out.txt")).unwrap(), format!("v={txid}"));
    let st = e.status(txid).unwrap();
    assert!(!st.one_phase);
    assert!(e.in_doubt().is_empty());
    assert!(e.locks().held().is_empty());
    let s = e.wal().stats();
    assert_eq!(s.fsyncs, 1, "exactly one forced write for a 2PC commit: {s:?}");
    e.shutdown().await;
}

#[tokio::test]
async fn one_participant_uses_one_phase_path() {
    let d = tempfile::tempdir().unwrap();
    let r0 = d.path().join("a");
    std::fs::create_dir_all(&r0).unwrap();
    let e = open(d.path()).await;
    let (txid, out) = e.run(&manifest(&[&r0], false)).await.unwrap();
    assert_eq!(out, TxOutcome::Committed);
    assert!(e.status(txid).unwrap().one_phase);
    assert_eq!(e.wal().stats().fsyncs, 0, "1PC must not force a coordinator write");
    assert!(r0.join("out.txt").exists());
    e.shutdown().await;
}

#[tokio::test]
async fn failing_step_aborts_everything() {
    let d = tempfile::tempdir().unwrap();
    let r0 = d.path().join("a");
    let r1 = d.path().join("b");
    std::fs::create_dir_all(&r0).unwrap();
    std::fs::create_dir_all(&r1).unwrap();
    let e = open(d.path()).await;
    let (_txid, out) = e.run(&manifest(&[&r0, &r1], true)).await.unwrap();
    assert!(matches!(out, TxOutcome::Aborted { .. }), "{out:?}");
    assert!(!r0.join("out.txt").exists());
    assert!(!r1.join("out.txt").exists());
    assert!(!d.path().join(".txp-stage").exists() || std::fs::read_dir(d.path().join(".txp-stage/a")).map(|r| r.count() == 0).unwrap_or(true));
    assert!(e.orphan_journals().is_empty());
    e.shutdown().await;
}

#[tokio::test]
async fn read_only_manifest_commits_trivially() {
    let d = tempfile::tempdir().unwrap();
    let r0 = d.path().join("a");
    std::fs::create_dir_all(&r0).unwrap();
    let e = open(d.path()).await;
    let m = format!("[txn]\nname = \"ro\"\n[[resource]]\nid = \"r\"\nkind = \"fs.tree\"\npath = \"{}\"\nmode = \"read\"\n", r0.display());
    let (_t, out) = e.run(&m).await.unwrap();
    assert_eq!(out, TxOutcome::Committed);
    e.shutdown().await;
}

#[tokio::test]
async fn conflicting_transactions_serialize() {
    let d = tempfile::tempdir().unwrap();
    let r0 = d.path().join("a");
    std::fs::create_dir_all(&r0).unwrap();
    let e = open(d.path()).await;
    let mut hs = Vec::new();
    for _ in 0..8 {
        let e = e.clone();
        let m = manifest(&[&r0], false);
        hs.push(tokio::spawn(async move { e.run(&m).await.unwrap().1 }));
    }
    for h in hs {
        assert_eq!(h.await.unwrap(), TxOutcome::Committed);
    }
    assert!(e.locks().held().is_empty());
    e.shutdown().await;
}

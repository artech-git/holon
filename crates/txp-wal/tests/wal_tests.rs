use std::path::Path;
use std::sync::Arc;
use txp_core::{LogRecord, Lsn, ParticipantId, ParticipantSpec, TxId, TxPhase};
use txp_wal::sim::SimDisk;
use txp_wal::{Disk, RealDisk, Wal, WalConfig};

fn begin(t: u128) -> LogRecord {
    LogRecord::Begin {
        txid: TxId(t),
        name: format!("t{t}"),
        manifest_digest: "d".into(),
        submitter: None,
        participants: vec![ParticipantSpec { id: ParticipantId::new("a"), kind: "k".into(), config: serde_json::Value::Null }],
    }
}

#[tokio::test]
async fn append_recover_roundtrip_real_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = WalConfig { segment_bytes: 4096, ..Default::default() };
    let (wal, rec) = Wal::open(Arc::new(RealDisk), dir.path(), cfg.clone()).unwrap();
    assert_eq!(rec.next_lsn, Lsn(1));
    for t in 1..=40u128 {
        wal.append(begin(t)).await.unwrap();
        let proof = wal.append_commit(TxId(t), vec![ParticipantId::new("a")]).await.unwrap();
        assert_eq!(proof.txid(), TxId(t));
        if t % 2 == 0 {
            wal.append(LogRecord::Done { txid: TxId(t) }).await.unwrap();
        }
    }
    wal.shutdown();
    let (wal2, rec2) = Wal::open(Arc::new(RealDisk), dir.path(), cfg).unwrap();
    assert!(rec2.segments.len() > 1, "segments should have rolled");
    let table = wal2.table();
    let t = table.lock();
    for n in 1..=40u128 {
        let e = t.get(TxId(n)).unwrap();
        assert_eq!(e.phase, if n % 2 == 0 { TxPhase::Done } else { TxPhase::Committing });
    }
    assert_eq!(t.recovery_actions().len(), 20);
}

#[tokio::test]
async fn checkpoint_truncates_segments_and_recovers_from_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = WalConfig { segment_bytes: 2048, ..Default::default() };
    let (wal, _) = Wal::open(Arc::new(RealDisk), dir.path(), cfg.clone()).unwrap();
    for t in 1..=30u128 {
        wal.append(begin(t)).await.unwrap();
        wal.append_commit(TxId(t), vec![]).await.unwrap();
        wal.append(LogRecord::Done { txid: TxId(t) }).await.unwrap();
    }
    // 31 stays in flight
    wal.append(begin(31)).await.unwrap();
    let before = RealDisk.list(dir.path()).unwrap().len();
    wal.checkpoint().await.unwrap();
    let after = RealDisk.list(dir.path()).unwrap().len();
    assert!(after < before, "expected truncation: {before} -> {after}");
    wal.append_commit(TxId(31), vec![]).await.unwrap();
    wal.shutdown();
    let (wal2, rec) = Wal::open(Arc::new(RealDisk), dir.path(), cfg).unwrap();
    assert_eq!(rec.records.len(), 1, "only the post-snapshot commit replays");
    assert_eq!(wal2.table().lock().get(TxId(31)).unwrap().phase, TxPhase::Committing);
}

#[tokio::test]
async fn torn_tail_is_truncated_and_acked_commits_survive() {
    for seed in 1..=25u64 {
        let disk = SimDisk::new(seed);
        let dir = Path::new("/sim/wal");
        let cfg = WalConfig { segment_bytes: 1 << 20, ..Default::default() };
        let (wal, _) = Wal::open(Arc::new(disk.clone()), dir, cfg.clone()).unwrap();
        let mut acked = Vec::new();
        for t in 1..=12u128 {
            wal.append(begin(t)).await.unwrap();
            if t <= 8 {
                wal.append_commit(TxId(t), vec![]).await.unwrap();
                acked.push(t);
            }
        }
        // Some unforced Begins after the last fsync may be lost or torn.
        let _ = wal.append(begin(100)).await;
        drop(wal);
        disk.crash();
        let (wal2, rec) = Wal::open(Arc::new(disk.clone()), dir, cfg).unwrap();
        let table = wal2.table();
        let t = table.lock();
        for a in &acked {
            assert_eq!(t.get(TxId(*a)).map(|e| e.phase), Some(TxPhase::Committing), "seed {seed} lost acked commit {a}");
        }
        // Whatever survived is a prefix; lsns are contiguous.
        let mut expect = Lsn(1);
        for (lsn, _) in &rec.records {
            assert_eq!(*lsn, expect);
            expect = expect.next();
        }
    }
}

#[test]
fn fsync_failure_is_fatal() {
    // Run the writer in a child so the abort does not kill the test binary.
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args(["--exact", "fsync_failure_is_fatal_child", "--ignored", "--nocapture"])
        .env("TXP_CHILD", "1")
        .output()
        .unwrap();
    assert!(!out.status.success(), "child should abort");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("FATAL"), "stderr: {stderr}");
}

#[test]
#[ignore]
fn fsync_failure_is_fatal_child() {
    if std::env::var("TXP_CHILD").is_err() {
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let disk = SimDisk::new(1);
        let (wal, _) = Wal::open(Arc::new(disk.clone()), Path::new("/sim/wal"), WalConfig::default()).unwrap();
        wal.append(begin(1)).await.unwrap();
        disk.set_fail_next_sync();
        let _ = wal.append_commit(TxId(1), vec![]).await;
        // unreachable: the writer thread aborted the process
    });
}

#[tokio::test]
async fn illegal_transition_is_rejected_before_write() {
    let disk = SimDisk::new(3);
    let (wal, _) = Wal::open(Arc::new(disk.clone()), Path::new("/sim/wal"), WalConfig::default()).unwrap();
    wal.append(begin(1)).await.unwrap();
    wal.append_commit(TxId(1), vec![]).await.unwrap();
    let e = wal.append(LogRecord::Abort { txid: TxId(1) }).await.unwrap_err();
    assert!(matches!(e, txp_wal::WalError::Rejected(_)));
    let e = wal.append(LogRecord::Done { txid: TxId(2) }).await.unwrap_err();
    assert!(matches!(e, txp_wal::WalError::Rejected(_)));
}

#[tokio::test]
async fn group_commit_batches_under_concurrency() {
    let disk = SimDisk::new(9);
    let (wal, _) = Wal::open(Arc::new(disk.clone()), Path::new("/sim/wal"), WalConfig::default()).unwrap();
    let mut hs = Vec::new();
    for t in 1..=200u128 {
        let w = wal.clone();
        hs.push(tokio::spawn(async move {
            w.append(begin(t)).await.unwrap();
            w.append_commit(TxId(t), vec![]).await.unwrap();
        }));
    }
    for h in hs {
        h.await.unwrap();
    }
    let s = wal.stats();
    assert_eq!(s.records, 400);
    assert!(s.max_batch > 1, "expected batching, stats {s:?}");
    assert!(s.fsyncs < 400, "expected fewer fsyncs than commits: {s:?}");
}

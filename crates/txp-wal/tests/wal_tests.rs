use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::sync::Arc;
use txp_core::{LogRecord, Lsn, ParticipantId, ParticipantSpec, TxId, TxPhase};
use txp_wal::format::encode_record;
use txp_wal::recovery::segment_path;
use txp_wal::sim::SimDisk;
use txp_wal::writer::WalStats;
use txp_wal::{Disk, HEADER_LEN, RealDisk, Wal, WalConfig, WalError};

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

/// Bytes one `begin(t)` frame takes for single-digit `t`.
fn frame_len() -> usize {
    encode_record(Lsn(1), &begin(1)).len()
}

/// Segments with room for exactly `records` single-digit `begin` frames.
fn small_segments(records: usize) -> WalConfig {
    WalConfig { segment_bytes: (HEADER_LEN + records * frame_len()) as u64, ..Default::default() }
}

const SIM_DIR: &str = "/sim/wal";

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(f)
}

fn open(disk: &SimDisk, cfg: WalConfig) -> Wal {
    Wal::open(Arc::new(disk.clone()), Path::new(SIM_DIR), cfg).unwrap().0
}

/// `(name, FATAL message, setup)`; the setup ends by triggering the failure.
type FatalScenario = (&'static str, &'static str, fn(SimDisk));

/// Disk failures the writer must not survive.
fn fatal_scenarios() -> Vec<FatalScenario> {
    vec![
        ("commit_datasync", "fdatasync failed", |disk| {
            block_on(async move {
                let wal = open(&disk, WalConfig::default());
                wal.append(begin(1)).await.unwrap();
                disk.set_fail_next_sync();
                let _ = wal.append_commit(TxId(1), vec![]).await;
            })
        }),
        ("write", "log write failed", |disk| {
            block_on(async move {
                let wal = open(&disk, WalConfig::default());
                disk.remove(&segment_path(Path::new(SIM_DIR), 1)).unwrap();
                let _ = wal.append(begin(1)).await;
            })
        }),
        ("roll_datasync", "fdatasync on segment roll failed", |disk| {
            block_on(async move {
                let wal = open(&disk, small_segments(2));
                wal.append(begin(1)).await.unwrap();
                wal.append(begin(2)).await.unwrap();
                disk.set_fail_next_sync();
                let _ = wal.append(begin(3)).await;
            })
        }),
        ("roll_create", "cannot roll segment", |disk| {
            block_on(async move {
                let wal = open(&disk, small_segments(2));
                wal.append(begin(1)).await.unwrap();
                wal.append(begin(2)).await.unwrap();
                disk.write_atomic(&segment_path(Path::new(SIM_DIR), 2), b"in the way").unwrap();
                let _ = wal.append(begin(3)).await;
            })
        }),
        ("new_segment_fsync", "fsync of new segment failed", |disk| {
            disk.set_fail_next_sync();
            let _ = open(&disk, WalConfig::default());
        }),
        ("dir_fsync", "fsync of log directory failed", |disk| {
            disk.set_fail_next_dir_sync();
            let _ = open(&disk, WalConfig::default());
        }),
        ("checkpoint_datasync", "fdatasync before checkpoint failed", |disk| {
            block_on(async move {
                let wal = open(&disk, WalConfig::default());
                wal.append(begin(1)).await.unwrap();
                disk.set_fail_next_sync();
                let _ = wal.checkpoint().await;
            })
        }),
    ]
}

#[test]
fn disk_failures_abort_the_process() {
    // After a failed sync the page cache may hold bytes the disk never saw,
    // so the writer aborts instead of retrying. Each scenario runs in a
    // child (`fatal_child`) so the abort does not kill the test binary.
    let exe = std::env::current_exe().unwrap();
    for (name, msg, _) in fatal_scenarios() {
        let out = std::process::Command::new(&exe)
            .args(["--exact", "fatal_child", "--nocapture"])
            .env("TXP_FATAL_SCENARIO", name)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.signal(), Some(nix::libc::SIGABRT), "{name}: {stderr}");
        assert!(stderr.contains(&format!("txp-wal FATAL: {msg}")), "{name}: {stderr}");
    }
}

/// Runs one scenario of `disk_failures_abort_the_process`; a no-op otherwise.
#[test]
fn fatal_child() {
    let Ok(name) = std::env::var("TXP_FATAL_SCENARIO") else { return };
    let (_, _, setup) = fatal_scenarios().into_iter().find(|(n, _, _)| *n == name).unwrap();
    setup(SimDisk::new(1));
}

#[tokio::test]
async fn shutdown_is_idempotent_and_later_requests_fail_closed() {
    let disk = SimDisk::new(5);
    let wal = open(&disk, WalConfig::default());
    assert_eq!(wal.dir(), Path::new(SIM_DIR));
    wal.append(begin(1)).await.unwrap();
    wal.shutdown();
    wal.shutdown();
    let e = wal.append(begin(2)).await.unwrap_err();
    assert!(matches!(e, WalError::Closed));
    assert_eq!(e.to_string(), "log writer stopped");
    assert!(matches!(wal.checkpoint().await, Err(WalError::Closed)));
    // The shutdown synced the tail: it survives a crash.
    drop(wal);
    disk.crash();
    let (_, rec) = Wal::open(Arc::new(disk), Path::new(SIM_DIR), WalConfig::default()).unwrap();
    assert_eq!(rec.records.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_is_split_when_it_outgrows_max_batch_bytes() {
    let disk = SimDisk::new(6);
    let wal = open(&disk, WalConfig { max_batch_bytes: frame_len() + 1, ..Default::default() });
    // Hold the table so the writer stalls on its first request while the
    // rest queue up behind it and arrive as one oversized batch.
    let table = wal.table();
    let guard = table.lock();
    let tasks: Vec<_> = (1..=9u128)
        .map(|t| {
            let w = wal.clone();
            tokio::spawn(async move { w.append(begin(t)).await })
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(200));
    drop(guard);
    let mut lsns = Vec::new();
    for t in tasks {
        lsns.push(t.await.unwrap().unwrap());
    }
    lsns.sort();
    assert_eq!(lsns, (1..=9).map(Lsn).collect::<Vec<_>>());
    let s = wal.stats();
    assert_eq!((s.records, s.max_batch, s.fsyncs), (9, 1, 0), "one record per flush: {s:?}");
}

#[tokio::test]
async fn checkpoint_reports_a_snapshot_it_cannot_write() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (wal, _) = Wal::open(Arc::new(RealDisk), dir.path(), WalConfig::default()).unwrap();
    wal.append(begin(1)).await.unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let probe = std::fs::File::create(dir.path().join("probe")).is_ok(); // root ignores modes
    let r = wal.checkpoint().await;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    if !probe {
        let e = r.unwrap_err();
        assert!(matches!(e, WalError::Io(_)) && e.to_string().starts_with("io: "), "{e}");
    }
}

#[tokio::test]
async fn checkpoint_tolerates_an_old_segment_that_is_already_gone() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = small_segments(2);
    let (wal, _) = Wal::open(Arc::new(RealDisk), dir.path(), cfg).unwrap();
    for t in 1..=7u128 {
        wal.append(begin(t)).await.unwrap();
    }
    let segs = |d: &Path| RealDisk.list(d).unwrap().into_iter().filter(|p| p.extension().is_some_and(|e| e == "wal")).count();
    assert_eq!(segs(dir.path()), 4);
    std::fs::remove_file(segment_path(dir.path(), 1)).unwrap();
    assert_eq!(wal.checkpoint().await.unwrap(), Lsn(7));
    // Segments 2 and 3 are removed; the current one (4) stays.
    assert_eq!(segs(dir.path()), 1);
    assert!(segment_path(dir.path(), 4).exists());
}

#[tokio::test]
async fn checkpoint_of_a_single_segment_removes_nothing() {
    let disk = SimDisk::new(8);
    let wal = open(&disk, WalConfig::default());
    assert_eq!(wal.checkpoint().await.unwrap(), Lsn::ZERO);
    wal.append(begin(1)).await.unwrap();
    assert_eq!(wal.checkpoint().await.unwrap(), Lsn(1));
    assert!(disk.exists(&segment_path(Path::new(SIM_DIR), 1)));
    assert!(disk.exists(&Path::new(SIM_DIR).join("snap.json")));
}

#[test]
fn config_and_stats_defaults() {
    let c = WalConfig::default();
    assert_eq!((c.segment_bytes, c.max_batch_bytes, c.max_batch_records), (64 << 20, 4 << 20, 4096));
    let s = WalStats::default();
    assert_eq!((s.batches, s.batch_hist), (0, [0; 5]));
    assert_eq!(WalError::Rejected("x".into()).to_string(), "rejected: x");
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

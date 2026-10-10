//! How the coordinator handles misbehaving participants, a log that stops
//! accepting writes, and recovery from a log left in each kind of state.
//!
//! Faults come from a wrapper around the real fs participant, installed
//! with `Engine::open_with`. Each managed root's directory name lists the
//! faults its participant shows, joined by `+` (e.g. `stop_log+abort_committed`).

use async_trait::async_trait;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use txp_core::{LogRecord, ParticipantId, ParticipantSpec, Submitter, TxId};
use txp_engine::engine::RecoveryReport;
use txp_engine::{Engine, EngineConfig, EngineError, TxOutcome};
use txp_lock::{Mode, ResourceKey};
use txp_participant::registry::Factory;
use txp_participant::{AbortOutcome, Capabilities, LocalState, Outcome, PartError, Participant, StageReport, StepSpec, TxCtx, Vote};
use txp_wal::{RealDisk, Wal, WalConfig};

struct Faulty {
    inner: Arc<dyn Participant>,
    faults: HashSet<String>,
    wal: Arc<OnceLock<Wal>>,
    failed_once: std::sync::atomic::AtomicBool,
}

impl Faulty {
    fn has(&self, fault: &str) -> bool {
        self.faults.contains(fault)
    }

    /// For `<op>_transient` faults: a transient error the first time only.
    fn transient_once(&self, op: &str) -> Result<(), PartError> {
        if self.has(&format!("{op}_transient")) && !self.failed_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(PartError::Transient(format!("{op} hiccup")));
        }
        Ok(())
    }
}

#[async_trait]
impl Participant for Faulty {
    fn id(&self) -> ParticipantId {
        self.inner.id()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn stage(&self, tx: &TxCtx, step: &StepSpec) -> Result<StageReport, PartError> {
        if self.has("slow_stage") {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        self.inner.stage(tx, step).await
    }
    async fn prepare(&self, tx: &TxCtx) -> Result<Vote, PartError> {
        if self.has("vote_no") {
            return Err(PartError::VoteNo("scripted".into()));
        }
        if self.has("prepare_panic") {
            panic!("scripted prepare panic");
        }
        if self.has("stop_log") {
            self.wal.get().unwrap().shutdown();
        }
        let v = self.inner.prepare(tx).await?;
        Ok(if self.has("read_only") { Vote::ReadOnly } else { v })
    }
    async fn commit(&self, txid: TxId) -> Result<(), PartError> {
        self.transient_once("commit")?;
        if self.has("commit_fatal") {
            return Err(PartError::Fatal("scripted".into()));
        }
        if self.has("commit_panic") {
            panic!("scripted commit panic");
        }
        self.inner.commit(txid).await
    }
    async fn abort(&self, txid: TxId) -> Result<AbortOutcome, PartError> {
        if self.has("abort_fatal") {
            return Err(PartError::Fatal("scripted".into()));
        }
        let r = self.inner.abort(txid).await?;
        Ok(if self.has("abort_committed") { AbortOutcome::AlreadyCommitted } else { r })
    }
    async fn commit_one_phase(&self, tx: &TxCtx) -> Result<Outcome, PartError> {
        self.transient_once("one_phase")?;
        if self.has("one_phase_aborted") {
            return Ok(Outcome::Aborted("scripted decline".into()));
        }
        if self.has("one_phase_fatal") {
            return Err(PartError::Fatal("scripted".into()));
        }
        self.inner.commit_one_phase(tx).await
    }
    async fn recover(&self) -> Result<Vec<(TxId, LocalState)>, PartError> {
        self.inner.recover().await
    }
}

fn faulty(wal: Arc<OnceLock<Wal>>) -> Factory {
    Arc::new(move |spec: &ParticipantSpec, data_dir: &Path| {
        let inner = txp_fs::FsParticipant::factory(spec, data_dir)?;
        let root = PathBuf::from(spec.config["root"].as_str().unwrap());
        let faults = root.file_name().unwrap().to_string_lossy().split('+').map(String::from).collect();
        Ok(Arc::new(Faulty { inner, faults, wal: wal.clone(), failed_once: Default::default() }) as Arc<dyn Participant>)
    })
}

async fn open_cfg(cfg: EngineConfig) -> Result<(Engine, RecoveryReport), EngineError> {
    let slot = Arc::new(OnceLock::new());
    let factory = faulty(slot.clone());
    let opened = Engine::open_with(cfg, move |r| r.register("fs", factory)).await;
    if let Ok((e, _)) = &opened {
        let _ = slot.set(e.wal().clone());
    }
    opened
}

async fn open(data: &Path) -> (Engine, RecoveryReport) {
    open_cfg(EngineConfig::new(data)).await.unwrap()
}

/// Create one managed root per name under `d`.
fn roots(d: &Path, names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|n| {
            let r = d.join(n);
            std::fs::create_dir_all(&r).unwrap();
            r
        })
        .collect()
}

/// One `fs.put` of `out` per root.
fn manifest(roots: &[PathBuf], timeout: &str) -> String {
    let mut m = format!("[txn]\nname = \"faults\"\ntimeout = \"{timeout}\"\n");
    for (i, r) in roots.iter().enumerate() {
        m += &format!("[[resource]]\nid = \"r{i}\"\nkind = \"fs.tree\"\npath = \"{}\"\n", r.display());
    }
    for i in 0..roots.len() {
        m += &format!("[[step]]\nid = \"put{i}\"\nkind = \"fs.put\"\nresource = \"r{i}\"\npath = \"out\"\ncontent = \"$txid\"\n");
    }
    m
}

/// Run one transaction over roots named `names` with a fresh engine.
async fn run(names: &[&str], timeout: &str) -> (Engine, TxOutcome, Vec<PathBuf>, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let rs = roots(d.path(), names);
    let (e, _) = open(&d.path().join("data")).await;
    let (_, out) = e.run(&manifest(&rs, timeout)).await.unwrap();
    (e, out, rs, d)
}

fn aborted_because(out: &TxOutcome, why: &str) -> bool {
    matches!(out, TxOutcome::Aborted { reason } if reason.contains(why))
}

#[tokio::test]
async fn a_step_outliving_the_transaction_aborts_it() {
    let (_, out, rs, _d) = run(&["slow_stage"], "300ms").await;
    assert!(aborted_because(&out, "step put0: transaction timeout"), "{out:?}");
    assert!(!rs[0].join("out").exists());
}

#[tokio::test]
async fn one_phase_decline_aborts_and_one_phase_failure_is_in_doubt() {
    let (_, out, _, _d) = run(&["one_phase_aborted"], "30s").await;
    assert!(aborted_because(&out, "scripted decline"), "{out:?}");
    let (e, out, _, _d) = run(&["one_phase_fatal"], "30s").await;
    assert!(matches!(&out, TxOutcome::InDoubt { decision, .. } if decision == "one_phase"), "{out:?}");
    assert!(e.locks().held().is_empty(), "locks released for the operator");
}

#[tokio::test]
async fn a_no_vote_or_a_crashed_prepare_aborts_everyone() {
    let (_, out, rs, _d) = run(&["vote_no", "ok"], "30s").await;
    assert!(aborted_because(&out, "vote no: scripted"), "{out:?}");
    assert!(!rs[1].join("out").exists());
    let (_, out, _, _d) = run(&["prepare_panic", "ok"], "30s").await;
    assert!(aborted_because(&out, "prepare task"), "{out:?}");
}

#[tokio::test]
async fn read_only_voters_skip_phase_two() {
    let (e, out, rs, _d) = run(&["read_only", "read_only+2"], "30s").await;
    assert_eq!(out, TxOutcome::Committed);
    assert!(!rs[0].join("out").exists() && !rs[1].join("out").exists());
    assert_eq!(e.wal().stats().fsyncs, 0, "no commit record was forced");
}

#[tokio::test]
async fn commit_failures_after_the_decision_leave_the_transaction_in_doubt() {
    let (e, out, rs, _d) = run(&["commit_fatal", "ok"], "30s").await;
    assert!(matches!(&out, TxOutcome::InDoubt { decision, error } if decision == "commit" && error.contains("scripted")), "{out:?}");
    assert!(rs[1].join("out").exists(), "the healthy participant committed");
    let doubt = e.in_doubt();
    assert_eq!(doubt.len(), 1);
    assert_eq!(doubt[0]["phase"], "Committing");
    assert_eq!(doubt[0]["status"]["outcome"], "in_doubt");
    // The stuck participant's journal is explained by the log: no orphans.
    assert!(e.orphan_journals().is_empty());

    let (_, out, _, _d) = run(&["commit_panic", "ok"], "30s").await;
    assert!(matches!(&out, TxOutcome::InDoubt { error, .. } if error.contains("commit task")), "{out:?}");
}

#[tokio::test]
async fn slow_retries_show_as_in_doubt_until_they_succeed() {
    let d = tempfile::tempdir().unwrap();
    let rs = roots(d.path(), &["commit_transient", "ok", "one_phase_transient"]);
    let mut cfg = EngineConfig::new(d.path().join("data"));
    cfg.in_doubt_after = Duration::ZERO;
    let (e, _) = open_cfg(cfg).await.unwrap();
    let (txid, rx) = e.submit(&manifest(&rs[..2], "30s"), None).unwrap();
    // While the first commit attempt backs off, the status says why.
    let start = std::time::Instant::now();
    while !matches!(e.status(txid).unwrap().outcome, Some(TxOutcome::InDoubt { .. })) {
        assert!(start.elapsed() < Duration::from_secs(5), "never reported in doubt");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(rx.await.unwrap(), TxOutcome::Committed);
    let (_, out) = e.run(&manifest(&rs[2..], "30s")).await.unwrap();
    assert_eq!(out, TxOutcome::Committed, "the one-phase commit is retried too");
}

#[tokio::test]
async fn a_participant_that_committed_locally_wins_over_abort() {
    let (_, out, _, _d) = run(&["abort_committed", "vote_no"], "30s").await;
    assert_eq!(out, TxOutcome::Committed);
    let (_, out, _, _d) = run(&["abort_fatal", "vote_no"], "30s").await;
    assert!(aborted_because(&out, "vote no"), "{out:?}");
}

#[tokio::test]
async fn a_log_that_stops_before_the_commit_point_means_abort() {
    // Commit, Abort and Done records all fail; the outcome is still abort.
    let (_, out, rs, _d) = run(&["stop_log", "ok"], "30s").await;
    assert!(aborted_because(&out, "commit record: log writer stopped"), "{out:?}");
    assert!(!rs[1].join("out").exists());
    // ...unless a participant already committed locally, which is adopted
    // even though the audit record cannot be written.
    let (_, out, _, _d) = run(&["stop_log", "abort_committed"], "30s").await;
    assert_eq!(out, TxOutcome::Committed);
}

#[tokio::test]
async fn admission_rules() {
    let d = tempfile::tempdir().unwrap();
    let rs = roots(d.path(), &["ok"]);
    let mut cfg = EngineConfig::new(d.path().join("data"));
    cfg.auth.allow_uids.insert(4242);
    let (e, report) = open_cfg(cfg).await.unwrap();
    assert!(report.in_doubt.is_empty() && e.recovery_report().redriven_commits.is_empty());
    assert_eq!(e.data_dir(), d.path().join("data"));
    assert!(e.authorized(0) && e.authorized(4242) && !e.authorized(31337));
    let text = manifest(&rs, "30s");
    let stranger = Submitter { uid: 31337, gid: 31337, pid: None };
    let e1 = e.submit(&text, Some(stranger)).err().unwrap();
    assert!(matches!(e1, EngineError::Unauthorized(_)) && e1.to_string() == "unauthorized: uid 31337 may not submit transactions");
    // An allowed non-root submitter is pinned to its own identity.
    let (txid, rx) = e.submit(&text, Some(Submitter { uid: 4242, gid: 4242, pid: Some(1) })).unwrap();
    assert_eq!(rx.await.unwrap(), TxOutcome::Committed);
    assert_eq!(e.list().iter().map(|s| s.txid).collect::<Vec<_>>(), vec![txid]);
    assert!(e.checkpoint().await.unwrap() > txp_core::Lsn::ZERO);
    e.stop_accepting();
    let e2 = e.submit(&text, None).err().unwrap();
    assert!(matches!(e2, EngineError::NotAccepting(_)), "{e2}");
}

#[tokio::test]
async fn transactions_that_cannot_start_abort_cleanly() {
    let d = tempfile::tempdir().unwrap();
    let rs = roots(d.path(), &["ok"]);
    let (e, _) = open(&d.path().join("data")).await;
    // Someone else holds the root for longer than our whole timeout.
    e.locks().acquire(TxId(1), ResourceKey::new(format!("fs:{}", rs[0].display())), Mode::Exclusive).await;
    let (_, out) = e.run(&manifest(&rs, "100ms")).await.unwrap();
    assert!(aborted_because(&out, "timed out waiting for locks"), "{out:?}");
    e.locks().release_all(TxId(1));
    // A managed root that does not exist cannot get a participant.
    let (_, out) = e.run(&manifest(&[d.path().join("missing")], "30s")).await.unwrap();
    assert!(aborted_because(&out, "participant setup"), "{out:?}");
    // Without a log nothing can begin.
    e.wal().shutdown();
    let (_, out) = e.run(&manifest(&rs, "30s")).await.unwrap();
    assert!(aborted_because(&out, "log: log writer stopped"), "{out:?}");
    assert!(e.locks().held().is_empty());
}

fn fs_spec(root: &Path) -> ParticipantSpec {
    ParticipantSpec { id: ParticipantId::new(format!("fs:{}", root.display())), kind: "fs".into(), config: serde_json::json!({ "root": root }) }
}

fn begin(txid: TxId, participants: Vec<ParticipantSpec>) -> LogRecord {
    LogRecord::Begin { txid, name: "left over".into(), manifest_digest: "d".into(), submitter: None, participants }
}

/// Leave `recs` in the log under `data`, as a crashed daemon would.
async fn crashed_with(data: &Path, recs: Vec<LogRecord>) {
    let (wal, _) = Wal::open(Arc::new(RealDisk), &data.join("wal"), WalConfig::default()).unwrap();
    for r in recs {
        wal.append(r).await.unwrap();
    }
    wal.shutdown();
}

#[tokio::test]
async fn recovery_finishes_or_reports_every_kind_of_leftover() {
    let d = tempfile::tempdir().unwrap();
    let rs = roots(d.path(), &["ok", "abort_committed", "commit_fatal", "ok2"]);
    let data = d.path().join("data");
    let (aborting, adopted, stuck, unknown, committing) = (TxId(1), TxId(2), TxId(3), TxId(4), TxId(5));
    let odd = ParticipantSpec { id: ParticipantId::new("pg:db"), kind: "pg".into(), config: serde_json::Value::Null };
    crashed_with(
        &data,
        vec![
            // Abort decided, Done never written.
            begin(aborting, vec![fs_spec(&rs[0])]),
            LogRecord::Abort { txid: aborting },
            // Only Begin, but the participant had committed locally (1PC).
            begin(adopted, vec![fs_spec(&rs[1])]),
            // Commit decided, and the participant now refuses.
            begin(stuck, vec![fs_spec(&rs[2])]),
            LogRecord::Commit { txid: stuck, participants: vec![fs_spec(&rs[2]).id] },
            // A participant kind this daemon does not know.
            begin(unknown, vec![odd]),
            // Commit decided and completed now.
            begin(committing, vec![fs_spec(&rs[3])]),
            LogRecord::Commit { txid: committing, participants: vec![fs_spec(&rs[3]).id] },
        ],
    )
    .await;
    let (e, report) = open(&data).await;
    assert_eq!(report.redriven_aborts, vec![aborting]);
    assert_eq!(report.redriven_commits, vec![adopted, committing]);
    let doubt: Vec<TxId> = report.in_doubt.iter().map(|(t, _)| *t).collect();
    assert_eq!(doubt, vec![stuck, unknown]);
    assert!(report.in_doubt[1].1.contains("no factory for participant kind \"pg\""), "{report:?}");
    assert_eq!(e.status(aborting).unwrap().outcome, Some(TxOutcome::Aborted { reason: "recovered: presumed abort".into() }));
    assert_eq!(e.status(adopted).unwrap().outcome, Some(TxOutcome::Committed));
}

#[tokio::test]
async fn a_torn_log_tail_is_reported_at_startup() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("data");
    let begin = begin(TxId(1), vec![]);
    let done = LogRecord::Done { txid: TxId(1) };
    let end = txp_wal::HEADER_LEN + txp_wal::format::encode_record(txp_core::Lsn(1), &begin).len() + txp_wal::format::encode_record(txp_core::Lsn(2), &done).len();
    crashed_with(&data, vec![begin, done]).await;
    // Half a record after the last whole one, as a crash mid-write leaves.
    use std::os::unix::fs::FileExt;
    let seg = std::fs::OpenOptions::new().write(true).open(data.join("wal/seg-0000000000000001.wal")).unwrap();
    seg.write_all_at(&[0x40, 0, 0, 0, 1, 2, 3], end as u64).unwrap();
    let (e, report) = open(&data).await;
    let torn = report.truncated_tail.unwrap();
    assert!(torn.contains(&format!("seg-0000000000000001.wal@{end}: crc mismatch")), "{torn}");
    assert_eq!(e.recovery_report().truncated_tail, Some(torn));
}

#[tokio::test]
async fn a_corrupt_log_stops_startup() {
    let d = tempfile::tempdir().unwrap();
    let wal = d.path().join("data/wal");
    std::fs::create_dir_all(&wal).unwrap();
    std::fs::write(wal.join("seg-0000000000000001.wal"), vec![0xab; 100]).unwrap();
    let e = open_cfg(EngineConfig::new(d.path().join("data"))).await.err().unwrap();
    assert!(matches!(&e, EngineError::Recovery(m) if m.contains("bad header")), "{e}");
}

#[tokio::test]
async fn orphan_journals_are_the_ones_the_log_cannot_explain() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("data");
    let (e, _) = open(&data).await;
    assert!(e.orphan_journals().is_empty(), "no participants directory yet");
    let pdir = data.join("participants");
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::write(pdir.join("stray-file"), "x").unwrap();
    let j = txp_participant::Journal::open(pdir.join("fs_x-000000000000")).unwrap();
    j.set(TxId(77), LocalState::Prepared, &()).unwrap();
    let orphans = e.orphan_journals();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0]["txid"], TxId(77).to_string());
    assert_eq!(orphans[0]["state"], "Prepared");
    assert_eq!(orphans[0]["resolution"], "Abort");
}

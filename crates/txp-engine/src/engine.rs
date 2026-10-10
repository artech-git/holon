//! The coordinator proper: admission, locking, staging, voting, the
//! forced commit point, phase two, abort, and replay-driven recovery.

use crate::crash;
use crate::plan::{Plan, RunAsPolicy};
use crate::status::{TxOutcome, TxStatus};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use txp_core::{DurableCommit, LogRecord, ParticipantId, ParticipantSpec, Submitter, TxId, TxPhase};
use txp_lock::{LockManager, Mode, ResourceKey};
use txp_manifest::Manifest;
use txp_participant::{AbortOutcome, Outcome, PartError, Participant, Registry, TxCtx, Vote};
use txp_proc::RunAs;
use txp_wal::{RealDisk, Wal, WalConfig};

/// Errors returned to clients and at startup.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The manifest did not parse or validate.
    #[error("manifest: {0}")]
    Manifest(#[from] txp_manifest::ManifestError),
    /// The decision log refused or failed a write.
    #[error("log: {0}")]
    Wal(String),
    /// The log could not be opened or replayed at startup.
    #[error("recovery: {0}")]
    Recovery(String),
    /// Filesystem error outside the log.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The engine is recovering or shutting down.
    #[error("not accepting transactions: {0}")]
    NotAccepting(String),
    /// The peer is not permitted to submit, or asked a process step to run as
    /// an identity it may not use.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
}

/// Who may submit transactions to this daemon.
///
/// A submission's identity comes from the client socket's peer credentials
/// (`SO_PEERCRED`). Only read-only introspection is open to any peer that can
/// reach the socket; submitting a manifest (which runs code) and the
/// state-changing admin operations are gated by this policy.
#[derive(Clone, Debug)]
pub struct AuthPolicy {
    /// The uid that owns the daemon (the human behind `sudo`, else the
    /// daemon's own euid). Always allowed.
    pub owner_uid: u32,
    /// Additional peer uids allowed to submit (`--allow-uid`).
    pub allow_uids: HashSet<u32>,
    /// Allow any peer that can open the socket (reproduces the pre-auth
    /// behaviour; insecure, opt-in via `--allow-anyone`).
    pub allow_anyone: bool,
}

impl AuthPolicy {
    /// Whether a peer with this uid may submit and run privileged operations.
    /// Root and the daemon owner are always allowed.
    pub fn allows(&self, uid: u32) -> bool {
        self.allow_anyone || uid == 0 || uid == self.owner_uid || self.allow_uids.contains(&uid)
    }
}

/// Engine settings.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Root for the log (`wal/`) and participant journals (`participants/`).
    pub data_dir: PathBuf,
    /// Log sizing.
    pub wal: WalConfig,
    /// Identity for a root submitter's process steps that do not set `user`.
    pub default_run_as: RunAs,
    /// Who may submit transactions.
    pub auth: AuthPolicy,
    /// How long phase two may keep failing before the transaction is shown
    /// as in-doubt (retries continue regardless).
    pub in_doubt_after: Duration,
}

impl EngineConfig {
    /// Defaults: `SUDO_UID`/`SUDO_GID` (else `nobody`, 65534) for `default_run_as`;
    /// the daemon owner (the `sudo` invoker, else the current euid) is the only
    /// non-root peer allowed to submit; 60 seconds for `in_doubt_after`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let sudo_uid: Option<u32> = std::env::var("SUDO_UID").ok().and_then(|s| s.parse().ok());
        let sudo_gid: Option<u32> = std::env::var("SUDO_GID").ok().and_then(|s| s.parse().ok());
        let uid = sudo_uid.unwrap_or(65534);
        let gid = sudo_gid.unwrap_or(65534);
        let owner_uid = sudo_uid.unwrap_or_else(|| nix::unistd::geteuid().as_raw());
        EngineConfig {
            data_dir: data_dir.into(),
            wal: WalConfig::default(),
            default_run_as: RunAs { uid, gid },
            auth: AuthPolicy { owner_uid, allow_uids: HashSet::new(), allow_anyone: false },
            in_doubt_after: Duration::from_secs(60),
        }
    }
}

/// What startup recovery did; also available later via [`Engine::recovery_report`].
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RecoveryReport {
    /// Transactions whose commit was completed (or adopted) during replay.
    pub redriven_commits: Vec<TxId>,
    /// Transactions aborted under presumed abort.
    pub redriven_aborts: Vec<TxId>,
    /// Transactions that could not be finished, with the error.
    pub in_doubt: Vec<(TxId, String)>,
    /// Description of a torn log tail that was discarded, if any.
    pub truncated_tail: Option<String>,
}

struct Inner {
    cfg: EngineConfig,
    wal: Wal,
    locks: LockManager,
    registry: Registry,
    status: Mutex<BTreeMap<TxId, TxStatus>>,
    accepting: AtomicBool,
    recovery: Mutex<RecoveryReport>,
}

/// Cloneable handle to the coordinator.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Lock keys implied by a participant set (used to re-lock during recovery).
fn lock_keys_for(specs: &[ParticipantSpec]) -> Vec<(ResourceKey, Mode)> {
    let mut keys = Vec::new();
    for s in specs {
        match s.kind.as_str() {
            "fs" => {
                if let Some(r) = s.config.get("root").and_then(|v| v.as_str()) {
                    keys.push((ResourceKey::new(format!("fs:{r}")), Mode::Exclusive));
                }
            }
            "proc" => {
                if let Some(ms) = s.config.get("mounts").and_then(|v| v.as_array()) {
                    for m in ms {
                        if let Some(r) = m.get("root").and_then(|v| v.as_str()) {
                            keys.push((ResourceKey::new(format!("fs:{r}")), Mode::Exclusive));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    keys
}

/// Retry `op` on transient errors with jittered backoff until `deadline`
/// (forever if `None`). `on_slow` is invoked once if retries exceed
/// `slow_after`.
async fn retry<T, F, Fut>(what: &str, deadline: Option<Instant>, slow_after: Duration, mut on_slow: impl FnMut(&str), mut op: F) -> Result<T, PartError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, PartError>>,
{
    let start = Instant::now();
    let mut attempt: u32 = 0;
    let mut slow_reported = false;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_transient() => {
                attempt += 1;
                if !slow_reported && start.elapsed() > slow_after {
                    slow_reported = true;
                    on_slow(&e.to_string());
                }
                let base = Duration::from_millis(50u64.saturating_mul(1u64 << attempt.min(7)));
                let jitter = Duration::from_millis((now_unix() ^ attempt as u64) % 50);
                let wait = (base + jitter).min(Duration::from_secs(5));
                if let Some(d) = deadline
                    && Instant::now() + wait > d {
                        return Err(PartError::Transient(format!("{what}: deadline exceeded after {attempt} attempts: {e}")));
                    }
                tracing::warn!(%what, attempt, %e, "transient error; retrying");
                tokio::time::sleep(wait).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// `on_slow` for retries whose slowness nobody needs to hear about.
fn unreported(_: &str) {}

type Parts = HashMap<ParticipantId, Arc<dyn Participant>>;

impl Engine {
    /// Open the log, recover, and (if recovery succeeds) start accepting.
    pub async fn open(cfg: EngineConfig) -> Result<(Engine, RecoveryReport), EngineError> {
        Self::open_with(cfg, |_| {}).await
    }

    /// [`Engine::open`], letting `customize` adjust the participant registry
    /// (which already knows `fs` and `proc`) before recovery uses it, e.g. to
    /// add an adapter kind or wrap an existing one.
    pub async fn open_with(cfg: EngineConfig, customize: impl FnOnce(&mut Registry)) -> Result<(Engine, RecoveryReport), EngineError> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let (wal, recovered) = Wal::open(Arc::new(RealDisk), &cfg.data_dir.join("wal"), cfg.wal.clone())
            .map_err(|e| EngineError::Recovery(e.to_string()))?;
        let mut registry = Registry::new(&cfg.data_dir);
        registry.register("fs", Arc::new(txp_fs::FsParticipant::factory));
        registry.register("proc", Arc::new(txp_proc::ProcParticipant::factory));
        customize(&mut registry);
        let eng = Engine {
            inner: Arc::new(Inner {
                cfg,
                wal,
                locks: LockManager::new(),
                registry,
                status: Mutex::new(BTreeMap::new()),
                accepting: AtomicBool::new(false),
                recovery: Mutex::new(RecoveryReport::default()),
            }),
        };
        let mut report = eng.recover().await?;
        report.truncated_tail = recovered.truncated_tail.as_ref().map(|t| format!("{}@{}: {}", t.path.display(), t.offset, t.reason));
        *eng.inner.recovery.lock() = report.clone();
        eng.inner.accepting.store(true, Ordering::SeqCst);
        Ok((eng, report))
    }

    /// The configured data directory.
    pub fn data_dir(&self) -> &Path {
        &self.inner.cfg.data_dir
    }
    /// The decision log.
    pub fn wal(&self) -> &Wal {
        &self.inner.wal
    }
    /// The lock manager, for observability.
    pub fn locks(&self) -> &LockManager {
        &self.inner.locks
    }
    /// What startup recovery did.
    pub fn recovery_report(&self) -> RecoveryReport {
        self.inner.recovery.lock().clone()
    }

    /// Stop admission; in-flight transactions reach a decision or abort.
    pub fn stop_accepting(&self) {
        self.inner.accepting.store(false, Ordering::SeqCst);
    }

    /// Snapshot the transaction table and drop fully covered log segments.
    pub async fn checkpoint(&self) -> Result<txp_core::Lsn, EngineError> {
        self.inner.wal.checkpoint().await.map_err(|e| EngineError::Wal(e.to_string()))
    }

    /// Status of one transaction known to this process.
    pub fn status(&self, txid: TxId) -> Option<TxStatus> {
        self.inner.status.lock().get(&txid).cloned()
    }

    /// Status of every transaction seen since startup.
    pub fn list(&self) -> Vec<TxStatus> {
        self.inner.status.lock().values().cloned().collect()
    }

    /// Transactions decided but not Done, as the log sees them.
    pub fn in_doubt(&self) -> Vec<serde_json::Value> {
        let table = self.inner.wal.table();
        let t = table.lock();
        let st = self.inner.status.lock();
        t.entries()
            .filter(|e| matches!(e.phase, TxPhase::Committing | TxPhase::Aborting))
            .map(|e| {
                serde_json::json!({
                    "txid": e.txid, "name": e.name, "phase": format!("{:?}", e.phase),
                    "commit_set": e.commit_set, "begin_lsn": e.begin_lsn, "last_lsn": e.last_lsn,
                    "status": st.get(&e.txid).and_then(|s| s.outcome.clone()),
                })
            })
            .collect()
    }

    fn update(&self, txid: TxId, f: impl FnOnce(&mut TxStatus)) {
        self.inner.status.lock().entry(txid).and_modify(f);
    }

    fn build_parts(&self, specs: &[ParticipantSpec]) -> Result<Parts, PartError> {
        let mut m = HashMap::new();
        for s in specs {
            m.insert(s.id.clone(), self.inner.registry.build(s)?);
        }
        Ok(m)
    }

    async fn append(&self, rec: LogRecord) -> Result<txp_core::Lsn, EngineError> {
        self.inner.wal.append(rec).await.map_err(|e| EngineError::Wal(e.to_string()))
    }

    /// Whether a peer with this uid may submit and run privileged operations.
    pub fn authorized(&self, uid: u32) -> bool {
        self.inner.cfg.auth.allows(uid)
    }

    /// The run-as constraints for a submitter. A root submitter (and in-process
    /// submission, `None`) may run steps as any uid and defaults to
    /// `default_run_as`; any other submitter is pinned to its own uid/gid.
    fn runas_policy(&self, who: Option<Submitter>) -> RunAsPolicy {
        match who {
            Some(s) if s.uid != 0 => {
                let r = RunAs { uid: s.uid, gid: s.gid };
                RunAsPolicy { default: r, pin: Some(r) }
            }
            _ => RunAsPolicy { default: self.inner.cfg.default_run_as, pin: None },
        }
    }

    /// Submit a manifest. Returns immediately with the txid and a receiver
    /// for the outcome; the transaction runs in its own task. `who` carries the
    /// authenticated peer credentials; `None` is trusted in-process submission.
    pub fn submit(&self, text: &str, who: Option<Submitter>) -> Result<(TxId, oneshot::Receiver<TxOutcome>), EngineError> {
        if !self.inner.accepting.load(Ordering::SeqCst) {
            return Err(EngineError::NotAccepting("engine is shutting down or recovering".into()));
        }
        if let Some(s) = &who
            && !self.inner.cfg.auth.allows(s.uid)
        {
            return Err(EngineError::Unauthorized(format!("uid {} may not submit transactions", s.uid)));
        }
        let runas = self.runas_policy(who);
        let manifest = Manifest::parse(text)?;
        let txid = TxId::generate();
        let plan = Plan::build(&manifest, text, txid, &runas)?;
        self.inner.status.lock().insert(
            txid,
            TxStatus {
                txid,
                name: plan.name.clone(),
                phase: "submitted".into(),
                steps_done: vec![],
                current_step: None,
                outcome: None,
                outputs: Default::default(),
                started_unix: now_unix(),
                finished_unix: None,
                one_phase: false,
            },
        );
        let (tx, rx) = oneshot::channel();
        let eng = self.clone();
        tokio::spawn(async move {
            let out = eng.run_txn(txid, plan, who).await;
            eng.update(txid, |s| {
                s.outcome = Some(out.clone());
                s.finished_unix = Some(now_unix());
                s.phase = "done".into();
            });
            let _ = tx.send(out);
        });
        Ok((txid, rx))
    }

    /// [`Engine::submit`] (trusted, in-process) and wait for the outcome.
    pub async fn run(&self, text: &str) -> Result<(TxId, TxOutcome), EngineError> {
        let (txid, rx) = self.submit(text, None)?;
        let out = rx.await.unwrap_or(TxOutcome::InDoubt { decision: "unknown".into(), error: "actor vanished".into() });
        Ok((txid, out))
    }

    async fn run_txn(&self, txid: TxId, plan: Plan, who: Option<Submitter>) -> TxOutcome {
        let deadline = Instant::now() + plan.timeout;
        let span = tracing::info_span!("txn", %txid, name = %plan.name);
        let _g = span.enter();
        drop(_g);

        // Locks: conservative, canonical order, before anything is logged.
        self.update(txid, |s| s.phase = "locking".into());
        let lk = self.inner.locks.clone();
        let keys = plan.locks.clone();
        if tokio::time::timeout_at(deadline.into(), lk.acquire_all(txid, keys)).await.is_err() {
            self.inner.locks.release_all(txid);
            return TxOutcome::Aborted { reason: "timed out waiting for locks".into() };
        }

        let parts = match self.build_parts(&plan.participants) {
            Ok(p) => p,
            Err(e) => {
                self.inner.locks.release_all(txid);
                return TxOutcome::Aborted { reason: format!("participant setup: {e}") };
            }
        };

        if let Err(e) = self
            .append(LogRecord::Begin {
                txid,
                name: plan.name.clone(),
                manifest_digest: plan.digest.clone(),
                submitter: who,
                participants: plan.participants.clone(),
            })
            .await
        {
            self.inner.locks.release_all(txid);
            return TxOutcome::Aborted { reason: format!("log: {e}") };
        }
        crash::maybe("after_begin");

        // Stage.
        self.update(txid, |s| s.phase = "staging".into());
        let mut ctx = TxCtx { txid, data_dir: self.inner.cfg.data_dir.clone(), deadline: Some(deadline), outputs: Default::default() };
        let mut staged: Vec<ParticipantId> = Vec::new();
        for step in &plan.steps {
            self.update(txid, |s| s.current_step = Some(step.spec.id.clone()));
            let p = parts[&step.participant].clone();
            if !staged.contains(&step.participant) {
                staged.push(step.participant.clone());
            }
            let r = tokio::time::timeout_at(deadline.into(), p.stage(&ctx, &step.spec)).await;
            match r {
                Ok(Ok(rep)) => {
                    tracing::info!(step = %step.spec.id, summary = %rep.summary, "staged");
                    ctx.outputs.insert(step.spec.id.clone(), rep.outputs.clone());
                    self.update(txid, |s| {
                        s.steps_done.push(step.spec.id.clone());
                        s.outputs.insert(step.spec.id.clone(), rep.outputs);
                    });
                }
                Ok(Err(e)) => {
                    let reason = format!("step {}: {e}", step.spec.id);
                    return self.abort_txn(txid, &parts, reason).await;
                }
                Err(_) => {
                    let reason = format!("step {}: transaction timeout", step.spec.id);
                    return self.abort_txn(txid, &parts, reason).await;
                }
            }
        }
        self.update(txid, |s| s.current_step = None);
        crash::maybe("after_stage");

        // Fast path: nothing staged.
        if staged.is_empty() {
            return self.finish_trivial(txid).await;
        }

        // One-phase fast path: exactly one participant staged anything.
        if staged.len() == 1 && parts[&staged[0]].capabilities().one_phase {
            self.update(txid, |s| {
                s.phase = "one_phase_commit".into();
                s.one_phase = true;
            });
            let p = parts[&staged[0]].clone();
            let c = ctx.clone();
            let r = retry("commit_one_phase", None, self.inner.cfg.in_doubt_after, unreported, || {
                let p = p.clone();
                let c = c.clone();
                async move { p.commit_one_phase(&c).await }
            })
            .await;
            return match r {
                Ok(Outcome::Committed) => {
                    crash::maybe("before_done");
                    self.finish(txid, TxOutcome::Committed).await
                }
                Ok(Outcome::Aborted(why)) => self.abort_txn(txid, &parts, why).await,
                Err(e) => {
                    // Local state is the truth; recovery resolves it via abort()
                    // (which finishes a locally-committed 1PC).
                    self.inner.locks.release_all(txid);
                    TxOutcome::InDoubt { decision: "one_phase".into(), error: e.to_string() }
                }
            };
        }

        // Two-phase: prepare all staged participants concurrently.
        self.update(txid, |s| s.phase = "preparing".into());
        let mut js = JoinSet::new();
        for id in &staged {
            let p = parts[id].clone();
            let c = ctx.clone();
            let id = id.clone();
            js.spawn(async move {
                let r = retry("prepare", Some(deadline), Duration::from_secs(3600), unreported, || {
                    let p = p.clone();
                    let c = c.clone();
                    async move { p.prepare(&c).await }
                })
                .await;
                (id, r)
            });
        }
        let mut votes: Vec<(ParticipantId, Vote)> = Vec::new();
        let mut failure: Option<String> = None;
        while let Some(r) = js.join_next().await {
            match r {
                Ok((id, Ok(v))) => votes.push((id, v)),
                Ok((id, Err(e))) => {
                    failure.get_or_insert(format!("{id}: {e}"));
                }
                Err(e) => {
                    failure.get_or_insert(format!("prepare task: {e}"));
                }
            }
        }
        if let Some(why) = failure {
            return self.abort_txn(txid, &parts, why).await;
        }
        let _ = self
            .append(LogRecord::Prepared { txid, votes: votes.iter().map(|(i, v)| (i.clone(), format!("{v:?}"))).collect() })
            .await;
        crash::maybe("after_prepare");

        let commit_set: Vec<ParticipantId> = votes.iter().filter(|(_, v)| *v == Vote::Prepared).map(|(i, _)| i.clone()).collect();
        if commit_set.is_empty() {
            return self.finish_trivial(txid).await;
        }

        // The commit point.
        self.update(txid, |s| s.phase = "committing".into());
        crash::maybe("before_commit_record");
        let proof: DurableCommit = match self.inner.wal.append_commit(txid, commit_set.clone()).await {
            Ok(p) => p,
            Err(e) => return self.abort_txn(txid, &parts, format!("commit record: {e}")).await,
        };
        crash::maybe("after_commit_record");
        self.phase_two(&proof, &parts, &commit_set).await
    }

    /// Phase two: only reachable with a `DurableCommit`. Retries forever.
    async fn phase_two(&self, proof: &DurableCommit, parts: &Parts, commit_set: &[ParticipantId]) -> TxOutcome {
        let txid = proof.txid();
        let mut js = JoinSet::new();
        let slow = self.inner.cfg.in_doubt_after;
        let first_done = Arc::new(AtomicBool::new(false));
        for id in commit_set {
            let p = parts[id].clone();
            let id = id.clone();
            let eng = self.clone();
            let first_done = first_done.clone();
            js.spawn(async move {
                let r = retry("commit", None, slow, |e| eng.update(txid, |s| s.outcome = Some(TxOutcome::InDoubt { decision: "commit".into(), error: e.to_string() })), || {
                    let p = p.clone();
                    async move { p.commit(txid).await }
                })
                .await;
                if !first_done.swap(true, Ordering::SeqCst) {
                    crash::maybe("mid_commit_fanout");
                }
                (id, r)
            });
        }
        let mut err = None;
        while let Some(r) = js.join_next().await {
            match r {
                Ok((_, Ok(()))) => {}
                Ok((id, Err(e))) => {
                    err.get_or_insert(format!("{id}: {e}"));
                }
                Err(e) => {
                    err.get_or_insert(format!("commit task: {e}"));
                }
            }
        }
        if let Some(e) = err {
            // Non-transient failure after the decision: the decision stands,
            // the operator must repair the participant and re-drive.
            tracing::error!(%txid, %e, "commit fan-out failed; transaction is in doubt");
            self.inner.locks.release_all(txid);
            return TxOutcome::InDoubt { decision: "commit".into(), error: e };
        }
        crash::maybe("before_done");
        self.finish(txid, TxOutcome::Committed).await
    }

    async fn finish(&self, txid: TxId, out: TxOutcome) -> TxOutcome {
        if let Err(e) = self.append(LogRecord::Done { txid }).await {
            tracing::error!(%txid, %e, "could not write Done; recovery will re-drive");
        }
        crash::maybe("after_done");
        self.inner.locks.release_all(txid);
        out
    }

    /// Nothing to commit: `Begin` → `Done` directly.
    async fn finish_trivial(&self, txid: TxId) -> TxOutcome {
        self.update(txid, |s| s.phase = "read_only".into());
        self.finish(txid, TxOutcome::Committed).await
    }

    async fn abort_txn(&self, txid: TxId, parts: &Parts, reason: String) -> TxOutcome {
        tracing::info!(%txid, %reason, "aborting");
        self.update(txid, |s| s.phase = "aborting".into());
        // Presumed abort: the Abort record is lazy, so fan out first; a
        // participant that already committed locally (1PC) wins.
        let adopted = self.abort_fanout(txid, parts).await;
        if !adopted.is_empty() {
            return self.adopt_local_commit(txid, adopted).await;
        }
        crash::maybe("before_abort_record");
        if let Err(e) = self.append(LogRecord::Abort { txid }).await {
            tracing::warn!(%txid, %e, "abort record rejected/failed");
        }
        self.finish(txid, TxOutcome::Aborted { reason }).await
    }

    /// A participant committed locally on the one-phase path before we lost
    /// track of it: the decision is theirs. Record it (forced, audited).
    async fn adopt_local_commit(&self, txid: TxId, adopted: Vec<ParticipantId>) -> TxOutcome {
        tracing::warn!(%txid, ?adopted, "participant had already committed (1PC); adopting commit");
        let rec = LogRecord::ForceResolve {
            txid,
            decision: txp_core::Decision::Commit,
            reason: format!("one-phase participant(s) {:?} committed locally", adopted),
        };
        if let Err(e) = self.append(rec).await {
            tracing::error!(%txid, %e, "could not record adopted commit");
        }
        self.finish(txid, TxOutcome::Committed).await
    }

    /// Returns the participants that reported a local commit (1PC).
    async fn abort_fanout(&self, txid: TxId, parts: &Parts) -> Vec<ParticipantId> {
        let mut js = JoinSet::new();
        let first_done = Arc::new(AtomicBool::new(false));
        for (id, p) in parts {
            let p = p.clone();
            let id = id.clone();
            let first_done = first_done.clone();
            js.spawn(async move {
                let r = retry("abort", None, Duration::from_secs(60), unreported, || {
                    let p = p.clone();
                    async move { p.abort(txid).await }
                })
                .await;
                if !first_done.swap(true, Ordering::SeqCst) {
                    crash::maybe("mid_abort_fanout");
                }
                match r {
                    Ok(AbortOutcome::AlreadyCommitted) => Some(id),
                    Ok(AbortOutcome::Discarded) => None,
                    Err(e) => {
                        tracing::error!(%txid, %id, %e, "abort failed (non-transient); leaving for recovery/doctor");
                        None
                    }
                }
            });
        }
        let mut committed = Vec::new();
        while let Some(r) = js.join_next().await {
            if let Ok(Some(id)) = r {
                committed.push(id);
            }
        }
        committed
    }

    /// Replay-derived recovery (design §1.4). Runs before admission opens.
    async fn recover(&self) -> Result<RecoveryReport, EngineError> {
        let actions = self.inner.wal.table().lock().recovery_actions();
        let mut report = RecoveryReport::default();
        for a in actions {
            let txid = a.txid();
            let specs: Vec<ParticipantSpec> = match &a {
                txp_core::RecoveryAction::AbortStarted { participants, .. }
                | txp_core::RecoveryAction::RedriveCommit { participants, .. }
                | txp_core::RecoveryAction::RedriveAbort { participants, .. } => participants.clone(),
            };
            self.inner.locks.acquire_all(txid, lock_keys_for(&specs)).await;
            self.inner.status.lock().insert(
                txid,
                TxStatus {
                    txid,
                    name: self.inner.wal.table().lock().get(txid).map(|e| e.name.clone()).unwrap_or_default(),
                    phase: "recovering".into(),
                    steps_done: vec![],
                    current_step: None,
                    outcome: None,
                    outputs: Default::default(),
                    started_unix: now_unix(),
                    finished_unix: None,
                    one_phase: false,
                },
            );
            let parts = match self.build_parts(&specs) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(%txid, %e, "cannot rebuild participants; in doubt");
                    report.in_doubt.push((txid, e.to_string()));
                    self.update(txid, |s| s.outcome = Some(TxOutcome::InDoubt { decision: format!("{a:?}"), error: e.to_string() }));
                    self.inner.locks.release_all(txid);
                    continue;
                }
            };
            match a {
                txp_core::RecoveryAction::RedriveCommit { commit_set, .. } => {
                    tracing::info!(%txid, "recovery: re-driving commit");
                    let proof = DurableCommit::mint(txid, self.inner.wal.table().lock().get(txid).map(|e| e.last_lsn).unwrap_or_default());
                    // Phase two only ever commits or stays in doubt.
                    match self.phase_two(&proof, &parts, &commit_set).await {
                        TxOutcome::InDoubt { error, .. } => report.in_doubt.push((txid, error)),
                        _ => report.redriven_commits.push(txid),
                    }
                    self.update(txid, |s| s.outcome = Some(TxOutcome::Committed));
                }
                txp_core::RecoveryAction::AbortStarted { .. } | txp_core::RecoveryAction::RedriveAbort { .. } => {
                    tracing::info!(%txid, "recovery: re-driving abort (presumed abort)");
                    let is_started = matches!(a, txp_core::RecoveryAction::AbortStarted { .. });
                    let adopted = self.abort_fanout(txid, &parts).await;
                    if !adopted.is_empty() && is_started {
                        let _ = self.adopt_local_commit(txid, adopted).await;
                        report.redriven_commits.push(txid);
                        self.update(txid, |s| s.outcome = Some(TxOutcome::Committed));
                    } else {
                        if is_started {
                            let _ = self.append(LogRecord::Abort { txid }).await;
                        }
                        let _ = self.finish(txid, TxOutcome::Aborted { reason: "recovered".into() }).await;
                        report.redriven_aborts.push(txid);
                        self.update(txid, |s| s.outcome = Some(TxOutcome::Aborted { reason: "recovered: presumed abort".into() }));
                    }
                }
            }
            self.update(txid, |s| {
                s.phase = "done".into();
                s.finished_unix = Some(now_unix());
            });
        }
        // Participant-side recovery: journals for txids the log no longer
        // tracks are reported (orphans); the doctor command shows them.
        Ok(report)
    }

    /// Participant journal entries not explained by the log.
    pub fn orphan_journals(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        let pdir = self.inner.cfg.data_dir.join("participants");
        let table = self.inner.wal.table();
        if let Ok(rd) = std::fs::read_dir(&pdir) {
            for e in rd.flatten() {
                if let Ok(j) = txp_participant::Journal::open(e.path()) {
                    for (txid, st) in j.list().unwrap_or_default() {
                        let known = table.lock().get(txid).map(|e| e.phase != TxPhase::Done).unwrap_or(false);
                        if !known {
                            out.push(serde_json::json!({ "participant_dir": e.path(), "txid": txid, "state": format!("{st:?}"),
                                "resolution": format!("{:?}", table.lock().resolve(txid)) }));
                        }
                    }
                }
            }
        }
        out
    }

    /// Graceful shutdown: stop admission, let the log drain.
    pub async fn shutdown(&self) {
        self.stop_accepting();
        self.inner.wal.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: &str, config: serde_json::Value) -> ParticipantSpec {
        ParticipantSpec { id: ParticipantId::new(kind), kind: kind.into(), config }
    }

    #[test]
    fn recovery_relocks_every_managed_root() {
        let specs = [
            spec("fs", serde_json::json!({"root": "/srv/a"})),
            spec("fs", serde_json::json!({})),
            spec("proc", serde_json::json!({"mounts": [{"root": "/srv/b"}, {"resource": "no root"}]})),
            spec("proc", serde_json::json!({})),
            spec("pg", serde_json::json!({"root": "/ignored"})),
        ];
        let keys: Vec<String> = lock_keys_for(&specs).into_iter().map(|(k, m)| format!("{} {m:?}", k.0)).collect();
        assert_eq!(keys, ["fs:/srv/a Exclusive", "fs:/srv/b Exclusive"]);
    }

    #[tokio::test]
    async fn retry_backs_off_on_transient_errors_only() {
        // Transient twice, then success; slowness is reported once.
        let calls = std::cell::Cell::new(0);
        let mut slow = Vec::new();
        let mut report = |e: &str| slow.push(e.to_string());
        let r = retry("op", None, Duration::ZERO, &mut report, || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move { if n < 3 { Err(PartError::Transient(format!("try {n}"))) } else { Ok(n) } }
        })
        .await;
        assert_eq!(r.unwrap(), 3);
        // A deadline too close for the next backoff gives up.
        let soon = Some(Instant::now() + Duration::from_millis(1));
        let r: Result<(), _> = retry("op", soon, Duration::from_secs(60), &mut report, || async { Err(PartError::Transient("busy".into())) }).await;
        assert!(r.unwrap_err().to_string().contains("op: deadline exceeded after 1 attempts: transient: busy"));
        // Anything else is returned at once.
        let r: Result<(), _> = retry("op", None, Duration::ZERO, &mut report, || async { Err(PartError::VoteNo("no".into())) }).await;
        assert!(matches!(r, Err(PartError::VoteNo(_))));
        unreported("not reported");
        assert_eq!(slow, ["transient: try 1"]);
    }

    #[test]
    fn auth_policy_allows_root_owner_and_listed_only() {
        let mut p = AuthPolicy { owner_uid: 1000, allow_uids: HashSet::new(), allow_anyone: false };
        assert!(p.allows(0), "root always allowed");
        assert!(p.allows(1000), "owner allowed");
        assert!(!p.allows(1001), "stranger denied");
        p.allow_uids.insert(1001);
        assert!(p.allows(1001), "explicitly allowed uid");
        let anyone = AuthPolicy { owner_uid: 1000, allow_uids: HashSet::new(), allow_anyone: true };
        assert!(anyone.allows(31337), "allow_anyone opens the door");
    }
}

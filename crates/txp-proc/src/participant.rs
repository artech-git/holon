//! The `proc` participant: one sandboxed process step.

use crate::cgroup::Cgroup;
use crate::diff::translate;
use crate::sandbox::{self, OverlayMount, SandboxSpec};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use txp_core::{ParticipantId, ParticipantSpec, TxId};
use txp_fs::{execute_redo, fsync_tree, stage_root_for, RedoOp};
use txp_participant::{
    id_dirname, AbortOutcome, Capabilities, Journal, LocalState, Outcome, PartError, Participant, StageReport, StepSpec, TxCtx, Vote,
};

/// A managed root to expose inside the sandbox as a staged view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MountSpec {
    /// Manifest resource id (informational).
    pub resource: String,
    /// The real directory; used as the bottom overlay layer.
    pub root: PathBuf,
    /// Where the staged view appears inside the sandbox (default: `root`).
    #[serde(default)]
    pub at: Option<PathBuf>,
    /// Uppers of earlier steps on the same resource, most recent first.
    #[serde(default)]
    pub extra_lowers: Vec<PathBuf>,
}

/// Unprivileged identity the step runs as.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct RunAs {
    /// User id.
    pub uid: u32,
    /// Group id.
    pub gid: u32,
}

/// Adapter configuration stored in the `Begin` record (built by the
/// engine's planner from a `process` step).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcConfig {
    /// Step id; names the staging directory and the cgroup.
    pub step: String,
    /// Program and arguments. `$txid` is substituted.
    pub argv: Vec<String>,
    /// Working directory inside the staged view.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Environment. `$txid` is substituted; `TXP_TXID`, `TXP_STEP` and a
    /// default `PATH` are added.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Roots to stage. At least one is required.
    pub mounts: Vec<MountSpec>,
    /// Identity to drop to.
    pub run_as: RunAs,
    /// Step timeout in seconds (default 600), capped by the transaction deadline.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Apply a Landlock ruleset (default `true`); refuses to run if unavailable.
    #[serde(default = "default_true")]
    pub landlock: bool,
    /// Install the seccomp syscall-denylist filter (default `true`); refuses to
    /// run if the kernel cannot apply it.
    #[serde(default = "default_true")]
    pub seccomp: bool,
    /// Give the process a private tmpfs `/tmp` (default `true`).
    #[serde(default = "default_true")]
    pub private_tmp: bool,
}

fn default_timeout() -> u64 {
    600
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Local {
    /// Per mount: (root, stage dir for this step, upper)
    stages: Vec<(PathBuf, PathBuf, PathBuf)>,
    ops: Vec<RedoOp>,
    exit_code: Option<i32>,
}

/// The `proc` participant: one sandboxed process step.
pub struct ProcParticipant {
    id: ParticipantId,
    cfg: ProcConfig,
    journal: Journal,
    running: Arc<Mutex<HashMap<TxId, Cgroup>>>,
}

fn io(e: std::io::Error) -> PartError {
    PartError::Fatal(e.to_string())
}

impl ProcParticipant {
    /// Open the adapter: creates its journal under `data_dir/participants`
    /// and rejects a config with no mounts.
    pub fn new(id: ParticipantId, cfg: ProcConfig, data_dir: &Path) -> Result<Self, PartError> {
        let journal = Journal::open(data_dir.join("participants").join(id_dirname(&id))).map_err(io)?;
        if cfg.mounts.is_empty() {
            return Err(PartError::Fatal("process step declares no mounts; nothing could be staged".into()));
        }
        Ok(ProcParticipant { id, cfg, journal, running: Default::default() })
    }

    /// Registry factory: decodes [`ProcConfig`] from `spec.config`.
    pub fn factory(spec: &ParticipantSpec, data_dir: &Path) -> Result<Arc<dyn Participant>, PartError> {
        let cfg: ProcConfig = serde_json::from_value(spec.config.clone()).map_err(|e| PartError::Fatal(e.to_string()))?;
        Ok(Arc::new(ProcParticipant::new(spec.id.clone(), cfg, data_dir)?))
    }

    /// Deterministic staging layout; the engine uses this to compute
    /// `extra_lowers` for later steps.
    pub fn upper_dir(root: &Path, txid: TxId, step: &str) -> PathBuf {
        stage_root_for(root).join(txid.to_string()).join(step).join("upper")
    }

    fn load(&self, txid: TxId) -> Result<Option<(LocalState, Local)>, PartError> {
        Ok(self.journal.get::<Local>(txid).map_err(io)?.map(|e| (e.state, e.data)))
    }

    fn discard(&self, l: &Local) {
        for (_, stage, _) in &l.stages {
            let _ = txp_fs::StageDir { path: stage.clone() }.discard();
        }
    }

    fn do_commit(&self, txid: TxId, l: &Local) -> Result<(), PartError> {
        execute_redo(&l.ops).map_err(|e| PartError::Transient(format!("publish: {e}")))?;
        self.journal.set(txid, LocalState::Committed, l).map_err(io)?;
        self.discard(l);
        self.journal.remove(txid).map_err(io)
    }

    fn do_prepare(&self, txid: TxId, mut l: Local) -> Result<Vote, PartError> {
        let mut ops = Vec::new();
        for (root, _stage, upper) in &l.stages {
            fsync_tree(upper).map_err(io)?;
            ops.extend(translate(upper, root).map_err(PartError::VoteNo)?);
        }
        if ops.is_empty() {
            self.discard(&l);
            self.journal.remove(txid).map_err(io)?;
            return Ok(Vote::ReadOnly);
        }
        for (_, _, upper) in &l.stages {
            fsync_tree(upper).map_err(io)?; // xattr strips above
        }
        l.ops = ops;
        self.journal.set(txid, LocalState::Prepared, &l).map_err(io)?;
        Ok(Vote::Prepared)
    }
}

#[async_trait]
impl Participant for ProcParticipant {
    fn id(&self) -> ParticipantId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { atomic: true, read_only_capable: true, one_phase: true, ..Default::default() }
    }

    async fn stage(&self, tx: &TxCtx, step: &StepSpec) -> Result<StageReport, PartError> {
        if self.load(tx.txid)?.is_some() {
            return Err(PartError::Fatal(format!("process step {} already staged for {}", self.cfg.step, tx.txid)));
        }
        let cfg = &self.cfg;
        let mut mounts = Vec::new();
        let mut stages = Vec::new();
        for m in &cfg.mounts {
            if !m.root.is_dir() {
                return Err(PartError::VoteNo(format!("managed root {} is not a directory", m.root.display())));
            }
            let stage = stage_root_for(&m.root).join(tx.txid.to_string()).join(&cfg.step);
            let upper = stage.join("upper");
            let work = stage.join("work");
            std::fs::create_dir_all(&upper).map_err(io)?;
            std::fs::create_dir_all(&work).map_err(io)?;
            if !txp_fs::staging::same_device(&upper, &m.root).map_err(io)? {
                return Err(PartError::Fatal(format!("staging for {} is not on the same filesystem", m.root.display())));
            }
            let mut lowers = m.extra_lowers.clone();
            lowers.push(m.root.clone());
            for l in &lowers {
                std::fs::create_dir_all(l).map_err(io)?;
            }
            mounts.push(OverlayMount { lowers, upper: upper.clone(), work, at: m.at.clone().unwrap_or_else(|| m.root.clone()) });
            stages.push((m.root.clone(), stage, upper));
        }
        let cg_name = format!("{}-{}", tx.txid, cfg.step.replace('/', "_"));
        let cg = Cgroup::create(&cg_name).map_err(|e| PartError::Fatal(format!("cgroup: {e}")))?;
        self.running.lock().insert(tx.txid, cg.clone());
        self.journal.set(tx.txid, LocalState::Staged, &Local { stages: stages.clone(), ops: vec![], exit_code: None }).map_err(io)?;

        let mut env: Vec<(String, String)> = cfg.env.iter().map(|(k, v)| (k.clone(), v.replace("$txid", &tx.txid.to_string()))).collect();
        env.push(("TXP_TXID".into(), tx.txid.to_string()));
        env.push(("TXP_STEP".into(), cfg.step.clone()));
        if !env.iter().any(|(k, _)| k == "PATH") {
            env.push(("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()));
        }
        let argv: Vec<String> = cfg.argv.iter().map(|a| a.replace("$txid", &tx.txid.to_string())).collect();
        let timeout = tx
            .deadline
            .map(|d| d.saturating_duration_since(std::time::Instant::now()))
            .unwrap_or(Duration::from_secs(cfg.timeout_secs))
            .min(Duration::from_secs(cfg.timeout_secs));
        let spec = SandboxSpec {
            argv,
            env,
            cwd: cfg.cwd.clone(),
            mounts,
            uid: cfg.run_as.uid,
            gid: cfg.run_as.gid,
            timeout,
            cgroup: cg.clone(),
            landlock: cfg.landlock,
            seccomp: cfg.seccomp,
            private_tmp: cfg.private_tmp,
            output_limit: 64 * 1024,
        };
        // Detached task: a dropped caller future must not leave the process
        // running unobserved; abort() kills via the cgroup in that case. A
        // panicking task is reported like any other sandbox error, after cleanup.
        let res = tokio::spawn(sandbox::run(spec)).await.map_err(std::io::Error::from).and_then(std::convert::identity);
        let _ = cg.destroy().await;
        self.running.lock().remove(&tx.txid);
        let res = res.map_err(|e| PartError::Fatal(format!("sandbox: {e}")))?;
        let mut l = Local { stages, ops: vec![], exit_code: res.exit_code };
        self.journal.set(tx.txid, LocalState::Staged, &l).map_err(io)?;
        if !res.success() {
            let tail: String = res.stderr.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
            let why = if res.timed_out {
                format!("step {} timed out after {:?}", cfg.step, timeout)
            } else {
                format!("step {} exited with code {:?} signal {:?}: {}", cfg.step, res.exit_code, res.signal, tail.trim())
            };
            l.ops.clear();
            return Err(PartError::VoteNo(why));
        }
        let _ = step;
        Ok(StageReport {
            summary: format!("process {} exited 0", cfg.step),
            outputs: serde_json::json!({ "stdout": res.stdout, "stderr": res.stderr, "exit_code": 0 }),
        })
    }

    async fn prepare(&self, tx: &TxCtx) -> Result<Vote, PartError> {
        match self.load(tx.txid)? {
            None => Ok(Vote::ReadOnly),
            Some((LocalState::Prepared, _)) => Ok(Vote::Prepared),
            Some((LocalState::Staged, l)) => {
                if l.exit_code != Some(0) {
                    return Err(PartError::VoteNo("process did not complete successfully".into()));
                }
                self.do_prepare(tx.txid, l)
            }
            Some((st, _)) => Err(PartError::Fatal(format!("prepare in state {st:?}"))),
        }
    }

    async fn commit(&self, txid: TxId) -> Result<(), PartError> {
        match self.load(txid)? {
            None | Some((LocalState::Aborted, _)) => Ok(()),
            Some((LocalState::Prepared | LocalState::Committed, l)) => self.do_commit(txid, &l),
            Some((LocalState::Staged, _)) => Err(PartError::Fatal("commit before prepare".into())),
        }
    }

    async fn abort(&self, txid: TxId) -> Result<AbortOutcome, PartError> {
        let cg = self.running.lock().remove(&txid);
        if let Some(cg) = cg {
            let _ = cg.destroy().await;
        }
        match self.load(txid)? {
            None => Ok(AbortOutcome::Discarded),
            Some((LocalState::Committed, l)) => {
                self.do_commit(txid, &l)?;
                Ok(AbortOutcome::AlreadyCommitted)
            }
            Some((_, l)) => {
                self.discard(&l);
                self.journal.remove(txid).map_err(io)?;
                Ok(AbortOutcome::Discarded)
            }
        }
    }

    async fn commit_one_phase(&self, tx: &TxCtx) -> Result<Outcome, PartError> {
        match self.load(tx.txid)? {
            None => Ok(Outcome::Committed),
            Some((LocalState::Staged, l)) => {
                if l.exit_code != Some(0) {
                    return Ok(Outcome::Aborted("process failed".into()));
                }
                match self.do_prepare(tx.txid, l)? {
                    Vote::ReadOnly => Ok(Outcome::Committed),
                    Vote::Prepared => {
                        let (_, l) = self.load(tx.txid)?.unwrap();
                        self.journal.set(tx.txid, LocalState::Committed, &l).map_err(io)?;
                        self.do_commit(tx.txid, &l)?;
                        Ok(Outcome::Committed)
                    }
                }
            }
            Some((LocalState::Prepared | LocalState::Committed, l)) => {
                self.journal.set(tx.txid, LocalState::Committed, &l).map_err(io)?;
                self.do_commit(tx.txid, &l)?;
                Ok(Outcome::Committed)
            }
            Some((LocalState::Aborted, _)) => Ok(Outcome::Aborted("locally aborted".into())),
        }
    }

    async fn recover(&self) -> Result<Vec<(TxId, LocalState)>, PartError> {
        self.journal.list().map_err(io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path) -> ProcConfig {
        let c = serde_json::json!({ "step": "build", "argv": ["true"], "mounts": [{ "resource": "r", "root": root }], "run_as": { "uid": 1, "gid": 1 } });
        serde_json::from_value(c).unwrap()
    }

    struct Fixture {
        _d: tempfile::TempDir,
        root: PathBuf,
        p: ProcParticipant,
    }

    fn fixture() -> Fixture {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("site");
        std::fs::create_dir_all(&root).unwrap();
        let p = ProcParticipant::new(ParticipantId::new("proc:build"), config(&root), d.path()).unwrap();
        Fixture { _d: d, root, p }
    }

    fn ctx(t: u128) -> TxCtx {
        TxCtx { txid: TxId(t), data_dir: PathBuf::new(), deadline: None, outputs: Default::default() }
    }

    /// Leave `txid` as `stage` would: an upper holding `files` and the
    /// process's exit code, journaled in `state`.
    fn staged(f: &Fixture, t: u128, state: LocalState, exit_code: i32, files: &[&str]) {
        let stage = stage_root_for(&f.root).join(TxId(t).to_string()).join("build");
        let upper = stage.join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        for name in files {
            std::fs::write(upper.join(name), *name).unwrap();
        }
        let l = Local { stages: vec![(f.root.clone(), stage, upper)], ops: vec![], exit_code: Some(exit_code) };
        f.p.journal.set(TxId(t), state, &l).unwrap();
    }

    /// Rewrite `txid`'s journal state, keeping its data.
    fn restate(f: &Fixture, t: u128, state: LocalState) {
        let (_, l) = f.p.load(TxId(t)).unwrap().unwrap();
        f.p.journal.set(TxId(t), state, &l).unwrap();
    }

    #[test]
    fn config_defaults_and_adapter_basics() {
        let c = config(Path::new("/srv"));
        assert_eq!((c.timeout_secs, c.landlock, c.seccomp, c.private_tmp), (600, true, true, true));
        let d = tempfile::tempdir().unwrap();
        let spec = |config| ParticipantSpec { id: ParticipantId::new("proc:b"), kind: "proc".into(), config };
        let p = ProcParticipant::factory(&spec(serde_json::to_value(&c).unwrap()), d.path()).unwrap();
        assert_eq!(p.id().as_str(), "proc:b");
        assert!(p.capabilities().one_phase && p.capabilities().atomic);
        assert!(ProcParticipant::factory(&spec(serde_json::json!({})), d.path()).is_err());
        let none = ProcConfig { mounts: vec![], ..c.clone() };
        assert!(ProcParticipant::new(ParticipantId::new("proc:b"), none, d.path()).err().unwrap().to_string().contains("no mounts"));
        // The journal cannot be created under a file.
        std::fs::write(d.path().join("file"), "").unwrap();
        assert!(matches!(ProcParticipant::new(ParticipantId::new("proc:b"), c, &d.path().join("file")), Err(PartError::Fatal(_))));
        assert_eq!(ProcParticipant::upper_dir(Path::new("/srv/site"), TxId(1), "s"), Path::new("/srv/.txp-stage/site").join(TxId(1).to_string()).join("s/upper"));
    }

    #[tokio::test]
    async fn prepare_votes_from_what_the_process_left() {
        let f = fixture();
        assert_eq!(f.p.prepare(&ctx(1)).await.unwrap(), Vote::ReadOnly, "never staged");
        staged(&f, 2, LocalState::Staged, 1, &["out"]);
        assert!(matches!(f.p.prepare(&ctx(2)).await, Err(PartError::VoteNo(_))), "failed process");
        staged(&f, 3, LocalState::Staged, 0, &[]);
        assert_eq!(f.p.prepare(&ctx(3)).await.unwrap(), Vote::ReadOnly, "wrote nothing");
        assert!(f.p.load(TxId(3)).unwrap().is_none());
        staged(&f, 4, LocalState::Staged, 0, &["out"]);
        assert_eq!(f.p.prepare(&ctx(4)).await.unwrap(), Vote::Prepared);
        assert_eq!(f.p.prepare(&ctx(4)).await.unwrap(), Vote::Prepared, "idempotent");
        restate(&f, 4, LocalState::Committed);
        assert!(matches!(f.p.prepare(&ctx(4)).await, Err(PartError::Fatal(_))));
        assert_eq!(f.p.recover().await.unwrap(), vec![(TxId(2), LocalState::Staged), (TxId(4), LocalState::Committed)]);
    }

    #[tokio::test]
    async fn commit_and_abort_are_total() {
        let f = fixture();
        f.p.commit(TxId(1)).await.unwrap();
        staged(&f, 2, LocalState::Aborted, 0, &[]);
        f.p.commit(TxId(2)).await.unwrap();
        staged(&f, 3, LocalState::Staged, 0, &["a"]);
        assert!(f.p.commit(TxId(3)).await.unwrap_err().to_string().contains("commit before prepare"));
        f.p.prepare(&ctx(3)).await.unwrap();
        f.p.commit(TxId(3)).await.unwrap();
        assert_eq!(std::fs::read_to_string(f.root.join("a")).unwrap(), "a");

        assert_eq!(f.p.abort(TxId(9)).await.unwrap(), AbortOutcome::Discarded);
        staged(&f, 4, LocalState::Staged, 0, &["b"]);
        assert_eq!(f.p.abort(TxId(4)).await.unwrap(), AbortOutcome::Discarded);
        assert!(!f.root.join("b").exists());
        // A local one-phase commit is finished, not undone.
        staged(&f, 5, LocalState::Staged, 0, &["c"]);
        f.p.prepare(&ctx(5)).await.unwrap();
        restate(&f, 5, LocalState::Committed);
        assert_eq!(f.p.abort(TxId(5)).await.unwrap(), AbortOutcome::AlreadyCommitted);
        assert!(f.root.join("c").exists());
    }

    #[tokio::test]
    async fn a_publish_that_cannot_happen_is_retried_not_dropped() {
        let f = fixture();
        staged(&f, 7, LocalState::Staged, 0, &["z"]);
        f.p.prepare(&ctx(7)).await.unwrap();
        let (_, l) = f.p.load(TxId(7)).unwrap().unwrap();
        std::fs::remove_file(l.stages[0].2.join("z")).unwrap();
        let e = f.p.commit(TxId(7)).await.unwrap_err();
        assert!(matches!(&e, PartError::Transient(m) if m.starts_with("publish: ")), "{e}");
    }

    #[tokio::test]
    async fn one_phase_commit_from_every_local_state() {
        let f = fixture();
        assert_eq!(f.p.commit_one_phase(&ctx(1)).await.unwrap(), Outcome::Committed);
        staged(&f, 2, LocalState::Staged, 3, &["x"]);
        assert_eq!(f.p.commit_one_phase(&ctx(2)).await.unwrap(), Outcome::Aborted("process failed".into()));
        staged(&f, 3, LocalState::Staged, 0, &[]);
        assert_eq!(f.p.commit_one_phase(&ctx(3)).await.unwrap(), Outcome::Committed);
        staged(&f, 4, LocalState::Staged, 0, &["d"]);
        assert_eq!(f.p.commit_one_phase(&ctx(4)).await.unwrap(), Outcome::Committed);
        assert!(f.root.join("d").exists());
        staged(&f, 5, LocalState::Staged, 0, &["e"]);
        f.p.prepare(&ctx(5)).await.unwrap();
        assert_eq!(f.p.commit_one_phase(&ctx(5)).await.unwrap(), Outcome::Committed);
        assert!(f.root.join("e").exists());
        staged(&f, 6, LocalState::Aborted, 0, &[]);
        assert_eq!(f.p.commit_one_phase(&ctx(6)).await.unwrap(), Outcome::Aborted("locally aborted".into()));
    }
}

//! The `fs` participant: manages one root directory.
//!
//! Step kinds:
//! - `fs.put`          `{ path, content | source }`  write a file (atomic per file)
//! - `fs.delete`       `{ path }`
//! - `fs.replace_tree` `{ path, source }`             swap a whole subtree
//!
//! It also serves as the publisher for `proc` participants, which hand it a
//! ready-made redo list via [`FsParticipant::stage_redo`].

use crate::publish::{copy_tree, execute_redo, fsync_tree, RedoOp};
use crate::staging::{same_device, StageDir};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use txp_core::{ParticipantId, ParticipantSpec, TxId};
use txp_participant::{
    id_dirname, AbortOutcome, Capabilities, Journal, LocalState, Outcome, PartError, Participant, StageReport, StepSpec, TxCtx, Vote,
};

/// Adapter configuration stored in the `Begin` record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FsConfig {
    /// The managed root directory. Must exist.
    pub root: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Local {
    stage: PathBuf,
    ops: Vec<RedoOp>,
}

/// The `fs` participant for one managed root.
pub struct FsParticipant {
    id: ParticipantId,
    root: PathBuf,
    journal: Journal,
}

fn io(e: std::io::Error) -> PartError {
    PartError::Fatal(e.to_string())
}

fn rel(root: &Path, p: &str) -> Result<PathBuf, PartError> {
    let p = p.trim_start_matches('/');
    let pb = Path::new(p);
    if pb.components().any(|c| matches!(c, std::path::Component::ParentDir)) || p.is_empty() {
        return Err(PartError::VoteNo(format!("bad relative path {p:?}")));
    }
    Ok(root.join(pb))
}

impl FsParticipant {
    /// Open the adapter: creates its journal under `data_dir/participants`
    /// and checks that the root is a directory.
    pub fn new(id: ParticipantId, cfg: FsConfig, data_dir: &Path) -> Result<FsParticipant, PartError> {
        let journal = Journal::open(data_dir.join("participants").join(id_dirname(&id))).map_err(io)?;
        if !cfg.root.is_dir() {
            return Err(PartError::Fatal(format!("managed root {} is not a directory", cfg.root.display())));
        }
        Ok(FsParticipant { id, root: cfg.root, journal })
    }

    /// Registry factory: decodes [`FsConfig`] from `spec.config`.
    pub fn factory(spec: &ParticipantSpec, data_dir: &Path) -> Result<std::sync::Arc<dyn Participant>, PartError> {
        let cfg: FsConfig = serde_json::from_value(spec.config.clone()).map_err(|e| PartError::Fatal(e.to_string()))?;
        Ok(std::sync::Arc::new(FsParticipant::new(spec.id.clone(), cfg, data_dir)?))
    }

    /// The managed root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn load(&self, txid: TxId) -> Result<Option<(LocalState, Local)>, PartError> {
        Ok(self.journal.get::<Local>(txid).map_err(io)?.map(|e| (e.state, e.data)))
    }

    fn ensure_staged(&self, txid: TxId) -> Result<Local, PartError> {
        match self.load(txid)? {
            Some((LocalState::Staged, l)) => Ok(l),
            Some((st, _)) => Err(PartError::Fatal(format!("stage on {txid} in state {st:?}"))),
            None => {
                let sd = StageDir::create(&self.root, txid).map_err(io)?;
                if !same_device(&sd.path, &self.root).map_err(io)? {
                    return Err(PartError::Fatal("staging dir is not on the same filesystem as the root".into()));
                }
                let files = sd.subdir("files").map_err(io)?;
                let l = Local { stage: files, ops: vec![] };
                self.journal.set(txid, LocalState::Staged, &l).map_err(io)?;
                Ok(l)
            }
        }
    }

    /// Record externally-built redo ops (used by the process participant).
    pub fn stage_redo(&self, txid: TxId, ops: Vec<RedoOp>) -> Result<(), PartError> {
        let mut l = self.ensure_staged(txid)?;
        l.ops.extend(ops);
        self.journal.set(txid, LocalState::Staged, &l).map_err(io)
    }

    fn do_commit(&self, txid: TxId, l: &Local) -> Result<(), PartError> {
        execute_redo(&l.ops).map_err(|e| PartError::Transient(format!("publish: {e}")))?;
        self.journal.set(txid, LocalState::Committed, l).map_err(io)?;
        let _ = StageDir { path: l.stage.clone() }.discard();
        self.journal.remove(txid).map_err(io)
    }
}

#[async_trait]
impl Participant for FsParticipant {
    fn id(&self) -> ParticipantId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { atomic: true, read_only_capable: true, one_phase: true, ..Default::default() }
    }

    async fn stage(&self, tx: &TxCtx, step: &StepSpec) -> Result<StageReport, PartError> {
        let mut l = self.ensure_staged(tx.txid)?;
        let n = l.ops.len();
        let cfg = &step.config;
        let get = |k: &str| cfg.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        let path = get("path").ok_or_else(|| PartError::VoteNo(format!("step {}: missing path", step.id)))?;
        let target = rel(&self.root, &path)?;
        let files = l.stage.clone();
        let mut outputs = serde_json::Map::new();
        match step.kind.as_str() {
            "fs.put" => {
                let staged = files.join(format!("{n}-{}", target.file_name().unwrap().to_string_lossy()));
                let bytes: Vec<u8> = if let Some(c) = get("content") {
                    c.replace("$txid", &tx.txid.to_string()).into_bytes()
                } else if let Some(src) = get("source") {
                    std::fs::read(&src).map_err(|e| PartError::VoteNo(format!("read source {src}: {e}")))?
                } else {
                    return Err(PartError::VoteNo(format!("step {}: need content or source", step.id)));
                };
                std::fs::write(&staged, &bytes).map_err(io)?;
                let ino = std::fs::metadata(&staged).map_err(io)?.ino();
                use sha2::Digest;
                outputs.insert("sha256".into(), hex::encode(sha2::Sha256::digest(&bytes)).into());
                outputs.insert("bytes".into(), bytes.len().into());
                l.ops.push(RedoOp::PublishFile { staged, target, ino });
            }
            "fs.delete" => l.ops.push(RedoOp::Delete { target }),
            "fs.replace_tree" => {
                let src = get("source").ok_or_else(|| PartError::VoteNo("replace_tree needs source".into()))?;
                let staged = files.join(format!("{n}-tree"));
                copy_tree(Path::new(&src), &staged).map_err(|e| PartError::VoteNo(format!("copy {src}: {e}")))?;
                let ino = std::fs::metadata(&staged).map_err(io)?.ino();
                l.ops.push(RedoOp::SwapDir { staged, target, ino });
            }
            k => return Err(PartError::VoteNo(format!("fs participant: unknown step kind {k}"))),
        }
        self.journal.set(tx.txid, LocalState::Staged, &l).map_err(io)?;
        Ok(StageReport { summary: format!("{} {}", step.kind, path), outputs: outputs.into() })
    }

    async fn prepare(&self, tx: &TxCtx) -> Result<Vote, PartError> {
        match self.load(tx.txid)? {
            None => Ok(Vote::ReadOnly),
            Some((LocalState::Prepared, _)) => Ok(Vote::Prepared),
            Some((LocalState::Staged, l)) => {
                if l.ops.is_empty() {
                    self.journal.remove(tx.txid).map_err(io)?;
                    let _ = StageDir { path: l.stage }.discard();
                    return Ok(Vote::ReadOnly);
                }
                fsync_tree(&l.stage).map_err(io)?;
                self.journal.set(tx.txid, LocalState::Prepared, &l).map_err(io)?;
                Ok(Vote::Prepared)
            }
            Some((st, _)) => Err(PartError::Fatal(format!("prepare in state {st:?}"))),
        }
    }

    async fn commit(&self, txid: TxId) -> Result<(), PartError> {
        match self.load(txid)? {
            None | Some((LocalState::Aborted, _)) => Ok(()),
            Some((LocalState::Prepared | LocalState::Committed, l)) => self.do_commit(txid, &l),
            Some((LocalState::Staged, l)) => {
                // Commit without prepare only happens on the 1PC path.
                fsync_tree(&l.stage).map_err(io)?;
                self.do_commit(txid, &l)
            }
        }
    }

    async fn abort(&self, txid: TxId) -> Result<AbortOutcome, PartError> {
        match self.load(txid)? {
            None => Ok(AbortOutcome::Discarded),
            // A local Committed record is a decision (1PC): finish it.
            Some((LocalState::Committed, l)) => {
                self.do_commit(txid, &l)?;
                Ok(AbortOutcome::AlreadyCommitted)
            }
            Some((_, l)) => {
                StageDir { path: l.stage }.discard().map_err(io)?;
                self.journal.remove(txid).map_err(io)?;
                Ok(AbortOutcome::Discarded)
            }
        }
    }

    async fn commit_one_phase(&self, tx: &TxCtx) -> Result<Outcome, PartError> {
        match self.load(tx.txid)? {
            None => Ok(Outcome::Committed),
            Some((LocalState::Staged | LocalState::Prepared, l)) => {
                fsync_tree(&l.stage).map_err(io)?;
                // The local Committed record is the commit point.
                self.journal.set(tx.txid, LocalState::Committed, &l).map_err(io)?;
                self.do_commit(tx.txid, &l)?;
                Ok(Outcome::Committed)
            }
            Some((LocalState::Committed, l)) => {
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
    use std::sync::Arc;

    fn ctx(txid: TxId, data: &Path) -> TxCtx {
        TxCtx { txid, data_dir: data.to_path_buf(), deadline: None, outputs: Default::default() }
    }

    #[tokio::test]
    async fn put_prepare_commit_and_abort() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("site");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/old.txt"), "old").unwrap();
        let p = Arc::new(FsParticipant::new(ParticipantId::new("fs:site"), FsConfig { root: root.clone() }, d.path()).unwrap());

        let c = ctx(TxId(1), d.path());
        p.stage(&c, &StepSpec { id: "a".into(), kind: "fs.put".into(), config: serde_json::json!({"path": "index.html", "content": "hello $txid"}) }).await.unwrap();
        p.stage(&c, &StepSpec { id: "b".into(), kind: "fs.delete".into(), config: serde_json::json!({"path": "sub/old.txt"}) }).await.unwrap();
        assert!(!root.join("index.html").exists(), "staged state must be invisible");
        assert_eq!(p.prepare(&c).await.unwrap(), Vote::Prepared);
        assert!(!root.join("index.html").exists());
        p.commit(TxId(1)).await.unwrap();
        p.commit(TxId(1)).await.unwrap(); // idempotent
        assert_eq!(std::fs::read_to_string(root.join("index.html")).unwrap(), format!("hello {}", TxId(1)));
        assert!(!root.join("sub/old.txt").exists());
        assert!(p.recover().await.unwrap().is_empty());

        let c2 = ctx(TxId(2), d.path());
        p.stage(&c2, &StepSpec { id: "a".into(), kind: "fs.put".into(), config: serde_json::json!({"path": "index.html", "content": "bad"}) }).await.unwrap();
        p.prepare(&c2).await.unwrap();
        p.abort(TxId(2)).await.unwrap();
        p.abort(TxId(99)).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join("index.html")).unwrap(), format!("hello {}", TxId(1)));
        assert!(!crate::stage_root_for(&root).join(TxId(2).to_string()).exists());
    }

    fn step(kind: &str, config: serde_json::Value) -> StepSpec {
        StepSpec { id: "s".into(), kind: kind.into(), config }
    }

    fn site(d: &Path) -> FsParticipant {
        let root = d.join("site");
        std::fs::create_dir_all(&root).unwrap();
        FsParticipant::new(ParticipantId::new("fs:site"), FsConfig { root }, d).unwrap()
    }

    #[tokio::test]
    async fn factory_identity_and_bad_configurations() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("r")).unwrap();
        let spec = |config| ParticipantSpec { id: ParticipantId::new("fs:r"), kind: "fs".into(), config };
        let p = FsParticipant::factory(&spec(serde_json::json!({"root": d.path().join("r")})), d.path()).unwrap();
        assert_eq!(p.id().as_str(), "fs:r");
        assert!(p.capabilities().one_phase && p.capabilities().read_only_capable);
        assert!(FsParticipant::factory(&spec(serde_json::json!({})), d.path()).is_err());
        let e = FsParticipant::new(ParticipantId::new("fs:x"), FsConfig { root: d.path().join("missing") }, d.path()).err().unwrap();
        assert!(e.to_string().contains("is not a directory"), "{e}");
        // The journal cannot be created under a file.
        let bad = d.path().join("bad");
        std::fs::create_dir(&bad).unwrap();
        std::fs::write(bad.join("participants"), "x").unwrap();
        let e = FsParticipant::new(ParticipantId::new("fs:r"), FsConfig { root: d.path().join("r") }, &bad).err().unwrap();
        assert!(matches!(e, PartError::Fatal(_)));
    }

    #[tokio::test]
    async fn steps_with_bad_arguments_vote_no() {
        let d = tempfile::tempdir().unwrap();
        let p = site(d.path());
        assert_eq!(p.root(), d.path().join("site"));
        let c = ctx(TxId(1), d.path());
        let cases = [
            step("fs.put", serde_json::json!({"content": "x"})),
            step("fs.put", serde_json::json!({"path": "../escape", "content": "x"})),
            step("fs.put", serde_json::json!({"path": "/", "content": "x"})),
            step("fs.put", serde_json::json!({"path": "f"})),
            step("fs.put", serde_json::json!({"path": "f", "source": d.path().join("missing")})),
            step("fs.replace_tree", serde_json::json!({"path": "t"})),
            step("fs.replace_tree", serde_json::json!({"path": "t", "source": d.path().join("missing")})),
            step("fs.chmod", serde_json::json!({"path": "f"})),
        ];
        for s in cases {
            let e = p.stage(&c, &s).await.unwrap_err();
            assert!(matches!(e, PartError::VoteNo(_)), "{s:?}: {e}");
        }
    }

    #[tokio::test]
    async fn put_from_source_and_replace_tree_publish_on_one_phase_commit() {
        let d = tempfile::tempdir().unwrap();
        let p = site(d.path());
        let src = d.path().join("src");
        std::fs::create_dir_all(src.join("tree")).unwrap();
        std::fs::write(src.join("file"), "from source").unwrap();
        std::fs::write(src.join("tree/a"), "a").unwrap();
        let c = ctx(TxId(4), d.path());
        let r = p.stage(&c, &step("fs.put", serde_json::json!({"path": "copy", "source": src.join("file")}))).await.unwrap();
        assert_eq!(r.outputs["bytes"], 11);
        p.stage(&c, &step("fs.replace_tree", serde_json::json!({"path": "tree", "source": src.join("tree")}))).await.unwrap();
        assert_eq!(p.commit_one_phase(&c).await.unwrap(), Outcome::Committed);
        assert_eq!(std::fs::read_to_string(p.root().join("copy")).unwrap(), "from source");
        assert_eq!(std::fs::read_to_string(p.root().join("tree/a")).unwrap(), "a");
    }

    #[tokio::test]
    async fn redo_ops_from_another_participant_and_wrong_state_staging() {
        let d = tempfile::tempdir().unwrap();
        let p = site(d.path());
        let staged = d.path().join("built");
        std::fs::write(&staged, "built elsewhere").unwrap();
        let ino = std::fs::metadata(&staged).unwrap().ino();
        let target = p.root().join("out");
        p.stage_redo(TxId(5), vec![RedoOp::PublishFile { staged, target: target.clone(), ino }]).unwrap();
        let c = ctx(TxId(5), d.path());
        assert_eq!(p.prepare(&c).await.unwrap(), Vote::Prepared);
        // Once prepared, nothing more can be staged.
        let e = p.stage(&c, &step("fs.delete", serde_json::json!({"path": "x"}))).await.unwrap_err();
        assert!(e.to_string().contains("in state Prepared"), "{e}");
        assert!(p.stage_redo(TxId(5), vec![]).is_err());
        p.commit(TxId(5)).await.unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "built elsewhere");
    }

    #[tokio::test]
    async fn a_publish_that_cannot_happen_is_retried_not_dropped() {
        let d = tempfile::tempdir().unwrap();
        let p = site(d.path());
        let c = ctx(TxId(6), d.path());
        p.stage(&c, &step("fs.put", serde_json::json!({"path": "f", "content": "x"}))).await.unwrap();
        assert_eq!(p.prepare(&c).await.unwrap(), Vote::Prepared);
        // Someone removes the staged copy: the commit cannot publish it.
        let (_, l) = p.load(TxId(6)).unwrap().unwrap();
        std::fs::remove_dir_all(&l.stage).unwrap();
        let e = p.commit(TxId(6)).await.unwrap_err();
        assert!(matches!(&e, PartError::Transient(m) if m.starts_with("publish: ")), "{e}");
    }

    #[tokio::test]
    async fn read_only_vote_when_nothing_staged() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("r");
        std::fs::create_dir_all(&root).unwrap();
        let p = FsParticipant::new(ParticipantId::new("fs:r"), FsConfig { root }, d.path()).unwrap();
        assert_eq!(p.prepare(&ctx(TxId(3), d.path())).await.unwrap(), Vote::ReadOnly);
    }
}

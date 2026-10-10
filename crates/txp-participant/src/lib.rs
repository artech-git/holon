//! txp-participant: the generic durable-promise contract (design §1.5).
//!
//! Rules every adapter must honour:
//! - `stage` leaves no externally visible trace.
//! - After `prepare` returns `Vote::Prepared`, commit must be possible after
//!   a crash and the participant must not unilaterally abort (I3).
//! - `commit`/`abort` are *total* and idempotent over any prior state,
//!   including "never heard of this txid" (I6).
//! - `recover` enumerates txids the participant still holds state for.

#![warn(missing_docs)]

pub mod journal;
pub mod registry;

pub use journal::{Journal, JournalEntry, LocalState};
pub use registry::Registry;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;
use txp_core::{ParticipantId, TxId};

/// What a participant can do, so the engine can pick fast paths
/// (read-only vote, one-phase commit) safely.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Commit is atomic (true rollback on abort), not merely deferred/compensated.
    pub atomic: bool,
    /// Effects are deferred to post-commit (outbox-style).
    pub deferred: bool,
    /// Abort after effect requires compensation.
    pub compensating: bool,
    /// Can vote ReadOnly when nothing was staged.
    pub read_only_capable: bool,
    /// Supports `commit_one_phase`.
    pub one_phase: bool,
}

/// A participant's answer to `prepare`. Voting "no" is an error
/// ([`PartError::VoteNo`]), not a variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Vote {
    /// Staged work is durable; the participant will commit on request.
    Prepared,
    /// Nothing to commit; the participant drops out of phase two.
    ReadOnly,
}

/// Error taxonomy every adapter maps its failures onto; the variant
/// decides whether the engine retries, aborts, or pages someone.
#[derive(Debug, thiserror::Error, Clone, Serialize, Deserialize)]
pub enum PartError {
    /// Retry with backoff.
    #[error("transient: {0}")]
    Transient(String),
    /// Only during stage/prepare: the transaction must abort.
    #[error("vote no: {0}")]
    VoteNo(String),
    /// Isolation conflict: abort/retry the transaction.
    #[error("conflict on {0}")]
    Conflict(String),
    /// Participant broken; mark in-doubt and page the operator.
    #[error("fatal: {0}")]
    Fatal(String),
    /// The operation (e.g. `commit_one_phase`) is not implemented by this adapter.
    #[error("unsupported")]
    Unsupported,
}

impl PartError {
    /// Whether the engine should retry with backoff.
    pub fn is_transient(&self) -> bool {
        matches!(self, PartError::Transient(_))
    }
}

/// Per-transaction context handed to `stage`, `prepare` and `commit_one_phase`.
#[derive(Clone, Debug)]
pub struct TxCtx {
    /// Transaction id.
    pub txid: TxId,
    /// Per-daemon data directory; adapters keep journals under it.
    pub data_dir: PathBuf,
    /// Absolute deadline for the whole transaction, if the manifest set one.
    pub deadline: Option<Instant>,
    /// Outputs of previously completed steps, keyed by step id.
    pub outputs: serde_json::Map<String, serde_json::Value>,
}

/// One unit of work routed to a participant by the engine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepSpec {
    /// Step id from the manifest; keys the outputs map.
    pub id: String,
    /// Step kind (`fs.put`, `process`, ...), interpreted by the adapter.
    pub kind: String,
    /// Kind-specific parameters.
    pub config: serde_json::Value,
}

/// What `stage` reports back on success.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StageReport {
    /// One-line description for logs and status.
    pub summary: String,
    /// Structured outputs made available to later steps via [`TxCtx::outputs`].
    pub outputs: serde_json::Value,
}

/// Result of the one-phase path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// The participant's local commit record is durable and applied.
    Committed,
    /// The participant declined; the reason is reported to the client.
    Aborted(String),
}

/// What `abort` found. `AlreadyCommitted` can only happen for a participant
/// that committed locally on the one-phase path; a recovering coordinator
/// adopts that decision instead of reporting a presumed abort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbortOutcome {
    /// Staged state (if any) was thrown away.
    Discarded,
    /// A local one-phase commit had already happened; it was completed.
    AlreadyCommitted,
}

/// The durable-promise contract. See the crate docs for the rules an
/// implementation must honour.
#[async_trait]
pub trait Participant: Send + Sync + 'static {
    /// Stable identity, as recorded in the `Begin` record.
    fn id(&self) -> ParticipantId;
    /// Static capabilities of this adapter.
    fn capabilities(&self) -> Capabilities;

    /// Execute/stage work for this txn. Must leave no externally visible trace.
    async fn stage(&self, tx: &TxCtx, step: &StepSpec) -> Result<StageReport, PartError>;

    /// Durable promise.
    async fn prepare(&self, tx: &TxCtx) -> Result<Vote, PartError>;

    /// Idempotent; may be called repeatedly, including after restart.
    async fn commit(&self, txid: TxId) -> Result<(), PartError>;
    /// Total over any prior state; reports whether the participant had
    /// already committed locally (1PC).
    async fn abort(&self, txid: TxId) -> Result<AbortOutcome, PartError>;

    /// One-phase path: prepare+commit collapsed; the participant-local commit
    /// record is the commit point.
    async fn commit_one_phase(&self, _tx: &TxCtx) -> Result<Outcome, PartError> {
        Err(PartError::Unsupported)
    }

    /// Startup: txns this participant still holds local state for.
    async fn recover(&self) -> Result<Vec<(TxId, LocalState)>, PartError>;
}

/// Stable, filesystem-safe directory name for a participant id.
pub fn id_dirname(id: &ParticipantId) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(id.as_str().as_bytes());
    let safe: String = id.as_str().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("{}-{}", &safe[..safe.len().min(32)], hex::encode(&h[..6]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;
    use txp_core::ParticipantSpec;

    /// The smallest possible adapter: nothing is ever staged.
    struct Nop;

    #[async_trait]
    impl Participant for Nop {
        fn id(&self) -> ParticipantId {
            ParticipantId::new("nop:x")
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn stage(&self, _tx: &TxCtx, step: &StepSpec) -> Result<StageReport, PartError> {
            Ok(StageReport { summary: step.id.clone(), ..Default::default() })
        }
        async fn prepare(&self, _tx: &TxCtx) -> Result<Vote, PartError> {
            Ok(Vote::ReadOnly)
        }
        async fn commit(&self, _txid: TxId) -> Result<(), PartError> {
            Ok(())
        }
        async fn abort(&self, _txid: TxId) -> Result<AbortOutcome, PartError> {
            Ok(AbortOutcome::Discarded)
        }
        async fn recover(&self) -> Result<Vec<(TxId, LocalState)>, PartError> {
            Ok(vec![])
        }
    }

    fn ctx() -> TxCtx {
        TxCtx { txid: TxId(1), data_dir: "/data".into(), deadline: None, outputs: Default::default() }
    }

    #[tokio::test]
    async fn one_phase_is_unsupported_unless_an_adapter_opts_in() {
        let p = Nop;
        assert_eq!(p.id().as_str(), "nop:x");
        assert_eq!(p.capabilities(), Capabilities::default());
        let step = StepSpec { id: "s".into(), kind: "k".into(), config: serde_json::Value::Null };
        assert_eq!(p.stage(&ctx(), &step).await.unwrap().summary, "s");
        assert_eq!(p.prepare(&ctx()).await.unwrap(), Vote::ReadOnly);
        p.commit(TxId(1)).await.unwrap();
        assert_eq!(p.abort(TxId(1)).await.unwrap(), AbortOutcome::Discarded);
        assert!(p.recover().await.unwrap().is_empty());
        assert!(matches!(p.commit_one_phase(&ctx()).await, Err(PartError::Unsupported)));
    }

    #[test]
    fn only_transient_errors_are_retried() {
        let all = [
            PartError::Transient("t".into()),
            PartError::VoteNo("v".into()),
            PartError::Conflict("c".into()),
            PartError::Fatal("f".into()),
            PartError::Unsupported,
        ];
        let msgs: Vec<String> = all.iter().map(|e| e.to_string()).collect();
        assert_eq!(msgs, ["transient: t", "vote no: v", "conflict on c", "fatal: f", "unsupported"]);
        assert_eq!(all.iter().filter(|e| e.is_transient()).count(), 1);
    }

    #[test]
    fn id_dirname_is_safe_stable_and_distinct() {
        let a = id_dirname(&ParticipantId::new("fs:/srv/site"));
        assert!(a.starts_with("fs__srv_site-"), "{a}");
        assert_eq!(a, id_dirname(&ParticipantId::new("fs:/srv/site")));
        assert_ne!(id_dirname(&ParticipantId::new("a:b")), id_dirname(&ParticipantId::new("a/b")));
        assert_eq!(id_dirname(&ParticipantId::new("x".repeat(100))).len(), 32 + 1 + 12);
    }

    #[test]
    fn registry_builds_registered_kinds_only() {
        let mut r = Registry::new("/data");
        assert_eq!(r.data_dir(), Path::new("/data"));
        r.register("nop", Arc::new(|_spec, _dir| Ok(Arc::new(Nop) as Arc<dyn Participant>)));
        r.register("broken", Arc::new(|_spec, _dir| Err(PartError::Fatal("cannot".into()))));
        assert_eq!(r.kinds(), vec!["broken", "nop"]);
        let spec = |kind: &str| ParticipantSpec { id: ParticipantId::new("p"), kind: kind.into(), config: serde_json::Value::Null };
        assert_eq!(r.build(&spec("nop")).unwrap().id().as_str(), "nop:x");
        assert!(r.build(&spec("broken")).is_err());
        let e = r.build(&spec("zz")).err().unwrap();
        assert_eq!(e.to_string(), "fatal: no factory for participant kind \"zz\"");
    }
}

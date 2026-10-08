//! The decision-log record set and the participant description it carries.

use crate::{ParticipantId, TxId};
use serde::{Deserialize, Serialize};

/// Everything needed to re-instantiate a participant adapter after a restart.
/// `config` is adapter-specific (e.g. the managed root for `fs`, the argv and
/// mounts for `proc`). It lives in the `Begin` record so recovery never needs
/// the original manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantSpec {
    /// Stable identity; also keys the participant's on-disk journal.
    pub id: ParticipantId,
    /// Adapter kind resolved through the registry (`fs`, `proc`).
    pub kind: String,
    /// Adapter-specific configuration, opaque to the coordinator.
    pub config: serde_json::Value,
}

/// Who submitted a transaction, captured from the client socket's peer
/// credentials (`SO_PEERCRED`) and kept in the `Begin` record for audit.
/// A transaction submitted in-process (the embedded CLI, the crash harness)
/// has no external submitter and records `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submitter {
    /// Peer user id.
    pub uid: u32,
    /// Peer group id.
    pub gid: u32,
    /// Peer process id, if the OS reported one.
    #[serde(default)]
    pub pid: Option<i32>,
}

/// The outcome of a transaction as the log records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Every participant in the commit set must apply its staged work.
    Commit,
    /// Every participant discards its staged work.
    Abort,
}

/// Decision-log record kinds (design §1.3).
///
/// Only `Commit` is force-written: it is the single commit point (I1).
/// `Begin`, `Abort` and `Done` are written lazily (presumed abort, I4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogRecord {
    /// Transaction started. Carries everything recovery needs to rebuild
    /// the participant adapters without the original manifest.
    Begin {
        /// Transaction id.
        txid: TxId,
        /// Human-readable name from the manifest's `[txn]` section.
        name: String,
        /// SHA-256 hex digest of the manifest text, for audit and dedup.
        manifest_digest: String,
        /// Authenticated submitter (peer credentials). `None` for in-process
        /// submission and for logs written before this field existed; the
        /// `#[serde(default)]` keeps those records readable.
        #[serde(default)]
        submitter: Option<Submitter>,
        /// Adapters this transaction may stage on, vote with, or commit.
        participants: Vec<ParticipantSpec>,
    },
    /// Optional, non-forced, for operators: which participants voted how.
    Prepared {
        /// Transaction id.
        txid: TxId,
        /// `(participant, vote)` pairs; the vote is its `Debug` rendering.
        votes: Vec<(ParticipantId, String)>,
    },
    /// FORCED. The commit point.
    Commit {
        /// Transaction id.
        txid: TxId,
        /// The commit set: participants that voted `Prepared` and must be
        /// driven to commit until each acknowledges.
        participants: Vec<ParticipantId>,
    },
    /// Abort decided. Written lazily: its absence already means abort.
    Abort {
        /// Transaction id.
        txid: TxId,
    },
    /// Phase two finished (or nothing was left to do); the entry can be
    /// dropped at the next checkpoint.
    Done {
        /// Transaction id.
        txid: TxId,
    },
    /// Audited heuristic decision by an operator. Never contradicts a durable
    /// `Commit`; the engine refuses to write one that would.
    ForceResolve {
        /// Transaction id.
        txid: TxId,
        /// The decision the operator imposed.
        decision: Decision,
        /// Free-form justification kept for the audit trail.
        reason: String,
    },
}

impl LogRecord {
    /// The transaction this record belongs to.
    pub fn txid(&self) -> TxId {
        match self {
            LogRecord::Begin { txid, .. }
            | LogRecord::Prepared { txid, .. }
            | LogRecord::Commit { txid, .. }
            | LogRecord::Abort { txid }
            | LogRecord::Done { txid }
            | LogRecord::ForceResolve { txid, .. } => *txid,
        }
    }

    /// Whether this record must be fdatasync'd before its writer is told it
    /// is durable. Only commit decisions (and operator overrides) are forced.
    pub fn is_forced(&self) -> bool {
        matches!(self, LogRecord::Commit { .. } | LogRecord::ForceResolve { .. })
    }

    /// Snake-case name of the record kind, identical to its serialized `kind` tag.
    pub fn kind_name(&self) -> &'static str {
        match self {
            LogRecord::Begin { .. } => "begin",
            LogRecord::Prepared { .. } => "prepared",
            LogRecord::Commit { .. } => "commit",
            LogRecord::Abort { .. } => "abort",
            LogRecord::Done { .. } => "done",
            LogRecord::ForceResolve { .. } => "force_resolve",
        }
    }
}

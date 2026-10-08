//! Status reported to clients for each transaction the engine has seen.

use serde::{Deserialize, Serialize};
use txp_core::TxId;

/// Final result of a transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TxOutcome {
    /// Every participant in the commit set applied its work.
    Committed,
    /// Nothing became visible.
    Aborted {
        /// Why: a failed step, a `no` vote, a timeout, or `recovered`.
        reason: String,
    },
    /// Decided, but phase two could not be completed; operator attention.
    InDoubt {
        /// `commit`, `one_phase`, or a recovery action.
        decision: String,
        /// Last error from the failing participant.
        error: String,
    },
}

/// Live status of one transaction, as returned by `status` and `list`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxStatus {
    /// Transaction id.
    pub txid: TxId,
    /// Manifest name.
    pub name: String,
    /// Current phase (`submitted`, `locking`, `staging`, `preparing`,
    /// `committing`, `one_phase_commit`, `read_only`, `aborting`, `recovering`, `done`).
    pub phase: String,
    /// Ids of steps that staged successfully, in order.
    pub steps_done: Vec<String>,
    /// Step currently staging, if any.
    pub current_step: Option<String>,
    /// Set once the transaction is decided and finished (or in doubt).
    pub outcome: Option<TxOutcome>,
    /// Per-step outputs reported by participants, keyed by step id.
    pub outputs: serde_json::Map<String, serde_json::Value>,
    /// Submission time, seconds since the Unix epoch.
    pub started_unix: u64,
    /// Completion time, if finished.
    pub finished_unix: Option<u64>,
    /// Whether the one-phase fast path was taken.
    pub one_phase: bool,
}

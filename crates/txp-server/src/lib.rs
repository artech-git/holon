//! Wire protocol shared by `txpd` and `txp`: newline-delimited JSON over a
//! Unix socket. One request per line, one response per line.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use txp_core::TxId;

/// A client request, tagged by `op` in JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Submit a manifest; with `wait`, the response carries the outcome.
    Run {
        /// Manifest text (TOML).
        manifest: String,
        /// Block until the transaction finishes and include its outcome.
        #[serde(default)]
        wait: bool,
    },
    /// Status of one transaction.
    Status {
        /// Transaction to look up.
        txid: TxId,
    },
    /// Status of every transaction since the daemon started.
    List,
    /// Transactions decided but not yet `Done`.
    InDoubt,
    /// Participant journal entries the log no longer explains.
    Orphans,
    /// Held locks and the wait-for graph.
    Locks,
    /// Group-commit counters.
    WalStats,
    /// Snapshot the transaction table and truncate the log.
    Checkpoint,
    /// Host capabilities (root, cgroups, overlayfs, Landlock).
    SelfTest,
    /// What startup recovery did.
    Recovery,
    /// Stop admission, drain and exit.
    Shutdown,
}

/// A reply, tagged by `ok` in JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "ok")]
pub enum Response {
    /// Success.
    #[serde(rename = "true")]
    Ok {
        /// Command-specific payload.
        result: serde_json::Value,
    },
    /// Failure.
    #[serde(rename = "false")]
    Err {
        /// Human-readable message.
        error: String,
    },
}

impl Response {
    /// Wrap a serializable value; serialization failure yields `null`.
    pub fn ok(v: impl Serialize) -> Response {
        Response::Ok { result: serde_json::to_value(v).unwrap_or(serde_json::Value::Null) }
    }
    /// Wrap an error message.
    pub fn err(e: impl ToString) -> Response {
        Response::Err { error: e.to_string() }
    }
}

/// `$TXP_SOCKET`, or `/run/txpd.sock`.
pub fn default_socket() -> std::path::PathBuf {
    std::env::var("TXP_SOCKET").map(Into::into).unwrap_or_else(|_| "/run/txpd.sock".into())
}

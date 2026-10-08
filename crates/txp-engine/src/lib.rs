//! txp-engine: the coordinator (design §1.2, §1.4, §1.6).
//!
//! Each transaction runs in its own spawned task (never in a request
//! future, so client cancellation cannot interrupt the protocol). The only
//! commit point is the forced `Commit` record; phase two starts only with the
//! [`txp_core::DurableCommit`] token minted by the log writer.

#![warn(missing_docs)]

pub mod crash;
pub mod engine;
pub mod plan;
pub mod status;

pub use engine::{AuthPolicy, Engine, EngineConfig, EngineError};
pub use plan::{Plan, PlanStep, RunAsPolicy};
pub use status::{TxOutcome, TxStatus};
pub use txp_core::Submitter;

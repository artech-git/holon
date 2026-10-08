//! txp-core: the pure heart of the coordinator.
//!
//! Nothing in this crate performs I/O. It defines:
//! - identifiers ([`TxId`], [`ParticipantId`], [`Lsn`]),
//! - the decision-log record set ([`LogRecord`]),
//! - the replayable transaction table ([`TxnTable`]) and the recovery actions
//!   derived from it (invariants I1, I2, I4, I6 of the design document),
//! - the typestate driver wrappers ([`typestate`]) and the unforgeable
//!   [`DurableCommit`] token that gates phase two.
//!
//! Phase 1 feeds this state machine from the local WAL; Phase 4 will feed it
//! from Raft `apply`. The signatures are deliberately runtime-agnostic.

#![warn(missing_docs)]

pub mod ids;
pub mod record;
pub mod table;
pub mod typestate;

pub use ids::{Lsn, ParticipantId, TxId};
pub use record::{Decision, LogRecord, ParticipantSpec, Submitter};
pub use table::{RecoveryAction, TxPhase, TxnEntry, TxnTable, TxnTableSnapshot};
pub use typestate::DurableCommit;

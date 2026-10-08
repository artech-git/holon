//! txp-fs: filesystem participant (design §1.7).
//!
//! The staging root lives on the same filesystem as the managed root, as a
//! sibling directory `<parent>/.txp-stage/<rootname>/<txid>/`, so every
//! publish is a `rename(2)` or `renameat2(RENAME_EXCHANGE)` and never a copy.
//!
//! Commit is a **redo list** persisted at prepare time ([`RedoOp`]). Each op
//! is idempotent: it records the staged inode, so a replay after a crash can
//! tell "already published" (target inode == staged inode) from "not yet".
//! Atomicity across a crash is guaranteed; a multi-path publish is not
//! instantaneous for unmanaged readers (single-file and single-swap layouts
//! are).

#![warn(missing_docs)]

pub mod participant;
pub mod publish;
pub mod staging;

pub use participant::{FsConfig, FsParticipant};
pub use publish::{execute_redo, fsync_tree, DirAttrs, RedoOp};
pub use staging::{stage_root_for, StageDir};

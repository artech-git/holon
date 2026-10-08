//! txp-wal: the decision log (design §1.3).
//!
//! Layout on disk (under `dir`):
//! ```text
//! seg-<id:016x>.wal     segment files, preallocated, append-only
//! snap.json             last checkpoint (TxnTableSnapshot), replaced atomically
//! ```
//! Segment file: `[SegmentHeader (64 bytes)]` then records:
//! `[len u32][crc32c u32 over (lsn..payload)][lsn u64][payload: JSON LogRecord]`.
//!
//! Rules enforced here:
//! - never rewrite a durable region in place (append-only; header immutable),
//! - a bad length/CRC in the *last* segment is a torn tail → truncate,
//!   anywhere else it is corruption → refuse to start,
//! - any error from `fdatasync`/`fsync` is fatal: the process aborts and
//!   recovers from what is on disk (fsyncgate lesson),
//! - one writer thread does group commit: drain queue → one write → one
//!   fdatasync (only if some record in the batch is forced) → resolve waiters.

#![warn(missing_docs)]

pub mod disk;
pub mod format;
pub mod recovery;
pub mod writer;

pub use disk::{Disk, RealDisk, SegmentHandle};
pub use format::{SegmentHeader, HEADER_LEN};
pub use recovery::{Recovered, ScanOutcome};
pub use writer::{Wal, WalConfig, WalError};

pub mod sim;

/// Abort the process. Used on fsync failure: after a failed fsync the page
/// cache may hold bytes the disk never saw, so no further progress is safe.
pub fn fatal(msg: &str) -> ! {
    tracing::error!("FATAL: {msg}");
    eprintln!("txp-wal FATAL: {msg}");
    std::process::abort();
}

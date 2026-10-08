//! Recovery scan: load snapshot, replay segments in order, truncate a torn
//! tail in the last segment, refuse to start on corruption elsewhere.

use crate::disk::Disk;
use crate::format::{decode_record, Frame, SegmentHeader, HEADER_LEN};
use std::path::{Path, PathBuf};
use txp_core::{LogRecord, Lsn, TxnTable, TxnTableSnapshot};

/// Why the log could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// An I/O error while reading or truncating.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A segment other than the last one has a bad header or frame; or the
    /// LSNs are not contiguous. Recovery refuses to guess.
    #[error("corrupt segment {path}: {reason}")]
    Corrupt {
        /// Offending segment.
        path: PathBuf,
        /// What was wrong.
        reason: String,
    },
    /// `snap.json` exists but does not parse.
    #[error("bad snapshot: {0}")]
    Snapshot(String),
    /// A replayed record violates the protocol (see `txp_core::table::ApplyError`).
    #[error("replay: {0}")]
    Apply(String),
}

/// Everything learned by scanning the log directory.
pub struct Recovered {
    /// The transaction table after replaying every durable record.
    pub table: TxnTable,
    /// Every record replayed after the snapshot (for tools / tests).
    pub records: Vec<(Lsn, LogRecord)>,
    /// LSN the next appended record will carry.
    pub next_lsn: Lsn,
    /// `(segment_id, path)` for every segment file, in id order.
    pub segments: Vec<(u64, PathBuf)>,
    /// (segment_id, byte offset) where appends continue, if any segment exists.
    pub tail: Option<(u64, u64)>,
    /// Set when a torn tail was found and zeroed in the last segment.
    pub truncated_tail: Option<ScanOutcome>,
}

/// Where and why a scan stopped early in the last segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    /// Segment file concerned.
    pub path: PathBuf,
    /// Byte offset of the first undecodable frame.
    pub offset: u64,
    /// Decoder's explanation (short tail, CRC mismatch, ...).
    pub reason: String,
}

/// `<dir>/seg-<id as 16 hex digits>.wal`.
pub fn segment_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("seg-{id:016x}.wal"))
}

/// Inverse of [`segment_path`] on the file name; `None` for other files.
pub fn parse_segment_id(p: &Path) -> Option<u64> {
    let name = p.file_name()?.to_str()?;
    let hex = name.strip_prefix("seg-")?.strip_suffix(".wal")?;
    u64::from_str_radix(hex, 16).ok()
}

/// `<dir>/snap.json`, the checkpoint file.
pub fn snapshot_path(dir: &Path) -> PathBuf {
    dir.join("snap.json")
}

/// Scan `dir`: load the snapshot if present, replay every segment in id
/// order, verify LSNs are contiguous, zero a torn tail in the last segment,
/// and refuse to continue on corruption anywhere else.
pub fn recover(disk: &dyn Disk, dir: &Path) -> Result<Recovered, RecoveryError> {
    disk.create_dir_all(dir)?;
    let mut table = TxnTable::new();
    let mut snap_lsn = Lsn::ZERO;
    let snap_path = snapshot_path(dir);
    if disk.exists(&snap_path) {
        let bytes = disk.read_all(&snap_path)?;
        let snap: TxnTableSnapshot =
            serde_json::from_slice(&bytes).map_err(|e| RecoveryError::Snapshot(e.to_string()))?;
        snap_lsn = snap.lsn;
        table = TxnTable::from_snapshot(snap);
    }

    let mut segments: Vec<(u64, PathBuf)> =
        disk.list(dir)?.into_iter().filter_map(|p| parse_segment_id(&p).map(|id| (id, p))).collect();
    segments.sort();

    let mut records = Vec::new();
    let mut next_lsn = snap_lsn.next();
    if snap_lsn == Lsn::ZERO {
        next_lsn = Lsn(1);
    }
    let mut tail = None;
    let mut truncated_tail = None;
    let last_idx = segments.len().saturating_sub(1);

    for (i, (id, path)) in segments.iter().enumerate() {
        let bytes = disk.read_all(path)?;
        let hdr = SegmentHeader::decode(&bytes)
            .ok_or_else(|| RecoveryError::Corrupt { path: path.clone(), reason: "bad header".into() })?;
        if hdr.segment_id != *id {
            return Err(RecoveryError::Corrupt { path: path.clone(), reason: "segment id mismatch".into() });
        }
        let mut off = HEADER_LEN;
        loop {
            match decode_record(&bytes[off..]) {
                Frame::End => break,
                Frame::Ok { lsn, rec, consumed } => {
                    if lsn > snap_lsn {
                        if lsn != next_lsn {
                            return Err(RecoveryError::Corrupt {
                                path: path.clone(),
                                reason: format!("lsn gap: expected {next_lsn}, found {lsn}"),
                            });
                        }
                        table.apply(lsn, &rec).map_err(|e| RecoveryError::Apply(e.to_string()))?;
                        records.push((lsn, rec));
                        next_lsn = lsn.next();
                    }
                    off += consumed;
                }
                Frame::Bad(reason) => {
                    if i == last_idx {
                        tracing::warn!(path = %path.display(), offset = off, %reason, "torn tail; truncating");
                        let mut h = disk.open(path)?;
                        // Zero the tail rather than shrinking the file so the
                        // preallocation stays intact.
                        let len = h.len()?;
                        if len > off as u64 {
                            let zeros = vec![0u8; (len - off as u64).min(1 << 20) as usize];
                            let mut o = off as u64;
                            while o < len {
                                let n = zeros.len().min((len - o) as usize);
                                h.write_at(o, &zeros[..n])?;
                                o += n as u64;
                            }
                            h.datasync()?;
                        }
                        truncated_tail = Some(ScanOutcome { path: path.clone(), offset: off as u64, reason });
                        break;
                    } else {
                        return Err(RecoveryError::Corrupt { path: path.clone(), reason });
                    }
                }
            }
        }
        if i == last_idx {
            tail = Some((*id, off as u64));
        }
    }

    Ok(Recovered { table, records, next_lsn, segments, tail, truncated_tail })
}

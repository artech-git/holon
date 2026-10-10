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
    /// `(segment_id, base LSN from its header)` for every segment, in id order.
    pub segment_bases: Vec<(u64, Lsn)>,
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
    let mut segment_bases = Vec::with_capacity(segments.len());
    let last_idx = segments.len().saturating_sub(1);

    for (i, (id, path)) in segments.iter().enumerate() {
        let bytes = disk.read_all(path)?;
        let hdr = SegmentHeader::decode(&bytes)
            .ok_or_else(|| RecoveryError::Corrupt { path: path.clone(), reason: "bad header".into() })?;
        if hdr.segment_id != *id {
            return Err(RecoveryError::Corrupt { path: path.clone(), reason: "segment id mismatch".into() });
        }
        segment_bases.push((*id, hdr.base_lsn));
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
                        let zeros = vec![0u8; len.saturating_sub(off as u64).min(1 << 20) as usize];
                        let mut o = off as u64;
                        while o < len {
                            let n = zeros.len().min((len - o) as usize);
                            h.write_at(o, &zeros[..n])?;
                            o += n as u64;
                        }
                        h.datasync()?;
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

    Ok(Recovered { table, records, next_lsn, segments, segment_bases, tail, truncated_tail })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::encode_record;
    use crate::sim::SimDisk;
    use txp_core::{TxId, TxPhase};

    const DIR: &str = "/wal";

    fn begin(t: u128) -> LogRecord {
        LogRecord::Begin { txid: TxId(t), name: "n".into(), manifest_digest: "d".into(), submitter: None, participants: vec![] }
    }

    /// Segment `id` (header says `hdr_id`) holding `recs`, then `tail`, then zeros.
    fn segment(hdr_id: u64, base: u64, recs: &[(u64, LogRecord)], tail: &[u8]) -> Vec<u8> {
        let mut b = SegmentHeader { version: 1, segment_id: hdr_id, base_lsn: Lsn(base), created_unix: 0 }.encode().to_vec();
        b.resize(HEADER_LEN, 0);
        for (lsn, r) in recs {
            b.extend_from_slice(&encode_record(Lsn(*lsn), r));
        }
        b.extend_from_slice(tail);
        b.resize(b.len() + 256, 0);
        b
    }

    fn disk_with(segs: &[(u64, Vec<u8>)]) -> SimDisk {
        let disk = SimDisk::new(1);
        for (id, bytes) in segs {
            disk.write_atomic(&segment_path(Path::new(DIR), *id), bytes).unwrap();
        }
        disk
    }

    fn corrupt_reason(disk: &SimDisk) -> String {
        let Err(RecoveryError::Corrupt { reason, .. }) = recover(disk, Path::new(DIR)) else { panic!("not corrupt") };
        reason
    }

    #[test]
    fn segment_names_roundtrip_and_other_files_are_ignored() {
        let p = segment_path(Path::new(DIR), 0x2a);
        assert_eq!(p, Path::new("/wal/seg-000000000000002a.wal"));
        assert_eq!(parse_segment_id(&p), Some(0x2a));
        assert_eq!(parse_segment_id(&snapshot_path(Path::new(DIR))), None);
        assert_eq!(parse_segment_id(Path::new("/wal/seg-zz.wal")), None);
        assert_eq!(parse_segment_id(Path::new("/")), None);
    }

    #[test]
    fn empty_directory_recovers_to_nothing() {
        let r = recover(&SimDisk::new(1), Path::new(DIR)).unwrap();
        assert_eq!((r.next_lsn, r.tail, r.segments.len()), (Lsn(1), None, 0));
    }

    #[test]
    fn records_replay_in_order_across_segments() {
        let disk = disk_with(&[
            (1, segment(1, 1, &[(1, begin(1)), (2, begin(2))], &[])),
            (2, segment(2, 3, &[(3, LogRecord::Abort { txid: TxId(1) })], &[])),
        ]);
        let r = recover(&disk, Path::new(DIR)).unwrap();
        assert_eq!(r.records.len(), 3);
        assert_eq!(r.next_lsn, Lsn(4));
        assert_eq!(r.segment_bases, vec![(1, Lsn(1)), (2, Lsn(3))]);
        let frame = encode_record(Lsn(3), &LogRecord::Abort { txid: TxId(1) }).len() as u64;
        assert_eq!(r.tail, Some((2, HEADER_LEN as u64 + frame)));
        assert_eq!(r.table.get(TxId(1)).unwrap().phase, TxPhase::Aborting);
        assert!(r.truncated_tail.is_none());
    }

    #[test]
    fn structural_damage_is_corruption() {
        let mut bad = segment(1, 1, &[], &[]);
        bad[0] ^= 0xff;
        assert_eq!(corrupt_reason(&disk_with(&[(1, bad)])), "bad header");
        assert_eq!(corrupt_reason(&disk_with(&[(2, segment(1, 1, &[], &[]))])), "segment id mismatch");
        let gap = segment(1, 1, &[(1, begin(1)), (3, begin(3))], &[]);
        assert_eq!(corrupt_reason(&disk_with(&[(1, gap)])), "lsn gap: expected 2, found 3");
        // A torn frame is only forgivable in the last segment.
        let torn = segment(1, 1, &[(1, begin(1))], &[7; 40]);
        let next = segment(2, 2, &[(2, begin(2))], &[]);
        assert!(corrupt_reason(&disk_with(&[(1, torn), (2, next)])).starts_with("implausible length"));
        let e = recover(&disk_with(&[(2, segment(1, 1, &[], &[]))]), Path::new(DIR)).err().unwrap();
        assert_eq!(e.to_string(), "corrupt segment /wal/seg-0000000000000002.wal: segment id mismatch");
    }

    #[test]
    fn a_record_that_breaks_the_protocol_stops_recovery() {
        let disk = disk_with(&[(1, segment(1, 1, &[(1, LogRecord::Done { txid: TxId(9) })], &[]))]);
        let e = recover(&disk, Path::new(DIR)).err().unwrap();
        assert!(matches!(&e, RecoveryError::Apply(m) if m.contains("unknown transaction")), "{e}");
        assert!(e.to_string().starts_with("replay: "));
    }

    #[test]
    fn torn_tail_in_the_last_segment_is_zeroed_once() {
        let disk = disk_with(&[(1, segment(1, 1, &[(1, begin(1))], &[0xab; 300]))]);
        let r = recover(&disk, Path::new(DIR)).unwrap();
        let t = r.truncated_tail.clone().unwrap();
        let end = HEADER_LEN as u64 + encode_record(Lsn(1), &begin(1)).len() as u64;
        assert_eq!((t.offset, r.tail), (end, Some((1, end))));
        assert!(t.reason.starts_with("implausible length"), "{t:?}");
        let bytes = disk.read_all(&segment_path(Path::new(DIR), 1)).unwrap();
        assert!(bytes[end as usize..].iter().all(|&b| b == 0), "tail zeroed");
        // The zeroing was synced, and a second scan finds a clean end.
        disk.crash();
        let again = recover(&disk, Path::new(DIR)).unwrap();
        assert!(again.truncated_tail.is_none());
        assert_eq!(again.records.len(), 1);
    }

    #[test]
    fn snapshot_supplies_the_table_and_older_records_are_skipped() {
        let disk = disk_with(&[(1, segment(1, 1, &[(1, begin(1)), (2, begin(2)), (3, begin(3))], &[]))]);
        let mut table = TxnTable::new();
        table.apply(Lsn(1), &begin(1)).unwrap();
        table.apply(Lsn(2), &begin(2)).unwrap();
        let snap = serde_json::to_vec(&table.snapshot()).unwrap();
        disk.write_atomic(&snapshot_path(Path::new(DIR)), &snap).unwrap();
        let r = recover(&disk, Path::new(DIR)).unwrap();
        assert_eq!(r.records.iter().map(|(l, _)| *l).collect::<Vec<_>>(), vec![Lsn(3)]);
        assert_eq!((r.next_lsn, r.table.entries().count()), (Lsn(4), 3));

        disk.write_atomic(&snapshot_path(Path::new(DIR)), b"not json").unwrap();
        let e = recover(&disk, Path::new(DIR)).err().unwrap();
        assert!(matches!(e, RecoveryError::Snapshot(_)));
        assert!(e.to_string().starts_with("bad snapshot: "));
        let io = RecoveryError::from(std::io::Error::other("boom"));
        assert_eq!(io.to_string(), "io: boom");
    }
}

//! Group-commit log writer (design §1.2 / §1.3).
//!
//! A single OS thread owns the current segment. Callers enqueue records and
//! await a oneshot. The thread drains everything pending, applies each record
//! to the authoritative [`TxnTable`] (rejecting illegal transitions before
//! they reach disk), writes the batch with one `pwrite`, issues one
//! `fdatasync` if any record in the batch is forced, then resolves waiters.

use crate::disk::{Disk, SegmentHandle};
use crate::fatal;
use crate::format::{encode_record, SegmentHeader, HEADER_LEN};
use crate::recovery::{recover, segment_path, snapshot_path, Recovered, RecoveryError};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
use txp_core::{DurableCommit, LogRecord, Lsn, ParticipantId, TxId, TxnTable, TxnTableSnapshot};

/// Errors surfaced to log clients. Disk failures never reach here: the
/// writer aborts the process instead (see [`crate::fatal`]).
#[derive(Debug, thiserror::Error, Clone)]
pub enum WalError {
    /// The writer thread has stopped (shutdown or last handle dropped).
    #[error("log writer stopped")]
    Closed,
    /// The record was refused by the transaction table before reaching disk.
    #[error("rejected: {0}")]
    Rejected(String),
    /// A non-fatal I/O problem, e.g. the checkpoint file could not be written.
    #[error("io: {0}")]
    Io(String),
}

/// Sizing knobs for segments and group-commit batches.
#[derive(Clone, Debug)]
pub struct WalConfig {
    /// Preallocated size of each segment; a batch that would overflow it
    /// triggers a roll to a new segment.
    pub segment_bytes: u64,
    /// Flush when the pending batch would exceed this many bytes.
    pub max_batch_bytes: usize,
    /// Maximum requests drained from the queue per batch.
    pub max_batch_records: usize,
}

impl Default for WalConfig {
    fn default() -> Self {
        WalConfig { segment_bytes: 64 << 20, max_batch_bytes: 4 << 20, max_batch_records: 4096 }
    }
}

enum Req {
    Append { rec: LogRecord, reply: oneshot::Sender<Result<Lsn, WalError>> },
    Checkpoint { reply: oneshot::Sender<Result<Lsn, WalError>> },
    Shutdown,
}

/// Counters kept by the writer thread, for the `wal-stats` command and tests.
#[derive(Default, Debug, Clone)]
pub struct WalStats {
    /// Number of flushes (one positional write each).
    pub batches: u64,
    /// Records accepted and written.
    pub records: u64,
    /// Flushes that ended in `fdatasync` (at least one forced record).
    pub fsyncs: u64,
    /// Largest number of records written in one flush.
    pub max_batch: u64,
    /// Histogram buckets of batch sizes: [1, 2-3, 4-7, 8-15, 16+]
    pub batch_hist: [u64; 5],
}

struct Inner {
    tx: Mutex<Option<mpsc::Sender<Req>>>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
    table: Arc<Mutex<TxnTable>>,
    stats: Arc<Mutex<WalStats>>,
    dir: PathBuf,
}

/// Cloneable handle to the log. Dropping the last handle shuts the writer down.
#[derive(Clone)]
pub struct Wal {
    inner: Arc<Inner>,
}

impl Wal {
    /// Recover and open the log at `dir`. Returns the handle and the records
    /// replayed after the last checkpoint (the engine uses the table, tools
    /// use the records).
    pub fn open(disk: Arc<dyn Disk>, dir: &Path, cfg: WalConfig) -> Result<(Wal, Recovered), RecoveryError> {
        let recovered = recover(disk.as_ref(), dir)?;
        let table = Arc::new(Mutex::new(recovered.table.clone()));
        let stats = Arc::new(Mutex::new(WalStats::default()));

        let mut w = Writer {
            disk: disk.clone(),
            dir: dir.to_path_buf(),
            cfg,
            cur: None,
            cur_id: 0,
            offset: 0,
            next_lsn: recovered.next_lsn,
            table: table.clone(),
            stats: stats.clone(),
            segments: recovered.segments.iter().map(|(id, _)| *id).collect(),
            segment_base: recovered.segment_bases.clone(),
        };
        match recovered.tail {
            Some((id, off)) => {
                w.cur = Some(disk.open(&segment_path(dir, id))?);
                w.cur_id = id;
                w.offset = off;
            }
            None => w.roll().map_err(RecoveryError::Io)?,
        }

        let (tx, rx) = mpsc::channel::<Req>();
        let join = std::thread::Builder::new()
            .name("txp-wal-writer".into())
            .spawn(move || w.run(rx))
            .expect("spawn wal writer");

        let wal = Wal {
            inner: Arc::new(Inner {
                tx: Mutex::new(Some(tx)),
                join: Mutex::new(Some(join)),
                table,
                stats,
                dir: dir.to_path_buf(),
            }),
        };
        Ok((wal, recovered))
    }

    /// Directory holding the segments and snapshot.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Authoritative transaction table (updated by the writer thread as
    /// records are accepted).
    pub fn table(&self) -> Arc<Mutex<TxnTable>> {
        self.inner.table.clone()
    }

    /// A copy of the current counters.
    pub fn stats(&self) -> WalStats {
        self.inner.stats.lock().clone()
    }

    fn send(&self, req: Req) -> Result<(), WalError> {
        let g = self.inner.tx.lock();
        match g.as_ref() {
            Some(tx) => tx.send(req).map_err(|_| WalError::Closed),
            None => Err(WalError::Closed),
        }
    }

    /// Append a record. Resolves when the record has been written (and, if
    /// `rec.is_forced()`, fdatasync'd). Cancel-safe for the caller: dropping
    /// the returned future does not un-enqueue the record.
    pub async fn append(&self, rec: LogRecord) -> Result<Lsn, WalError> {
        let (reply, rx) = oneshot::channel();
        self.send(Req::Append { rec, reply })?;
        rx.await.map_err(|_| WalError::Closed)?
    }

    /// Write the forced Commit record and mint the only proof that phase two
    /// may begin.
    pub async fn append_commit(&self, txid: TxId, participants: Vec<ParticipantId>) -> Result<DurableCommit, WalError> {
        let lsn = self.append(LogRecord::Commit { txid, participants }).await?;
        Ok(DurableCommit::mint(txid, lsn))
    }

    /// Snapshot the table, persist it atomically, and delete segments that are
    /// entirely below the snapshot.
    pub async fn checkpoint(&self) -> Result<Lsn, WalError> {
        let (reply, rx) = oneshot::channel();
        self.send(Req::Checkpoint { reply })?;
        rx.await.map_err(|_| WalError::Closed)?
    }

    /// Stop the writer after draining queued requests.
    pub fn shutdown(&self) {
        if let Some(tx) = self.inner.tx.lock().take() {
            let _ = tx.send(Req::Shutdown);
        }
        if let Some(j) = self.inner.join.lock().take() {
            let _ = j.join();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.lock().take() {
            let _ = tx.send(Req::Shutdown);
        }
        if let Some(j) = self.join.lock().take() {
            let _ = j.join();
        }
    }
}

struct Writer {
    disk: Arc<dyn Disk>,
    dir: PathBuf,
    cfg: WalConfig,
    cur: Option<Box<dyn SegmentHandle>>,
    cur_id: u64,
    offset: u64,
    next_lsn: Lsn,
    table: Arc<Mutex<TxnTable>>,
    stats: Arc<Mutex<WalStats>>,
    segments: Vec<u64>,
    segment_base: Vec<(u64, Lsn)>,
}

impl Writer {
    fn roll(&mut self) -> std::io::Result<()> {
        if let Some(cur) = self.cur.as_mut() {
            // Make the old segment fully durable before moving on.
            if let Err(e) = cur.datasync() {
                fatal(&format!("fdatasync on segment roll failed: {e}"));
            }
        }
        let id = self.segments.last().map(|i| i + 1).unwrap_or(1);
        let path = segment_path(&self.dir, id);
        let mut h = self.disk.create(&path)?;
        h.preallocate(self.cfg.segment_bytes)?;
        let hdr = SegmentHeader {
            version: 1,
            segment_id: id,
            base_lsn: self.next_lsn,
            created_unix: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        };
        h.write_at(0, &hdr.encode())?;
        if let Err(e) = h.fsync() {
            fatal(&format!("fsync of new segment failed: {e}"));
        }
        if let Err(e) = self.disk.fsync_dir(&self.dir) {
            fatal(&format!("fsync of log directory failed: {e}"));
        }
        self.cur = Some(h);
        self.cur_id = id;
        self.offset = HEADER_LEN as u64;
        self.segments.push(id);
        self.segment_base.push((id, self.next_lsn));
        tracing::info!(segment = id, base_lsn = %self.next_lsn, "rolled log segment");
        Ok(())
    }

    /// The open segment; present from the end of [`Wal::open`] on.
    fn cur(&mut self) -> &mut Box<dyn SegmentHandle> {
        self.cur.as_mut().expect("current segment")
    }

    fn run(mut self, rx: mpsc::Receiver<Req>) {
        // Every handle sends `Shutdown` before letting go, so the channel
        // only closes after the loop has already returned.
        while let Ok(first) = rx.recv() {
            let mut reqs = vec![first];
            while reqs.len() < self.cfg.max_batch_records {
                match rx.try_recv() {
                    Ok(r) => reqs.push(r),
                    Err(_) => break,
                }
            }
            let mut shutdown = false;
            let mut batch: Vec<u8> = Vec::new();
            let mut waiters: Vec<(oneshot::Sender<Result<Lsn, WalError>>, Lsn)> = Vec::new();
            let mut forced = false;
            let mut checkpoints = Vec::new();

            for req in reqs {
                match req {
                    Req::Shutdown => shutdown = true,
                    Req::Checkpoint { reply } => checkpoints.push(reply),
                    Req::Append { rec, reply } => {
                        let lsn = self.next_lsn;
                        // Validate against the authoritative table first.
                        if let Err(e) = self.table.lock().apply(lsn, &rec) {
                            let _ = reply.send(Err(WalError::Rejected(e.to_string())));
                            continue;
                        }
                        let frame = encode_record(lsn, &rec);
                        if batch.len() + frame.len() > self.cfg.max_batch_bytes && !batch.is_empty() {
                            // Flush what we have, then continue with a fresh batch.
                            self.flush(&batch, forced, std::mem::take(&mut waiters));
                            batch.clear();
                            forced = false;
                        }
                        if self.offset + (batch.len() + frame.len()) as u64 > self.cfg.segment_bytes {
                            self.flush(&batch, forced, std::mem::take(&mut waiters));
                            batch.clear();
                            forced = false;
                            if let Err(e) = self.roll() {
                                fatal(&format!("cannot roll segment: {e}"));
                            }
                        }
                        forced |= rec.is_forced();
                        batch.extend_from_slice(&frame);
                        waiters.push((reply, lsn));
                        self.next_lsn = lsn.next();
                    }
                }
            }
            // Waiters only exist alongside the frames they wait for.
            if !batch.is_empty() {
                self.flush(&batch, forced, waiters);
            }
            for reply in checkpoints {
                let r = self.checkpoint();
                let _ = reply.send(r);
            }
            if shutdown {
                let _ = self.cur().datasync();
                return;
            }
        }
    }

    fn flush(&mut self, batch: &[u8], forced: bool, waiters: Vec<(oneshot::Sender<Result<Lsn, WalError>>, Lsn)>) {
        let cur = self.cur.as_mut().expect("current segment");
        if !batch.is_empty() {
            if let Err(e) = cur.write_at(self.offset, batch) {
                // A failed write before sync is recoverable in principle, but
                // we cannot know what landed; treat it like a sync failure.
                fatal(&format!("log write failed: {e}"));
            }
            self.offset += batch.len() as u64;
        }
        if forced
            && let Err(e) = cur.datasync() {
                fatal(&format!("fdatasync failed: {e}"));
            }
        {
            let mut s = self.stats.lock();
            s.batches += 1;
            s.records += waiters.len() as u64;
            s.fsyncs += forced as u64;
            s.max_batch = s.max_batch.max(waiters.len() as u64);
            let n = waiters.len() as u64;
            let b = if n <= 1 { 0 } else if n <= 3 { 1 } else if n <= 7 { 2 } else if n <= 15 { 3 } else { 4 };
            s.batch_hist[b] += 1;
        }
        for (w, lsn) in waiters {
            let _ = w.send(Ok(lsn));
        }
    }

    fn checkpoint(&mut self) -> Result<Lsn, WalError> {
        // Everything applied so far has been written (table apply happens
        // before write and we flush before servicing checkpoints), so the
        // snapshot lsn is the last written lsn. Force it durable first.
        if let Err(e) = self.cur().datasync() {
            fatal(&format!("fdatasync before checkpoint failed: {e}"));
        }
        let snap: TxnTableSnapshot = self.table.lock().snapshot();
        let bytes = serde_json::to_vec_pretty(&snap).map_err(|e| WalError::Io(e.to_string()))?;
        self.disk.write_atomic(&snapshot_path(&self.dir), &bytes).map_err(|e| WalError::Io(e.to_string()))?;
        let cutoff = snap.lsn.next();
        // A segment is deletable if it is not the current one and the *next*
        // segment's base lsn is <= cutoff (so all its records are in the
        // snapshot). Stop at the first one that is not.
        let deletable: Vec<u64> = self
            .segment_base
            .windows(2)
            .take_while(|w| w[0].0 != self.cur_id && w[1].1 <= cutoff)
            .map(|w| w[0].0)
            .collect();
        let mut removed = Vec::new();
        for id in deletable {
            let p = segment_path(&self.dir, id);
            match self.disk.remove(&p) {
                Ok(()) => removed.push(id),
                Err(e) => tracing::warn!(path = %p.display(), %e, "could not remove old segment"),
            }
        }
        if !removed.is_empty() {
            let _ = self.disk.fsync_dir(&self.dir);
            self.segments.retain(|i| !removed.contains(i));
            self.segment_base.retain(|(i, _)| !removed.contains(i));
            tracing::info!(?removed, "truncated log segments");
        }
        Ok(snap.lsn)
    }
}

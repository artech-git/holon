//! Simulated disk for crash-consistency tests.
//!
//! Keeps a *durable* image and a *volatile* image per file. Writes land in the
//! volatile image; `datasync`/`fsync` copy it to durable. `crash()` discards
//! the volatile image except for a random subset of 512-byte sectors (torn
//! writes). `fail_next_sync` makes the next sync return EIO so tests can
//! assert the writer aborts instead of retrying.

use crate::disk::{Disk, SegmentHandle};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SECTOR: usize = 512;

#[derive(Default, Clone)]
struct FileImg {
    durable: Vec<u8>,
    volatile: Vec<u8>,
}

/// Shared state behind a [`SimDisk`]; inspect it through [`SimDisk::state`].
#[derive(Default)]
pub struct SimState {
    files: BTreeMap<PathBuf, FileImg>,
    /// When set, the next `datasync`/`fsync`/`fsync_dir` fails with `EIO`
    /// and clears the flag.
    pub fail_next_sync: bool,
    /// Successful syncs so far.
    pub syncs: u64,
    /// `write_at` calls so far.
    pub writes: u64,
    rng: u64,
}

impl SimState {
    fn rand(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

/// In-memory [`Disk`] with separate durable and volatile images per file.
#[derive(Clone)]
pub struct SimDisk {
    state: Arc<Mutex<SimState>>,
}

impl SimDisk {
    /// Create an empty disk; `seed` drives which sectors survive [`SimDisk::crash`].
    pub fn new(seed: u64) -> SimDisk {
        let s = SimState { rng: seed.max(1), ..Default::default() };
        SimDisk { state: Arc::new(Mutex::new(s)) }
    }

    /// Handle to the shared state for assertions and fault injection.
    pub fn state(&self) -> Arc<Mutex<SimState>> {
        self.state.clone()
    }

    /// Simulate power loss: unsynced data survives only per random sector.
    pub fn crash(&self) {
        let mut s = self.state.lock();
        let mut rolls = Vec::new();
        for _ in 0..64 {
            rolls.push(s.rand());
        }
        let mut ri = 0;
        for img in s.files.values_mut() {
            let len = img.volatile.len().max(img.durable.len());
            let mut out = img.durable.clone();
            out.resize(len, 0);
            let mut off = 0;
            while off < img.volatile.len() {
                let end = (off + SECTOR).min(img.volatile.len());
                let keep = (rolls[ri % rolls.len()] >> (ri % 60)) & 1 == 1;
                ri += 1;
                if keep {
                    if out.len() < end {
                        out.resize(end, 0);
                    }
                    out[off..end].copy_from_slice(&img.volatile[off..end]);
                }
                off = end;
            }
            img.durable = out.clone();
            img.volatile = out;
        }
    }

    /// Make the next sync fail with `EIO`.
    pub fn set_fail_next_sync(&self) {
        self.state.lock().fail_next_sync = true;
    }
}

struct SimHandle {
    path: PathBuf,
    state: Arc<Mutex<SimState>>,
}

impl SegmentHandle for SimHandle {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut s = self.state.lock();
        s.writes += 1;
        let f = s.files.get_mut(&self.path).ok_or_else(|| io::Error::other("no file"))?;
        let end = offset as usize + data.len();
        if f.volatile.len() < end {
            f.volatile.resize(end, 0);
        }
        f.volatile[offset as usize..end].copy_from_slice(data);
        Ok(())
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let s = self.state.lock();
        let f = s.files.get(&self.path).ok_or_else(|| io::Error::other("no file"))?;
        let off = offset as usize;
        if off >= f.volatile.len() {
            return Ok(0);
        }
        let n = buf.len().min(f.volatile.len() - off);
        buf[..n].copy_from_slice(&f.volatile[off..off + n]);
        Ok(n)
    }
    fn datasync(&mut self) -> io::Result<()> {
        let mut s = self.state.lock();
        if s.fail_next_sync {
            s.fail_next_sync = false;
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        s.syncs += 1;
        let f = s.files.get_mut(&self.path).ok_or_else(|| io::Error::other("no file"))?;
        f.durable = f.volatile.clone();
        Ok(())
    }
    fn fsync(&mut self) -> io::Result<()> {
        self.datasync()
    }
    fn len(&mut self) -> io::Result<u64> {
        let s = self.state.lock();
        Ok(s.files.get(&self.path).map(|f| f.volatile.len() as u64).unwrap_or(0))
    }
    fn preallocate(&mut self, len: u64) -> io::Result<()> {
        let mut s = self.state.lock();
        let f = s.files.get_mut(&self.path).ok_or_else(|| io::Error::other("no file"))?;
        if f.volatile.len() < len as usize {
            f.volatile.resize(len as usize, 0);
        }
        Ok(())
    }
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let mut s = self.state.lock();
        let f = s.files.get_mut(&self.path).ok_or_else(|| io::Error::other("no file"))?;
        f.volatile.resize(len as usize, 0);
        Ok(())
    }
}

impl Disk for SimDisk {
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let s = self.state.lock();
        Ok(s.files.keys().filter(|p| p.parent() == Some(dir)).cloned().collect())
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn SegmentHandle>> {
        let mut s = self.state.lock();
        if s.files.contains_key(path) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        s.files.insert(path.to_path_buf(), FileImg::default());
        Ok(Box::new(SimHandle { path: path.to_path_buf(), state: self.state.clone() }))
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn SegmentHandle>> {
        let s = self.state.lock();
        if !s.files.contains_key(path) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(Box::new(SimHandle { path: path.to_path_buf(), state: self.state.clone() }))
    }
    fn read_all(&self, path: &Path) -> io::Result<Vec<u8>> {
        let s = self.state.lock();
        s.files.get(path).map(|f| f.volatile.clone()).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
    fn exists(&self, path: &Path) -> bool {
        self.state.lock().files.contains_key(path)
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        self.state.lock().files.remove(path);
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut s = self.state.lock();
        let f = s.files.remove(from).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        s.files.insert(to.to_path_buf(), f);
        Ok(())
    }
    fn write_atomic(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut s = self.state.lock();
        s.files.insert(path.to_path_buf(), FileImg { durable: data.to_vec(), volatile: data.to_vec() });
        Ok(())
    }
    fn fsync_dir(&self, _dir: &Path) -> io::Result<()> {
        let mut s = self.state.lock();
        if s.fail_next_sync {
            s.fail_next_sync = false;
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(())
    }
    fn create_dir_all(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }
}

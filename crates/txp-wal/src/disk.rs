//! The `Disk` seam. Production is `RealDisk` (std::fs + fdatasync); tests use
//! `sim::SimDisk` to inject torn writes, lost unsynced data and EIO.

use std::io;
use std::path::{Path, PathBuf};

/// An open segment file. All offsets are absolute; the handle keeps no
/// cursor so a batch is always written with one positional write.
#[allow(clippy::len_without_is_empty)]
pub trait SegmentHandle: Send {
    /// Write all of `data` at `offset` (`pwrite` semantics).
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// Fill `buf` from `offset`; returns the number of bytes read, which is
    /// short only at end of file.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// `fdatasync`: make data (not necessarily metadata) durable.
    fn datasync(&mut self) -> io::Result<()>;
    /// `fsync`: make data and metadata durable.
    fn fsync(&mut self) -> io::Result<()>;
    /// Current file size in bytes.
    fn len(&mut self) -> io::Result<u64>;
    /// Grow the file to `len` bytes of zeros up front so later appends
    /// change no metadata.
    fn preallocate(&mut self, len: u64) -> io::Result<()>;
    /// Shrink or extend the file to exactly `len` bytes.
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

/// Everything the log needs from a filesystem, so tests can substitute
/// a simulated one.
pub trait Disk: Send + Sync + 'static {
    /// All entries directly inside `dir`, sorted.
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    /// Create a new segment file; fails if it already exists.
    fn create(&self, path: &Path) -> io::Result<Box<dyn SegmentHandle>>;
    /// Open an existing segment file read-write.
    fn open(&self, path: &Path) -> io::Result<Box<dyn SegmentHandle>>;
    /// Read a whole file into memory.
    fn read_all(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;
    /// Delete a file.
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// Atomically rename `from` to `to`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Write `data` to `path` durably: tmp file, fsync, rename, fsync dir.
    fn write_atomic(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Make directory entries (creations, renames, deletions) durable.
    fn fsync_dir(&self, dir: &Path) -> io::Result<()>;
    /// Create `dir` and any missing parents.
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
}

/// The production [`Disk`]: `std::fs` plus `fallocate`/`fdatasync`.
pub struct RealDisk;

struct RealHandle(std::fs::File);

impl SegmentHandle for RealHandle {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.0.write_all_at(data, offset)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = self.0.read_at(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }
    fn datasync(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn fsync(&mut self) -> io::Result<()> {
        self.0.sync_all()
    }
    fn len(&mut self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }
    fn preallocate(&mut self, len: u64) -> io::Result<()> {
        use std::os::unix::io::AsRawFd;
        // fallocate with mode 0 allocates *and* extends the size with zeros,
        // so steady-state appends change no metadata and fdatasync suffices.
        let r = unsafe { libc::fallocate(self.0.as_raw_fd(), 0, 0, len as libc::off_t) };
        if r != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EOPNOTSUPP) {
                // Filesystem without fallocate: fall back to extending.
                return self.0.set_len(len);
            }
            return Err(e);
        }
        Ok(())
    }
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
}

impl Disk for RealDisk {
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        v.sort();
        Ok(v)
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn SegmentHandle>> {
        let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        Ok(Box::new(RealHandle(f)))
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn SegmentHandle>> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Box::new(RealHandle(f)))
    }
    fn read_all(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn write_atomic(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let dir = path.parent().unwrap_or(Path::new("."));
        let tmp = dir.join(format!(".{}.tmp", path.file_name().unwrap().to_string_lossy()));
        {
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        self.fsync_dir(dir)
    }
    fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
        std::fs::File::open(dir)?.sync_all()
    }
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }
}

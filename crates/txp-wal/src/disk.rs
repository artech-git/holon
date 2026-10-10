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
        use nix::errno::Errno;
        use nix::fcntl::{FallocateFlags, fallocate};
        // fallocate with mode 0 allocates *and* extends the size with zeros,
        // so steady-state appends change no metadata and fdatasync suffices.
        match fallocate(&self.0, FallocateFlags::empty(), 0, len as nix::libc::off_t) {
            Ok(()) => Ok(()),
            // Filesystem without fallocate: fall back to extending.
            Err(Errno::EOPNOTSUPP) => self.0.set_len(len),
            Err(e) => Err(e.into()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_segment_handle_reads_writes_syncs_and_resizes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("seg");
        let mut h = RealDisk.create(&p).unwrap();
        assert_eq!(RealDisk.create(&p).err().map(|e| e.kind()), Some(io::ErrorKind::AlreadyExists));
        h.preallocate(4096).unwrap();
        assert_eq!(h.len().unwrap(), 4096);
        h.write_at(10, b"hello").unwrap();
        h.datasync().unwrap();
        h.fsync().unwrap();
        let mut buf = [0u8; 5];
        assert_eq!(h.read_at(10, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"hello");
        // A read running past the end returns what is there.
        let mut tail = [0u8; 16];
        assert_eq!(h.read_at(4090, &mut tail).unwrap(), 6);
        h.truncate(12).unwrap();
        assert_eq!(h.len().unwrap(), 12);
        // fallocate refuses a length that does not fit in off_t.
        assert_eq!(h.preallocate(u64::MAX).unwrap_err().raw_os_error(), Some(nix::libc::EINVAL));
        let mut again = RealDisk.open(&p).unwrap();
        assert_eq!(again.len().unwrap(), 12);
        assert!(RealDisk.open(&d.path().join("missing")).is_err());
    }

    #[test]
    fn real_disk_directory_operations() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("a/b");
        RealDisk.create_dir_all(&dir).unwrap();
        RealDisk.write_atomic(&dir.join("snap.json"), b"{}").unwrap();
        assert_eq!(RealDisk.read_all(&dir.join("snap.json")).unwrap(), b"{}");
        RealDisk.create(&dir.join("x")).unwrap();
        RealDisk.rename(&dir.join("x"), &dir.join("y")).unwrap();
        assert!(!RealDisk.exists(&dir.join("x")));
        assert!(RealDisk.exists(&dir.join("y")));
        assert_eq!(RealDisk.list(&dir).unwrap(), vec![dir.join("snap.json"), dir.join("y")]);
        RealDisk.remove(&dir.join("y")).unwrap();
        RealDisk.fsync_dir(&dir).unwrap();
        assert_eq!(RealDisk.list(&dir).unwrap(), vec![dir.join("snap.json")]);
        assert!(RealDisk.list(&d.path().join("missing")).is_err());
    }
}

//! Idempotent redo publish.

use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// One idempotent publish step. The staged inode recorded at prepare
/// time lets a replay tell "already done" from "not yet".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RedoOp {
    /// `rename(staged, target)`: atomically replaces a file/symlink target.
    /// If the target is currently a directory it is removed first.
    PublishFile {
        /// Fully written file in the staging area.
        staged: PathBuf,
        /// Final path under the managed root.
        target: PathBuf,
        /// Inode of `staged`; a `target` with this inode is already published.
        ino: u64,
    },
    /// Replace a whole directory: `renameat2(RENAME_EXCHANGE)` if the target
    /// exists (old tree lands at `staged`, GC'd with the staging dir), plain
    /// rename otherwise.
    SwapDir {
        /// Complete replacement tree in the staging area.
        staged: PathBuf,
        /// Directory to replace.
        target: PathBuf,
        /// Inode of `staged`; a `target` with this inode is already published.
        ino: u64,
    },
    /// Remove `target` whatever it is.
    Delete {
        /// File, symlink or directory tree to remove; missing is fine.
        target: PathBuf,
    },
    /// Ensure a directory exists at `target` and (when given) apply the
    /// staged directory's owner and mode so published trees keep the
    /// attributes the sandboxed process gave them.
    Mkdir {
        /// Directory to ensure.
        target: PathBuf,
        /// Owner and mode to apply, when known.
        #[serde(default)]
        attrs: Option<DirAttrs>,
    },
}

/// Ownership and permission bits of a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirAttrs {
    /// Owner user id.
    pub uid: u32,
    /// Owner group id.
    pub gid: u32,
    /// Permission bits (`st_mode & 0o7777`).
    pub mode: u32,
}

impl DirAttrs {
    /// Read the attributes of `p` without following symlinks.
    pub fn of(p: &Path) -> io::Result<DirAttrs> {
        let m = std::fs::symlink_metadata(p)?;
        Ok(DirAttrs { uid: m.uid(), gid: m.gid(), mode: m.mode() & 0o7777 })
    }
}

fn apply_attrs(target: &Path, a: &DirAttrs) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let c = CString::new(target.as_os_str().as_encoded_bytes()).map_err(|_| io::Error::other("nul in path"))?;
    if unsafe { libc::lchown(c.as_ptr(), a.uid, a.gid) } != 0 {
        let e = io::Error::last_os_error();
        // Unprivileged daemons cannot chown; keep going with the mode.
        if e.raw_os_error() != Some(libc::EPERM) {
            return Err(e);
        }
    }
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(a.mode))
}

fn ino_of(p: &Path) -> Option<u64> {
    std::fs::symlink_metadata(p).ok().map(|m| m.ino())
}

fn fsync_parent(p: &Path) -> io::Result<()> {
    if let Some(parent) = p.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn rename_exchange(a: &Path, b: &Path) -> io::Result<()> {
    let ca = CString::new(a.as_os_str().as_encoded_bytes()).map_err(|_| io::Error::other("nul in path"))?;
    let cb = CString::new(b.as_os_str().as_encoded_bytes()).map_err(|_| io::Error::other("nul in path"))?;
    const RENAME_EXCHANGE: libc::c_uint = 2;
    let r = unsafe {
        libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, ca.as_ptr(), libc::AT_FDCWD, cb.as_ptr(), RENAME_EXCHANGE)
    };
    if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn remove_any(p: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Apply one op. Safe to call again after a crash at any point.
pub fn apply(op: &RedoOp) -> io::Result<()> {
    match op {
        RedoOp::PublishFile { staged, target, ino } => {
            if ino_of(target) == Some(*ino) {
                return Ok(()); // already published
            }
            if !staged.exists() && std::fs::symlink_metadata(staged).is_err() {
                return Err(io::Error::other(format!(
                    "redo anomaly: staged {} missing and target {} is not it",
                    staged.display(),
                    target.display()
                )));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if let Ok(m) = std::fs::symlink_metadata(target)
                && m.is_dir() {
                    std::fs::remove_dir_all(target)?;
                }
            std::fs::rename(staged, target)?;
            fsync_parent(target)
        }
        RedoOp::SwapDir { staged, target, ino } => {
            if ino_of(target) == Some(*ino) {
                return Ok(());
            }
            if std::fs::symlink_metadata(staged).is_err() {
                return Err(io::Error::other(format!("redo anomaly: staged dir {} missing", staged.display())));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::symlink_metadata(target) {
                Ok(m) if m.is_dir() => rename_exchange(staged, target)?,
                Ok(_) => {
                    std::fs::remove_file(target)?;
                    std::fs::rename(staged, target)?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::rename(staged, target)?,
                Err(e) => return Err(e),
            }
            fsync_parent(target)
        }
        RedoOp::Delete { target } => {
            remove_any(target)?;
            fsync_parent(target)
        }
        RedoOp::Mkdir { target, attrs } => {
            if let Ok(m) = std::fs::symlink_metadata(target)
                && !m.is_dir() {
                    std::fs::remove_file(target)?;
                }
            std::fs::create_dir_all(target)?;
            if let Some(a) = attrs {
                apply_attrs(target, a)?;
            }
            fsync_parent(target)
        }
    }
}

/// Apply all ops in order. Idempotent as a whole.
pub fn execute_redo(ops: &[RedoOp]) -> io::Result<()> {
    for op in ops {
        apply(op).map_err(|e| io::Error::new(e.kind(), format!("{op:?}: {e}")))?;
    }
    Ok(())
}

/// fsync every file and directory under `p` (prepare durability).
pub fn fsync_tree(p: &Path) -> io::Result<()> {
    let m = std::fs::symlink_metadata(p)?;
    if m.is_dir() {
        for e in std::fs::read_dir(p)? {
            fsync_tree(&e?.path())?;
        }
        std::fs::File::open(p)?.sync_all()
    } else if m.is_file() {
        std::fs::File::open(p)?.sync_all()
    } else {
        Ok(()) // symlinks etc.: durability comes from the parent dir fsync
    }
}

/// Recursive copy used by `replace_tree` staging.
pub fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let m = std::fs::symlink_metadata(from)?;
    if m.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else if m.file_type().is_symlink() {
        let t = std::fs::read_link(from)?;
        std::os::unix::fs::symlink(t, to)
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_file_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let staged = d.path().join("s");
        let target = d.path().join("t");
        std::fs::write(&staged, b"new").unwrap();
        std::fs::write(&target, b"old").unwrap();
        let ino = ino_of(&staged).unwrap();
        let op = RedoOp::PublishFile { staged: staged.clone(), target: target.clone(), ino };
        apply(&op).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        apply(&op).unwrap(); // replay
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn swap_dir_exchanges_and_replays() {
        let d = tempfile::tempdir().unwrap();
        let staged = d.path().join("s");
        let target = d.path().join("t");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(staged.join("f"), b"new").unwrap();
        std::fs::write(target.join("f"), b"old").unwrap();
        let ino = ino_of(&staged).unwrap();
        let op = RedoOp::SwapDir { staged: staged.clone(), target: target.clone(), ino };
        apply(&op).unwrap();
        assert_eq!(std::fs::read(target.join("f")).unwrap(), b"new");
        assert_eq!(std::fs::read(staged.join("f")).unwrap(), b"old");
        apply(&op).unwrap();
        assert_eq!(std::fs::read(target.join("f")).unwrap(), b"new");
    }

    #[test]
    fn file_over_dir_and_dir_over_file() {
        let d = tempfile::tempdir().unwrap();
        let sf = d.path().join("sf");
        let td = d.path().join("td");
        std::fs::write(&sf, b"x").unwrap();
        std::fs::create_dir_all(td.join("inner")).unwrap();
        apply(&RedoOp::PublishFile { staged: sf.clone(), target: td.clone(), ino: ino_of(&sf).unwrap() }).unwrap();
        assert!(td.is_file());
        let sd = d.path().join("sd");
        std::fs::create_dir_all(&sd).unwrap();
        apply(&RedoOp::SwapDir { staged: sd.clone(), target: td.clone(), ino: ino_of(&sd).unwrap() }).unwrap();
        assert!(td.is_dir());
    }
}

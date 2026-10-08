//! Translate an overlayfs upperdir into filesystem redo operations.
//!
//! Supported: regular files, directories (merged or opaque), symlinks,
//! whiteouts (char device 0:0). Rejected with a `VoteNo`: hard links, device
//! nodes, fifos, sockets, `redirect`/`metacopy` xattrs. The overlay must be
//! unmounted (its mount namespace is gone) before this runs.

use std::ffi::CString;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use txp_fs::RedoOp;

const OPAQUE: &str = "trusted.overlay.opaque";
const REDIRECT: &str = "trusted.overlay.redirect";
const METACOPY: &str = "trusted.overlay.metacopy";

fn lgetxattr(p: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    let cp = CString::new(p.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    let cn = CString::new(name).map_err(io::Error::other)?;
    let mut buf = vec![0u8; 256];
    let r = unsafe { libc::lgetxattr(cp.as_ptr(), cn.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENODATA) | Some(libc::ENOTSUP) => Ok(None),
            _ => Err(e),
        };
    }
    buf.truncate(r as usize);
    Ok(Some(buf))
}

fn llistxattr(p: &Path) -> io::Result<Vec<String>> {
    let cp = CString::new(p.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    let mut buf = vec![0u8; 4096];
    let r = unsafe { libc::llistxattr(cp.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENOTSUP) => Ok(vec![]),
            _ => Err(e),
        };
    }
    buf.truncate(r as usize);
    Ok(buf.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect())
}

fn lremovexattr(p: &Path, name: &str) -> io::Result<()> {
    let cp = CString::new(p.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    let cn = CString::new(name).map_err(io::Error::other)?;
    let r = unsafe { libc::lremovexattr(cp.as_ptr(), cn.as_ptr()) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn is_whiteout(m: &std::fs::Metadata) -> bool {
    m.file_type().is_char_device() && m.rdev() == 0
}

/// Remove overlay bookkeeping from a subtree that will be published as-is
/// (an opaque directory): whiteouts inside it are meaningless once the
/// lower is gone, and `trusted.overlay.*` xattrs must not leak.
fn clean_tree(p: &Path) -> Result<(), String> {
    let m = std::fs::symlink_metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if is_whiteout(&m) {
        return std::fs::remove_file(p).map_err(|e| format!("{}: {e}", p.display()));
    }
    strip_xattrs(p, &m)?;
    if m.is_dir() {
        for e in std::fs::read_dir(p).map_err(|e| format!("{}: {e}", p.display()))? {
            let e = e.map_err(|e| e.to_string())?;
            clean_tree(&e.path())?;
        }
    } else {
        check_regular(p, &m)?;
    }
    Ok(())
}

fn strip_xattrs(p: &Path, m: &std::fs::Metadata) -> Result<(), String> {
    if m.file_type().is_symlink() {
        return Ok(());
    }
    for x in llistxattr(p).map_err(|e| format!("listxattr {}: {e}", p.display()))? {
        if x.starts_with("trusted.overlay.") {
            if x == REDIRECT || x == METACOPY {
                return Err(format!("{}: unsupported overlay feature {x}", p.display()));
            }
            lremovexattr(p, &x).map_err(|e| format!("removexattr {}: {e}", p.display()))?;
        }
    }
    Ok(())
}

fn check_regular(p: &Path, m: &std::fs::Metadata) -> Result<(), String> {
    let ft = m.file_type();
    if ft.is_symlink() || ft.is_file() {
        if ft.is_file() && m.nlink() > 1 {
            return Err(format!("{}: hard links are not supported", p.display()));
        }
        Ok(())
    } else {
        Err(format!("{}: unsupported file type (fifo/socket/device)", p.display()))
    }
}

fn walk(upper: &Path, root: &Path, rel: &Path, out: &mut Vec<RedoOp>) -> Result<(), String> {
    let dir = upper.join(rel);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let rel_child: PathBuf = rel.join(&name);
        let staged = upper.join(&rel_child);
        let target = root.join(&rel_child);
        let m = std::fs::symlink_metadata(&staged).map_err(|e| format!("{}: {e}", staged.display()))?;
        if is_whiteout(&m) {
            out.push(RedoOp::Delete { target });
            continue;
        }
        if m.is_dir() {
            let opaque = lgetxattr(&staged, OPAQUE)
                .map_err(|e| format!("getxattr {}: {e}", staged.display()))?
                .map(|v| v == b"y")
                .unwrap_or(false);
            if lgetxattr(&staged, REDIRECT).map_err(|e| e.to_string())?.is_some() {
                return Err(format!("{}: directory rename (redirect) is not supported", staged.display()));
            }
            if opaque {
                clean_tree(&staged)?;
                out.push(RedoOp::SwapDir { staged, target, ino: m.ino() });
            } else {
                strip_xattrs(&staged, &m)?;
                let attrs = txp_fs::publish::DirAttrs::of(&staged).map_err(|e| e.to_string())?;
                out.push(RedoOp::Mkdir { target, attrs: Some(attrs) });
                walk(upper, root, &rel_child, out)?;
            }
            continue;
        }
        check_regular(&staged, &m)?;
        strip_xattrs(&staged, &m)?;
        out.push(RedoOp::PublishFile { staged, target, ino: m.ino() });
    }
    Ok(())
}

/// Build the redo list for publishing `upper` onto `root`.
pub fn translate(upper: &Path, root: &Path) -> Result<Vec<RedoOp>, String> {
    let mut out = Vec::new();
    if !upper.is_dir() {
        return Ok(out);
    }
    // The upper root itself may carry xattrs (never opaque at the top).
    let m = std::fs::symlink_metadata(upper).map_err(|e| e.to_string())?;
    strip_xattrs(upper, &m)?;
    walk(upper, root, Path::new(""), &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_files_dirs_and_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let upper = d.path().join("upper");
        let root = d.path().join("root");
        std::fs::create_dir_all(upper.join("sub")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(upper.join("a.txt"), "a").unwrap();
        std::fs::write(upper.join("sub/b.txt"), "b").unwrap();
        std::os::unix::fs::symlink("a.txt", upper.join("link")).unwrap();
        let ops = translate(&upper, &root).unwrap();
        let kinds: Vec<String> = ops.iter().map(|o| format!("{o:?}").split_whitespace().next().unwrap().to_string()).collect();
        assert_eq!(kinds, vec!["PublishFile", "PublishFile", "Mkdir", "PublishFile"]);
        txp_fs::execute_redo(&ops).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("sub/b.txt")).unwrap(), "b");
        assert!(root.join("link").is_symlink());
    }

    #[test]
    fn rejects_fifo() {
        let d = tempfile::tempdir().unwrap();
        let upper = d.path().join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        let p = CString::new(upper.join("f").to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(p.as_ptr(), 0o644) }, 0);
        assert!(translate(&upper, d.path()).is_err());
    }
}

//! Translate an overlayfs upperdir into filesystem redo operations.
//!
//! Supported: regular files, directories (merged or opaque), symlinks,
//! whiteouts (char device 0:0). Rejected with a `VoteNo`: hard links, device
//! nodes, fifos, sockets, `redirect`/`metacopy` xattrs. The overlay must be
//! unmounted (its mount namespace is gone) before this runs.

use nix::errno::Errno;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use txp_fs::RedoOp;

const OPAQUE: &str = "trusted.overlay.opaque";
const REDIRECT: &str = "trusted.overlay.redirect";
const METACOPY: &str = "trusted.overlay.metacopy";

/// The filesystem has no xattr support: there is nothing to read or list.
fn unsupported(e: &io::Error) -> bool {
    e.raw_os_error().map(Errno::from_raw) == Some(Errno::ENOTSUP)
}

// The `xattr` crate's plain functions are the `l*` variants: they act on a
// symlink itself rather than its target.

fn lgetxattr(p: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    xattr::get(p, name).or_else(|e| if unsupported(&e) { Ok(None) } else { Err(e) })
}

fn llistxattr(p: &Path) -> io::Result<Vec<String>> {
    let names = xattr::list(p).map(|names| names.map(|n| n.to_string_lossy().into_owned()).collect());
    // Local filesystems answer an empty list; FUSE or NFS may refuse.
    names.or_else(|e| if unsupported(&e) { Ok(vec![]) } else { Err(e) })
}

/// An overlay xattr of `p`, with any failure as a message.
fn overlay_xattr(p: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
    lgetxattr(p, name).map_err(|e| format!("getxattr {}: {e}", p.display()))
}

fn lremovexattr(p: &Path, name: &str) -> io::Result<()> {
    xattr::remove(p, name)
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
            let opaque = overlay_xattr(&staged, OPAQUE)?.is_some_and(|v| v == b"y");
            if overlay_xattr(&staged, REDIRECT)?.is_some() {
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
    fn foreign_xattrs_stay_and_hard_links_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let upper = d.path().join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::write(upper.join("f"), "x").unwrap();
        xattr::set(upper.join("f"), "user.keep", b"1").unwrap();
        assert_eq!(translate(&upper, d.path()).unwrap().len(), 1);
        assert_eq!(xattr::get(upper.join("f"), "user.keep").unwrap(), Some(b"1".to_vec()), "only overlay xattrs are stripped");
        std::fs::hard_link(upper.join("f"), upper.join("g")).unwrap();
        assert!(translate(&upper, d.path()).unwrap_err().contains("hard links are not supported"));
    }

    #[test]
    fn a_missing_upper_is_no_change_and_an_unreadable_one_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        assert!(translate(&d.path().join("none"), d.path()).unwrap().is_empty());
        let upper = d.path().join("upper");
        std::fs::create_dir_all(upper.join("sub")).unwrap();
        std::fs::set_permissions(upper.join("sub"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let r = translate(&upper, d.path());
        std::fs::set_permissions(upper.join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // (root reads it anyway)
        assert!(r.is_err() || nix::unistd::geteuid().is_root(), "{r:?}");
    }

    #[test]
    fn xattr_reads_tolerate_filesystems_without_xattrs() {
        // procfs supports none: nothing to read, rather than an error.
        assert_eq!(lgetxattr(Path::new("/proc/self/status"), OPAQUE).unwrap(), None);
        assert!(lgetxattr(Path::new("/nonexistent"), OPAQUE).is_err());
        assert!(llistxattr(Path::new("/nonexistent")).is_err());
        assert!(overlay_xattr(Path::new("/nonexistent"), OPAQUE).unwrap_err().starts_with("getxattr /nonexistent: "));
    }

    #[test]
    fn rejects_fifo() {
        let d = tempfile::tempdir().unwrap();
        let upper = d.path().join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        nix::unistd::mkfifo(&upper.join("f"), nix::sys::stat::Mode::from_bits_truncate(0o644)).unwrap();
        assert!(translate(&upper, d.path()).is_err());
    }
}

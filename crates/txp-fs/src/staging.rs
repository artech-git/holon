//! Where staged files live: a same-filesystem sibling of the managed root.

use std::io;
use std::path::{Path, PathBuf};
use txp_core::TxId;

/// `<parent>/.txp-stage/<rootname>` for a managed root. Must be on the same
/// filesystem as `root` (it is, being a sibling) unless `root` is itself a
/// mount point; the engine's startup self-test checks `st_dev`.
pub fn stage_root_for(root: &Path) -> PathBuf {
    let parent = root.parent().unwrap_or(Path::new("/"));
    let name = root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "root".into());
    parent.join(".txp-stage").join(name)
}

/// Per-transaction staging directory.
#[derive(Clone, Debug)]
pub struct StageDir {
    /// `<stage root>/<txid>` or a sub-stage below it.
    pub path: PathBuf,
}

impl StageDir {
    /// Create (if needed) the staging directory for `txid` under `root`'s stage root.
    pub fn create(root: &Path, txid: TxId) -> io::Result<StageDir> {
        let path = stage_root_for(root).join(txid.to_string());
        std::fs::create_dir_all(&path)?;
        Ok(StageDir { path })
    }

    /// Refer to the staging directory for `txid` without touching the filesystem.
    pub fn open(root: &Path, txid: TxId) -> StageDir {
        StageDir { path: stage_root_for(root).join(txid.to_string()) }
    }

    /// Create and return `<path>/<name>`.
    pub fn subdir(&self, name: &str) -> io::Result<PathBuf> {
        let p = self.path.join(name);
        std::fs::create_dir_all(&p)?;
        Ok(p)
    }

    /// Remove this staging directory. If it is a sub-stage (e.g. `<txid>/files`)
    /// the now-empty `<txid>` parent is removed too; a non-empty parent is
    /// left for the other participant that still uses it.
    pub fn discard(&self) -> io::Result<()> {
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(parent) = self.path.parent() {
            if parent.file_name().and_then(|n| n.to_str()).and_then(TxId::parse).is_some() {
                let _ = std::fs::remove_dir(parent);
            }
            if let Ok(f) = std::fs::File::open(parent.parent().unwrap_or(parent)) {
                let _ = f.sync_all();
            }
        }
        Ok(())
    }
}

/// Same-device check: staging must be on the same filesystem as the root.
pub fn same_device(a: &Path, b: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(a)?.dev() == std::fs::metadata(b)?.dev())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_root_is_a_hidden_sibling() {
        assert_eq!(stage_root_for(Path::new("/srv/site")), Path::new("/srv/.txp-stage/site"));
        assert_eq!(stage_root_for(Path::new("/")), Path::new("/.txp-stage/root"));
        let sd = StageDir::open(Path::new("/srv/site"), TxId(1));
        assert_eq!(sd.path, Path::new("/srv/.txp-stage/site").join(TxId(1).to_string()));
    }

    #[test]
    fn discarding_a_sub_stage_removes_its_empty_txid_parent_only() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("site");
        std::fs::create_dir(&root).unwrap();
        let sd = StageDir::create(&root, TxId(7)).unwrap();
        let files = StageDir { path: sd.subdir("files").unwrap() };
        let other = sd.subdir("proc").unwrap();
        files.discard().unwrap();
        assert!(sd.path.exists(), "parent still used by another sub-stage");
        StageDir { path: other }.discard().unwrap();
        assert!(!sd.path.exists(), "empty txid parent removed");
        // Discarding again, or a whole txid stage, is fine.
        files.discard().unwrap();
        StageDir::create(&root, TxId(8)).unwrap().discard().unwrap();
        assert!(same_device(&root, &stage_root_for(&root)).unwrap());
    }

    #[test]
    fn discard_odd_paths() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(StageDir { path: file.join("x") }.discard().is_err());
        StageDir { path: PathBuf::from("/nonexistent/a/b") }.discard().unwrap();
        StageDir { path: PathBuf::new() }.discard().unwrap();
        assert!(same_device(&file, &d.path().join("missing")).is_err());
    }
}

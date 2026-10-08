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

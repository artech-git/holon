//! cgroup v2 leaf per sandboxed step. `cgroup.kill` (Linux ≥ 5.14) kills the
//! whole subtree atomically and copes with concurrent forks.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Mount point of the unified cgroup v2 hierarchy.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// A leaf cgroup under `/sys/fs/cgroup/txp/`.
#[derive(Debug, Clone)]
pub struct Cgroup {
    /// Directory of the cgroup.
    pub path: PathBuf,
}

impl Cgroup {
    /// Create `txp/<name>` (idempotent) and verify `cgroup.kill` is present.
    pub fn create(name: &str) -> io::Result<Cgroup> {
        let parent = Path::new(CGROUP_ROOT).join("txp");
        std::fs::create_dir_all(&parent)?;
        let path = parent.join(name);
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        if !path.join("cgroup.kill").exists() {
            return Err(io::Error::other("cgroup.kill not available (need cgroup v2 on Linux >= 5.14)"));
        }
        Ok(Cgroup { path })
    }

    /// Path of `cgroup.procs`; writing `0` there moves the caller in.
    pub fn procs_path(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    /// Move process `pid` in. Its later children are born inside.
    pub fn add(&self, pid: u32) -> io::Result<()> {
        std::fs::write(self.procs_path(), format!("{pid}\n"))
    }

    /// Kill every process in the subtree. A cgroup that no longer exists counts as killed.
    pub fn kill(&self) -> io::Result<()> {
        match std::fs::write(self.path.join("cgroup.kill"), "1") {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether any process is still inside, per `cgroup.events`.
    pub fn populated(&self) -> io::Result<bool> {
        let s = std::fs::read_to_string(self.path.join("cgroup.events"))?;
        Ok(s.lines().any(|l| l.trim() == "populated 1"))
    }

    /// Wait until no process remains. Polls `cgroup.events` (an inotify
    /// watch is the non-polling upgrade path).
    pub async fn wait_empty(&self, timeout: Duration) -> io::Result<bool> {
        let start = Instant::now();
        loop {
            match self.populated() {
                Ok(false) => return Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(true),
                Err(e) => return Err(e),
                Ok(true) => {}
            }
            if start.elapsed() > timeout {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Remove the (empty) cgroup directory. Idempotent.
    pub fn remove(&self) -> io::Result<()> {
        match std::fs::remove_dir(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Kill, wait, remove. Best effort; errors are logged by the caller.
    pub async fn destroy(&self) -> io::Result<()> {
        self.kill()?;
        self.wait_empty(Duration::from_secs(10)).await?;
        self.remove()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain directory standing in for a cgroup.
    fn fake(events: &str) -> (tempfile::TempDir, Cgroup) {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("cg");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("cgroup.events"), events).unwrap();
        (d, Cgroup { path })
    }

    #[tokio::test]
    async fn waiting_follows_cgroup_events() {
        let (_d, cg) = fake("populated 1\nfrozen 0\n");
        assert!(cg.populated().unwrap());
        assert!(!cg.wait_empty(Duration::from_millis(20)).await.unwrap(), "still populated at the timeout");
        std::fs::write(cg.path.join("cgroup.events"), "populated 0\n").unwrap();
        assert!(cg.wait_empty(Duration::from_secs(1)).await.unwrap());
        // A cgroup that is gone is empty; anything else unreadable is an error.
        assert!(Cgroup { path: cg.path.join("gone") }.wait_empty(Duration::ZERO).await.unwrap());
        assert!(Cgroup { path: cg.path.join("cgroup.events") }.wait_empty(Duration::ZERO).await.is_err());
    }

    #[tokio::test]
    async fn kill_add_remove_and_destroy() {
        let (_d, cg) = fake("populated 0\n");
        cg.kill().unwrap();
        assert_eq!(std::fs::read_to_string(cg.path.join("cgroup.kill")).unwrap(), "1");
        cg.add(42).unwrap();
        assert_eq!(std::fs::read_to_string(cg.procs_path()).unwrap(), "42\n");
        assert!(cg.remove().is_err(), "a directory with files cannot be removed");
        assert!(cg.destroy().await.is_err());
        let not_a_dir = Cgroup { path: cg.path.join("cgroup.events") };
        assert!(not_a_dir.kill().is_err());
        let gone = Cgroup { path: cg.path.join("gone") };
        gone.kill().unwrap();
        gone.remove().unwrap();
        gone.destroy().await.unwrap();
    }
}

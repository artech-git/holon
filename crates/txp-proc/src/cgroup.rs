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

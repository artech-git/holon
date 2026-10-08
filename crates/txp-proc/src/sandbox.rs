//! Spawn a process in a private staged view.
//!
//! Implementation of the `unshare -pf` pattern inside `pre_exec`: the forked
//! child joins the cgroup, unshares namespaces, forks again (the grandchild
//! becomes PID 1 of the new PID namespace), sets up mounts, drops privileges,
//! applies Landlock and execs. The intermediate waits and exits with the
//! grandchild's status. Everything in the child runs after `fork()` in a
//! multithreaded parent, so only async-signal-safe calls are used and all
//! strings are prepared beforehand.

use crate::cgroup::Cgroup;
use crate::landlock;
use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// One overlayfs mount inside the sandbox.
#[derive(Clone, Debug)]
pub struct OverlayMount {
    /// Topmost first; the real root is last.
    pub lowers: Vec<PathBuf>,
    /// Writable layer; becomes the diff that prepare translates.
    pub upper: PathBuf,
    /// overlayfs `workdir`; must be on the same filesystem as `upper`.
    pub work: PathBuf,
    /// Mount point inside the sandbox (usually the real root path).
    pub at: PathBuf,
}

/// Everything needed to run one confined process.
#[derive(Clone, Debug)]
pub struct SandboxSpec {
    /// Program and arguments; `argv[0]` is resolved via `PATH` from `env`.
    pub argv: Vec<String>,
    /// Complete environment; the parent's is not inherited.
    pub env: Vec<(String, String)>,
    /// Working directory, resolved after the mounts are in place.
    pub cwd: Option<PathBuf>,
    /// Overlay mounts to set up, in order.
    pub mounts: Vec<OverlayMount>,
    /// User id to drop to before `exec`.
    pub uid: u32,
    /// Group id to drop to before `exec` (supplementary groups are cleared).
    pub gid: u32,
    /// Wall-clock limit; on expiry the cgroup is killed.
    pub timeout: Duration,
    /// Cgroup the process tree is placed in. Not destroyed by [`run`].
    pub cgroup: Cgroup,
    /// Fail closed if Landlock cannot be applied.
    pub landlock: bool,
    /// Install the seccomp syscall denylist; fail closed if it cannot be applied.
    pub seccomp: bool,
    /// Mount a fresh tmpfs on `/tmp` (skipped when an overlay lives under `/tmp`).
    pub private_tmp: bool,
    /// Bytes of stdout and of stderr to keep; the rest is drained and dropped.
    pub output_limit: usize,
}

/// How the process ended.
#[derive(Clone, Debug)]
pub struct SandboxResult {
    /// Exit status, or `None` if killed by a signal.
    pub exit_code: Option<i32>,
    /// Terminating signal, if any.
    pub signal: Option<i32>,
    /// The timeout fired and the cgroup was killed.
    pub timed_out: bool,
    /// Captured stdout (lossy UTF-8, truncated to `output_limit`).
    pub stdout: String,
    /// Captured stderr (lossy UTF-8, truncated to `output_limit`).
    pub stderr: String,
}

impl SandboxResult {
    /// Exited with status 0 and did not time out.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

fn cstr(p: impl AsRef<[u8]>) -> io::Result<CString> {
    CString::new(p.as_ref()).map_err(|_| io::Error::other("NUL in string"))
}

fn check_overlay_path(p: &Path) -> io::Result<()> {
    let s = p.to_string_lossy();
    if s.contains(':') || s.contains(',') || s.contains('\\') {
        return Err(io::Error::other(format!("path {s:?} contains characters overlayfs cannot escape")));
    }
    if !p.is_absolute() {
        return Err(io::Error::other(format!("path {s:?} must be absolute")));
    }
    Ok(())
}

struct PreparedMount {
    opts: CString,
    at: CString,
}

/// Run to completion (or kill on timeout). The cgroup is left to the caller
/// to destroy so that an abort racing with the run can kill it.
pub async fn run(spec: SandboxSpec) -> io::Result<SandboxResult> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other("the process participant must run as root (no user-namespace mode yet)"));
    }
    let abi = landlock::abi_version();
    if spec.landlock && abi < 1 {
        return Err(io::Error::other("Landlock is not available on this kernel; refusing to run unconfined"));
    }
    if spec.argv.is_empty() {
        return Err(io::Error::other("empty argv"));
    }

    // Prepare everything the child needs, outside the child.
    let mut mounts = Vec::new();
    let mut rules_paths: Vec<CString> = Vec::new();
    for m in &spec.mounts {
        for l in &m.lowers {
            check_overlay_path(l)?;
        }
        check_overlay_path(&m.upper)?;
        check_overlay_path(&m.work)?;
        std::fs::create_dir_all(&m.upper)?;
        std::fs::create_dir_all(&m.work)?;
        // The merged root takes its attributes from the upperdir: mirror the
        // real root's owner and mode so permission checks match reality.
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let rm = std::fs::metadata(m.lowers.last().unwrap())?;
            let cu = cstr(m.upper.as_os_str().as_encoded_bytes())?;
            if unsafe { libc::chown(cu.as_ptr(), rm.uid(), rm.gid()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            std::fs::set_permissions(&m.upper, std::fs::Permissions::from_mode(rm.mode() & 0o7777))?;
        }
        let lowers: Vec<String> = m.lowers.iter().map(|p| p.to_string_lossy().to_string()).collect();
        let opts = format!(
            "lowerdir={},upperdir={},workdir={},redirect_dir=off,index=off,metacopy=off",
            lowers.join(":"),
            m.upper.display(),
            m.work.display()
        );
        mounts.push(PreparedMount { opts: cstr(opts)?, at: cstr(m.at.as_os_str().as_encoded_bytes())? });
        rules_paths.push(cstr(m.at.as_os_str().as_encoded_bytes())?);
    }
    let procs_path = cstr(spec.cgroup.procs_path().as_os_str().as_encoded_bytes())?;
    let cwd = match &spec.cwd {
        Some(c) => Some(cstr(c.as_os_str().as_encoded_bytes())?),
        None => None,
    };
    let private_tmp = spec.private_tmp && !spec.mounts.iter().any(|m| m.at.starts_with("/tmp"));
    let uid = spec.uid;
    let gid = spec.gid;
    let use_landlock = spec.landlock;
    // Compiled in the parent so the post-fork child allocates nothing.
    let seccomp_filter = if spec.seccomp { Some(crate::seccomp::Filter::compile()) } else { None };
    let c_root = cstr("/")?;
    let c_tmp = cstr("/tmp")?;
    let c_dev = cstr("/dev")?;
    let c_proc = cstr("/proc")?;
    let c_overlay = cstr("overlay")?;
    let c_proc_fs = cstr("proc")?;
    let c_tmpfs = cstr("tmpfs")?;
    let c_tmp_opts = cstr("mode=1777")?;

    let mut cmd = tokio::process::Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    cmd.env_clear();
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(false);

    unsafe {
        cmd.pre_exec(move || {
            // 1. Join the cgroup (write "0" = this pid).
            let fd = libc::open(procs_path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let w = libc::write(fd, b"0\n".as_ptr() as *const libc::c_void, 2);
            libc::close(fd);
            if w != 2 {
                return Err(io::Error::other("cgroup.procs write failed"));
            }
            // 2. New namespaces.
            let flags = libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS;
            if libc::unshare(flags) != 0 {
                return Err(io::Error::last_os_error());
            }
            // 3. Fork: the grandchild is PID 1 in the new PID namespace.
            let pid = libc::fork();
            if pid < 0 {
                return Err(io::Error::last_os_error());
            }
            if pid > 0 {
                // Intermediate: die with the daemon, wait for the leader,
                // propagate its status.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                // Drop every inherited descriptor (including std's CLOEXEC
                // exec-error pipe, which would otherwise make `spawn()` block
                // until this reaper exits).
                libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
                let mut status: libc::c_int = 0;
                loop {
                    let r = libc::waitpid(pid, &mut status, 0);
                    if r == pid {
                        break;
                    }
                    if r < 0 && *libc::__errno_location() != libc::EINTR {
                        libc::_exit(127);
                    }
                }
                if libc::WIFEXITED(status) {
                    libc::_exit(libc::WEXITSTATUS(status));
                } else if libc::WIFSIGNALED(status) {
                    libc::_exit(128 + libc::WTERMSIG(status));
                }
                libc::_exit(127);
            }
            // 4. Mounts (grandchild).
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            if libc::mount(std::ptr::null(), c_root.as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::mount(
                c_proc_fs.as_ptr(),
                c_proc.as_ptr(),
                c_proc_fs.as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                std::ptr::null(),
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if private_tmp
                && libc::mount(
                    c_tmpfs.as_ptr(),
                    c_tmp.as_ptr(),
                    c_tmpfs.as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV,
                    c_tmp_opts.as_ptr() as *const libc::c_void,
                ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            for m in &mounts {
                if libc::mount(c_overlay.as_ptr(), m.at.as_ptr(), c_overlay.as_ptr(), 0, m.opts.as_ptr() as *const libc::c_void) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            // 5. Drop privileges.
            if libc::setgroups(0, std::ptr::null()) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                return Err(io::Error::last_os_error());
            }
            // 6. Landlock: read-only everywhere except the overlays, /tmp, /dev.
            if use_landlock {
                let full = landlock::fs_rights_for_abi(abi);
                let mut rules = Vec::with_capacity(rules_paths.len() + 3);
                rules.push(landlock::Rule { path: c_root.as_c_str(), allowed: landlock::read_only_rights() });
                rules.push(landlock::Rule { path: c_tmp.as_c_str(), allowed: full });
                rules.push(landlock::Rule { path: c_dev.as_c_str(), allowed: landlock::FS_READ_FILE | landlock::FS_WRITE_FILE });
                for p in &rules_paths {
                    rules.push(landlock::Rule { path: p.as_c_str(), allowed: full });
                }
                landlock::restrict_self(abi, &rules)?;
            } else {
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
            }
            // 6b. seccomp syscall denylist: additive defence in depth. Runs
            // after no_new_privs is set (by Landlock or the branch above) and
            // before exec; fails closed if the kernel cannot apply it.
            if let Some(f) = &seccomp_filter {
                f.install()?;
            }
            // 7. cwd inside the staged view (must come after the mounts).
            if let Some(c) = &cwd
                && libc::chdir(c.as_ptr()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    let limit = spec.output_limit;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let rd_out = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = (&mut out).take(limit as u64).read_to_end(&mut buf).await;
        // drain the rest so the child never blocks on a full pipe
        let mut sink = [0u8; 4096];
        while let Ok(n) = out.read(&mut sink).await {
            if n == 0 {
                break;
            }
        }
        buf
    });
    let rd_err = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = (&mut err).take(limit as u64).read_to_end(&mut buf).await;
        let mut sink = [0u8; 4096];
        while let Ok(n) = err.read(&mut sink).await {
            if n == 0 {
                break;
            }
        }
        buf
    });

    let mut timed_out = false;
    let status = tokio::select! {
        s = child.wait() => s?,
        _ = tokio::time::sleep(spec.timeout) => {
            timed_out = true;
            let _ = spec.cgroup.kill();
            child.wait().await?
        }
    };
    // Make sure nothing lingers (e.g. the intermediate), then let the caller
    // tear the cgroup down.
    let _ = spec.cgroup.kill();
    let _ = spec.cgroup.wait_empty(Duration::from_secs(10)).await;

    use std::os::unix::process::ExitStatusExt;
    let stdout = String::from_utf8_lossy(&rd_out.await.unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&rd_err.await.unwrap_or_default()).to_string();
    Ok(SandboxResult { exit_code: status.code(), signal: status.signal(), timed_out, stdout, stderr })
}

//! Spawn a process in a private staged view.
//!
//! The daemon is multithreaded, so it never runs setup code between `fork`
//! and `exec` (Rust only allows that through `unsafe` `pre_exec`). Instead
//! [`run`] starts the `txp-sandbox` helper, a fresh single-threaded process,
//! which re-executes itself once: the `unshare -pf` pattern in two stages.
//!
//! 1. **reaper**: moved into the step's cgroup by [`run`] before it is told
//!    anything, so nothing it starts can escape. It unshares the mount, PID,
//!    net, IPC and UTS namespaces and starts stage 2, which becomes PID 1 of
//!    the new PID namespace; it then waits and exits with that status.
//! 2. **init**: sets up the mounts, drops privileges, applies Landlock and
//!    seccomp, changes directory and `exec`s the step.
//!
//! Each stage reads its plan as JSON from a socket on its stdin and reports a
//! setup failure back over the same socket. A successful `exec` closes the
//! init's end with nothing written, so [`run`] can tell a setup error from
//! the step's own exit status.

use crate::cgroup::Cgroup;
use crate::landlock;
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::sys::prctl;
use nix::sys::signal::Signal;
use nix::sys::stat::{SFlag, fstat};
use nix::unistd::{Gid, Uid, setgid, setgroups, setuid};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// File name of the helper binary.
pub const HELPER_NAME: &str = "txp-sandbox";
/// Environment variable that names the helper binary explicitly.
pub const HELPER_ENV: &str = "TXP_SANDBOX_HELPER";
/// `argv[1]` that selects the init stage.
const INIT_ARG: &str = "--init";

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

/// Where [`run`] finds the helper: `$TXP_SANDBOX_HELPER` if set, otherwise
/// `txp-sandbox` next to the current executable. `None` if that file does
/// not exist.
pub fn helper_path() -> Option<PathBuf> {
    let p = match std::env::var_os(HELPER_ENV) {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe().ok()?.parent()?.join(HELPER_NAME),
    };
    p.is_file().then_some(p)
}

/// What the init stage needs; prepared by [`run`] and sent as JSON.
#[derive(Serialize, Deserialize)]
struct Plan {
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    /// overlayfs mount options and mount point, in mount order.
    overlays: Vec<(String, PathBuf)>,
    private_tmp: bool,
    uid: u32,
    gid: u32,
    landlock: bool,
    seccomp: bool,
}

/// The reaper's verdict on setup: `Ok` once the step has been `exec`ed.
type Reply = Result<(), String>;

/// Run to completion (or kill on timeout). The cgroup is left to the caller
/// to destroy so that an abort racing with the run can kill it.
pub async fn run(spec: SandboxSpec) -> io::Result<SandboxResult> {
    if !nix::unistd::geteuid().is_root() {
        return Err(io::Error::other("the process participant must run as root (no user-namespace mode yet)"));
    }
    // (A kernel without Landlock is refused by the init stage, fail closed.)
    if spec.argv.is_empty() {
        return Err(io::Error::other("empty argv"));
    }
    let helper = helper_path().ok_or_else(|| {
        io::Error::other(format!("sandbox helper {HELPER_NAME} not found next to the daemon (set {HELPER_ENV} to its path)"))
    })?;

    // Prepare everything the helper needs here, where errors are easy to report.
    let mut overlays = Vec::new();
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
            nix::unistd::chown(&m.upper, Some(Uid::from_raw(rm.uid())), Some(Gid::from_raw(rm.gid())))?;
            std::fs::set_permissions(&m.upper, std::fs::Permissions::from_mode(rm.mode() & 0o7777))?;
        }
        let lowers: Vec<String> = m.lowers.iter().map(|p| p.to_string_lossy().to_string()).collect();
        let opts = format!(
            "lowerdir={},upperdir={},workdir={},redirect_dir=off,index=off,metacopy=off",
            lowers.join(":"),
            m.upper.display(),
            m.work.display()
        );
        overlays.push((opts, m.at.clone()));
    }
    let plan = Plan {
        argv: spec.argv.clone(),
        env: spec.env.clone(),
        cwd: spec.cwd.clone(),
        overlays,
        private_tmp: spec.private_tmp && !spec.mounts.iter().any(|m| m.at.starts_with("/tmp")),
        uid: spec.uid,
        gid: spec.gid,
        landlock: spec.landlock,
        seccomp: spec.seccomp,
    };
    let plan = serde_json::to_vec(&plan).map_err(io::Error::other)?;

    let (ctl, helper_end) = UnixStream::pair()?;
    let mut cmd = tokio::process::Command::new(&helper);
    cmd.env_clear();
    // Under scripts/coverage.sh, let the helper record its own profile.
    #[cfg(coverage)]
    cmd.envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)));
    cmd.stdin(OwnedFd::from(helper_end)).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(false);
    let mut child = cmd.spawn().map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", helper.display())))?;
    // `cmd` still holds our copy of the helper's end of the socket; close it
    // so that end-of-file on `ctl` means the helper side has let go.
    drop(cmd);

    // The helper is blocked reading its plan, so it has started nothing yet:
    // once it is in the cgroup, so is everything it will start.
    if let Err(e) = spec.cgroup.add(child.id().expect("a child not yet waited for has a pid")) {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(e);
    }

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

    // The timeout covers setup as well as the step itself.
    let deadline = tokio::time::Instant::now() + spec.timeout;
    let (status, timed_out) = match tokio::time::timeout_at(deadline, handshake(ctl, &plan)).await {
        Ok(Ok(())) => tokio::select! {
            s = child.wait() => (s?, false),
            _ = tokio::time::sleep_until(deadline) => {
                let _ = spec.cgroup.kill();
                (child.wait().await?, true)
            }
        },
        Ok(Err(e)) => {
            let _ = spec.cgroup.kill();
            let _ = child.wait().await;
            let _ = spec.cgroup.wait_empty(Duration::from_secs(10)).await;
            return Err(e);
        }
        Err(_) => {
            let _ = spec.cgroup.kill();
            (child.wait().await?, true)
        }
    };
    // Make sure nothing lingers (e.g. the reaper), then let the caller tear
    // the cgroup down.
    let _ = spec.cgroup.kill();
    let _ = spec.cgroup.wait_empty(Duration::from_secs(10)).await;

    let stdout = String::from_utf8_lossy(&rd_out.await.unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&rd_err.await.unwrap_or_default()).to_string();
    Ok(SandboxResult { exit_code: status.code(), signal: status.signal(), timed_out, stdout, stderr })
}

/// Send the plan to the helper and wait for its verdict on setup.
async fn handshake(ctl: UnixStream, plan: &[u8]) -> io::Result<()> {
    let died = |e| io::Error::other(format!("sandbox helper exited during setup: {e}"));
    ctl.set_nonblocking(true)?;
    let mut ctl = tokio::net::UnixStream::from_std(ctl)?;
    ctl.write_all(plan).await.map_err(died)?;
    ctl.shutdown().await.map_err(died)?;
    let mut reply = Vec::new();
    ctl.read_to_end(&mut reply).await.map_err(died)?;
    match serde_json::from_slice::<Reply>(&reply) {
        Ok(r) => r.map_err(io::Error::other),
        Err(_) => Err(died(io::Error::other("no reply"))),
    }
}

/// Entry point of the `txp-sandbox` helper binary.
pub fn helper_main() -> ! {
    let stdin_is_socket = fstat(io::stdin().as_fd())
        .is_ok_and(|st| SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT == SFlag::S_IFSOCK);
    if !stdin_is_socket {
        eprintln!("{HELPER_NAME}: internal helper of txpd; it is not meant to be run by hand");
        std::process::exit(2);
    }
    let init = std::env::args_os().nth(1).is_some_and(|a| a == INIT_ARG);
    // The control socket is our stdin. Work on a close-on-exec duplicate so
    // that only the original descriptor is handed on.
    let ctl = UnixStream::from(io::stdin().as_fd().try_clone_to_owned().expect("duplicate the control socket"));
    std::process::exit(if init { init_stage(ctl) } else { reaper_stage(ctl) })
}

fn read_message(ctl: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    ctl.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Stage 1. Returns the helper's exit code.
fn reaper_stage(mut ctl: UnixStream) -> i32 {
    let started = start_init(&mut ctl);
    let reply: Reply = started.as_ref().map(|_| ()).map_err(String::clone);
    // If the daemon is gone, the parent-death signal or the cgroup kill ends us.
    if ctl.write_all(&serde_json::to_vec(&reply).unwrap_or_default()).is_err()
        && let Err(e) = &reply
    {
        eprintln!("{HELPER_NAME}: {e}");
    }
    let _ = ctl.shutdown(Shutdown::Both);
    let Ok(mut init) = started else { return 1 };
    // Wait for PID 1 of the new namespace and pass its status on.
    init.wait().map_or(127, |s| s.code().or_else(|| s.signal().map(|sig| 128 + sig)).unwrap_or(127))
}

/// Unshare the namespaces and start the init stage as their PID 1; returns
/// once it has `exec`ed the step.
fn start_init(ctl: &mut UnixStream) -> Result<std::process::Child, String> {
    let plan = read_message(ctl).map_err(|e| format!("reading plan: {e}"))?;
    // Die with the daemon.
    prctl::set_pdeathsig(Signal::SIGKILL).map_err(|e| format!("PR_SET_PDEATHSIG: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| format!("locating {HELPER_NAME}: {e}"))?;
    let flags = CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWNET
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWUTS;
    unshare(flags).map_err(|e| format!("unshare: {e}"))?;
    let (mut to_init, init_end) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
    // The `Command`, and with it our copy of `init_end`, is gone at the end
    // of this block: EOF on `to_init` then means init let go of it.
    let init = {
        let mut cmd = std::process::Command::new(exe);
        cmd.arg(INIT_ARG).env_clear().stdin(OwnedFd::from(init_end));
        #[cfg(coverage)]
        cmd.envs(std::env::var_os("LLVM_PROFILE_FILE").map(|v| ("LLVM_PROFILE_FILE", v)));
        cmd.spawn().map_err(|e| format!("starting init: {e}"))?
    };
    to_init.write_all(&plan).and_then(|()| to_init.shutdown(Shutdown::Write)).map_err(|e| format!("sending plan: {e}"))?;
    // Init writes nothing if it gets as far as `exec` (which closes the
    // socket), and the reason otherwise.
    let reply = read_message(&mut to_init).map_err(|e| format!("waiting for init: {e}"))?;
    if reply.is_empty() { Ok(init) } else { Err(String::from_utf8_lossy(&reply).into_owned()) }
}

/// Stage 2, PID 1 of the new namespaces. Returns only on failure.
fn init_stage(mut ctl: UnixStream) -> i32 {
    let Err(e) = read_message(&mut ctl).map_err(|e| format!("reading plan: {e}")).and_then(|p| confine_and_exec(&p));
    // The reaper relays this; if it is gone, so is everyone who would care.
    let _ = ctl.write_all(e.as_bytes());
    1
}

fn confine_and_exec(plan: &[u8]) -> Result<Infallible, String> {
    let p: Plan = serde_json::from_slice(plan).map_err(|e| format!("bad plan: {e}"))?;
    let argv0 = p.argv.first().ok_or("empty argv")?;
    let _ = prctl::set_pdeathsig(Signal::SIGKILL);
    // Mounts: stop propagation to the host, then /proc for the new PID
    // namespace, a private /tmp and the overlays.
    let none = None::<&str>;
    mount(none, "/", none, MsFlags::MS_REC | MsFlags::MS_PRIVATE, none).map_err(|e| format!("make / private: {e}"))?;
    let flags = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    mount(Some("proc"), "/proc", Some("proc"), flags, none).map_err(|e| format!("mount /proc: {e}"))?;
    if p.private_tmp {
        let flags = MsFlags::MS_NOSUID | MsFlags::MS_NODEV;
        mount(Some("tmpfs"), "/tmp", Some("tmpfs"), flags, Some("mode=1777")).map_err(|e| format!("mount /tmp: {e}"))?;
    }
    for (opts, at) in &p.overlays {
        mount(Some("overlay"), at.as_path(), Some("overlay"), MsFlags::empty(), Some(opts.as_str()))
            .map_err(|e| format!("mount overlay at {}: {e}", at.display()))?;
    }
    // Drop privileges.
    setgroups(&[]).map_err(|e| format!("setgroups: {e}"))?;
    setgid(Gid::from_raw(p.gid)).map_err(|e| format!("setgid {}: {e}", p.gid))?;
    setuid(Uid::from_raw(p.uid)).map_err(|e| format!("setuid {}: {e}", p.uid))?;
    // Landlock: read-only everywhere except the overlays, /tmp, /dev.
    if p.landlock {
        let full = landlock::full_rights();
        let mut rules = vec![
            landlock::Rule { path: Path::new("/"), allowed: landlock::read_only_rights() },
            landlock::Rule { path: Path::new("/tmp"), allowed: full },
            landlock::Rule { path: Path::new("/dev"), allowed: landlock::read_write_file_rights() },
        ];
        rules.extend(p.overlays.iter().map(|(_, at)| landlock::Rule { path: at, allowed: full }));
        landlock::restrict_self(&rules).map_err(|e| format!("Landlock: {e}"))?;
    } else {
        prctl::set_no_new_privs().map_err(|e| format!("PR_SET_NO_NEW_PRIVS: {e}"))?;
    }
    // seccomp syscall denylist: additive defence in depth. Runs after
    // no_new_privs is set and before exec; fails closed.
    if p.seccomp {
        crate::seccomp::Filter::compile().and_then(|f| f.install()).map_err(|e| format!("seccomp: {e}"))?;
    }
    // cwd inside the staged view (must come after the mounts).
    if let Some(c) = &p.cwd {
        std::env::set_current_dir(c).map_err(|e| format!("chdir {}: {e}", c.display()))?;
    }
    let e = std::process::Command::new(argv0)
        .args(&p.argv[1..])
        .env_clear()
        .envs(p.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .exec();
    Err(format!("exec {argv0}: {e}"))
}

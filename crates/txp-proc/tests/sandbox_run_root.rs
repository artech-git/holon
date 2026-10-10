//! Root-only tests of `sandbox::run` itself (run via `scripts/test-root.sh`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use txp_proc::cgroup::Cgroup;
use txp_proc::sandbox::{OverlayMount, SandboxSpec, run};

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// A process with no mounts and every optional confinement off.
fn bare(cg: &Cgroup, argv: &[&str]) -> SandboxSpec {
    SandboxSpec {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        cwd: None,
        mounts: vec![],
        uid: 65534,
        gid: 65534,
        timeout: Duration::from_secs(10),
        cgroup: cg.clone(),
        landlock: false,
        seccomp: false,
        private_tmp: false,
        output_limit: 1 << 16,
    }
}

fn cgroup(name: &str) -> Cgroup {
    Cgroup::create(&format!("txp-test-{name}-{}", std::process::id())).unwrap()
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("txp-run-").tempdir_in("/var/tmp").unwrap()
}

#[tokio::test]
async fn a_bare_process_still_gets_no_new_privs_and_its_output_is_capped() {
    if !is_root() {
        return;
    }
    let cg = cgroup("bare");
    let lots = "head -c 100000 /dev/zero | tr '\\0' e";
    let script = format!("grep NoNewPrivs /proc/self/status; {lots}; {lots} >&2");
    let mut spec = bare(&cg, &["/bin/sh", "-c", &script]);
    spec.output_limit = 1000;
    let r = run(spec).await.unwrap();
    cg.destroy().await.unwrap();
    assert!(r.success(), "{r:?}");
    assert!(r.stdout.starts_with("NoNewPrivs:\t1\n"), "{:?}", &r.stdout[..40]);
    assert_eq!((r.stdout.len(), r.stderr.len()), (1000, 1000), "both capped, the rest drained");
}

#[tokio::test]
async fn specs_that_cannot_work_are_refused_up_front() {
    if !is_root() {
        return;
    }
    let cg = cgroup("refused");
    let d = tmp();
    let mount = |lower: &str, upper: PathBuf| OverlayMount { lowers: vec![PathBuf::from(lower)], upper, work: d.path().join("w"), at: d.path().into() };
    for (m, why) in [
        (mount("/a:b", d.path().join("u")), "characters overlayfs cannot escape"),
        (mount("/a", PathBuf::from("relative")), "must be absolute"),
    ] {
        let mut spec = bare(&cg, &["/bin/true"]);
        spec.mounts = vec![m];
        assert!(run(spec).await.unwrap_err().to_string().contains(why), "{why}");
    }
    assert_eq!(run(bare(&cg, &[])).await.unwrap_err().to_string(), "empty argv");
    cg.destroy().await.unwrap();
}

#[tokio::test]
async fn a_cgroup_that_is_gone_stops_the_run() {
    if !is_root() {
        return;
    }
    let cg = cgroup("gone");
    cg.remove().unwrap();
    let e = run(bare(&cg, &["/bin/true"])).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
}

#[tokio::test]
async fn setup_failures_are_errors_and_setup_counts_against_the_timeout() {
    if !is_root() {
        return;
    }
    let cg = cgroup("setup");
    let mut spec = bare(&cg, &["/bin/true"]);
    spec.cwd = Some("/nonexistent".into());
    let e = run(spec).await.unwrap_err();
    assert!(e.to_string().starts_with("chdir /nonexistent: "), "{e}");
    let mut spec = bare(&cg, &["/bin/true"]);
    spec.timeout = Duration::ZERO;
    let r = run(spec).await.unwrap();
    assert!(r.timed_out && !r.success(), "{r:?}");
    cg.destroy().await.unwrap();
}

#[tokio::test]
async fn an_overlay_that_cannot_mount_is_an_error() {
    if !is_root() {
        return;
    }
    let cg = cgroup("overlay");
    let d = tmp();
    let at = d.path().join("at");
    std::fs::create_dir(&at).unwrap();
    let mut spec = bare(&cg, &["/bin/true"]);
    // The real root (last lower) exists; an earlier step's upper does not.
    spec.mounts = vec![OverlayMount { lowers: vec![d.path().join("missing"), at.clone()], upper: d.path().join("u"), work: d.path().join("w"), at }];
    let e = run(spec).await.unwrap_err();
    assert!(e.to_string().starts_with("mount overlay at "), "{e}");
    cg.destroy().await.unwrap();
}

/// What `run` says when the helper it finds is `helper` (`None`: unset).
fn run_with_helper(helper: Option<&str>) -> String {
    run_with_helper_within(helper, Duration::from_secs(10))
}

fn run_with_helper_within(helper: Option<&str>, timeout: Duration) -> String {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "helper_child", "--nocapture"]).env("TXP_HELPER_CHILD", timeout.as_millis().to_string());
    match helper {
        Some(h) => cmd.env("TXP_SANDBOX_HELPER", h),
        None => cmd.env_remove("TXP_SANDBOX_HELPER"),
    };
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_helper_must_be_present_and_genuine() {
    if !is_root() {
        return;
    }
    assert!(run_with_helper(None).contains("sandbox helper txp-sandbox not found"));
    // cat reads the plan and exits without a word.
    assert!(run_with_helper(Some("/bin/cat")).contains("sandbox helper exited during setup: no reply"));
    assert!(run_with_helper(Some(&Path::new("/nonexistent").display().to_string())).contains("not found"));
    // A file that cannot be executed.
    assert!(run_with_helper(Some("/etc/hostname")).contains("/etc/hostname: Permission denied"));
    // A helper that never answers: setup runs into the step's timeout.
    let d = tmp();
    let hang = d.path().join("hang");
    std::fs::write(&hang, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&hang, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let start = Instant::now();
    assert!(run_with_helper_within(Some(&hang.display().to_string()), Duration::from_millis(300)).contains("RESULT timed out"));
    assert!(start.elapsed() < Duration::from_secs(10));
}

/// Runs a sandbox within the given milliseconds and prints the outcome,
/// for `the_helper_must_be_present_and_genuine`.
#[tokio::test]
async fn helper_child() {
    let Ok(ms) = std::env::var("TXP_HELPER_CHILD") else { return };
    let cg = cgroup("helper");
    let mut spec = bare(&cg, &["/bin/true"]);
    spec.timeout = Duration::from_millis(ms.parse().unwrap());
    let r = run(spec).await;
    let _ = cg.destroy().await;
    match r {
        Ok(r) if r.timed_out => println!("RESULT timed out"),
        Ok(r) => println!("RESULT {r:?}"),
        Err(e) => println!("RESULT {e}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_killed_from_outside_reports_128_plus_the_signal() {
    if !is_root() {
        return;
    }
    let cg = cgroup("signal");
    let procs = cg.procs_path();
    let task = tokio::spawn(run(bare(&cg, &["/bin/sleep", "100"])));
    // The step is PID 1 of its namespace; from the host it is the cgroup
    // member running `sleep`, and SIGKILL from outside always lands.
    let start = Instant::now();
    let pid = loop {
        let pids = std::fs::read_to_string(&procs).unwrap_or_default();
        if let Some(p) = pids.lines().find(|p| std::fs::read_to_string(format!("/proc/{p}/comm")).is_ok_and(|c| c.trim() == "sleep")) {
            break p.parse::<i32>().unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(10), "step never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL).unwrap();
    let r = task.await.unwrap().unwrap();
    cg.destroy().await.unwrap();
    assert_eq!((r.exit_code, r.timed_out), (Some(137), false), "{r:?}");
}

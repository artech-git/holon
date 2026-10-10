//! The `txp-sandbox` helper's own protocol, driven directly the way the
//! daemon drives it. Root-only parts run via `scripts/test-root.sh`.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

const HELPER: &str = env!("CARGO_BIN_EXE_txp-sandbox");

#[test]
fn refuses_to_run_by_hand() {
    let out = Command::new(HELPER).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not meant to be run by hand"));
}

/// Start the helper on one end of a socket pair; the other end is ours.
fn start() -> (UnixStream, std::process::Child) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    // The temporary Command (holding a copy of `theirs`) is dropped here.
    let child = Command::new(HELPER).stdin(OwnedFd::from(theirs)).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    (ours, child)
}

#[test]
fn a_bad_plan_is_reported_to_the_daemon_or_else_to_stderr() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let (mut ours, mut child) = start();
    ours.write_all(b"not a plan").unwrap();
    ours.shutdown(Shutdown::Write).unwrap();
    let mut reply = String::new();
    ours.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("{\"Err\":\"bad plan: "), "{reply}");
    assert_eq!(child.wait().unwrap().code(), Some(1));

    // Nobody is left to read the reply: it goes to stderr instead.
    let (mut ours, child) = start();
    ours.write_all(b"not a plan").unwrap();
    ours.shutdown(Shutdown::Both).unwrap();
    drop(ours);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("txp-sandbox: bad plan: "), "{}", String::from_utf8_lossy(&out.stderr));
}

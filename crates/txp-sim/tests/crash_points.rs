//! Runs the harness binary over every crash point (fs participants only;
//! the process variant needs root: `sudo txp-crashtest run --with-process`).

#[test]
fn every_crash_point_is_atomic() {
    let exe = env!("CARGO_BIN_EXE_txp-crashtest");
    let base = tempfile::Builder::new().prefix("txp-crash-").tempdir_in("/var/tmp").unwrap();
    let out = std::process::Command::new(exe)
        .args(["run", "--base"])
        .arg(base.path())
        .args(["--iterations", "2"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("{stdout}");
    assert!(out.status.success(), "harness failed:\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
}

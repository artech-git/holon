//! End-to-end tests of the `txp` client: daemon commands against a fake
//! daemon that answers canned replies, and the offline commands for real.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TXP: &str = env!("CARGO_BIN_EXE_txp");

/// A daemon stand-in that answers every request line with `reply(request)`.
fn fake_daemon(reply: fn(&Value) -> String) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let sock = d.path().join("txpd.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let mut conn = conn.unwrap();
            let mut line = String::new();
            BufReader::new(&conn).read_line(&mut line).unwrap();
            let resp = reply(&serde_json::from_str(&line).unwrap());
            let _ = writeln!(conn, "{resp}");
        }
    });
    (d, sock)
}

fn txp(sock: &Path, args: &[&str]) -> Output {
    Command::new(TXP).env("TXP_SOCKET", sock).args(args).output().unwrap()
}

fn stdout(o: &Output) -> Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn stderr(o: &Output) -> String {
    assert!(!o.status.success());
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Echo the request back as the result.
fn echo(req: &Value) -> String {
    json!({"ok": "true", "result": req}).to_string()
}

#[test]
fn each_daemon_command_sends_its_request() {
    let (_d, sock) = fake_daemon(echo);
    for (cmd, op) in [
        ("list", "list"),
        ("in-doubt", "in_doubt"),
        ("orphans", "orphans"),
        ("locks", "locks"),
        ("wal-stats", "wal_stats"),
        ("checkpoint", "checkpoint"),
        ("self-test", "self_test"),
        ("recovery", "recovery"),
        ("shutdown", "shutdown"),
    ] {
        assert_eq!(stdout(&txp(&sock, &[cmd]))["op"], op, "{cmd}");
    }
    let r = stdout(&txp(&sock, &["status", "  ab  "]));
    assert_eq!((r["op"].as_str(), r["txid"].as_str()), (Some("status"), Some(&*format!("{:032x}", 0xab))));
    assert!(stderr(&txp(&sock, &["status", "zz"])).contains("bad txid"));
}

const MANIFEST: &str = "[txn]\nname = \"m\"\n[[resource]]\nid = \"r\"\nkind = \"fs.tree\"\npath = \"/srv/r\"\n[[step]]\nid = \"s\"\nkind = \"fs.delete\"\nresource = \"r\"\npath = \"x\"\n";

fn write_manifest(dir: &Path, name: &str, text: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p.display().to_string()
}

#[test]
fn run_exits_non_zero_unless_committed() {
    // Committed when the manifest names it so; aborted otherwise.
    fn decide(req: &Value) -> String {
        let outcome = if req["manifest"].as_str().unwrap().contains("name = \"m\"") { "committed" } else { "aborted" };
        let result = if req["wait"] == true { json!({"txid": "1", "outcome": {"outcome": outcome}}) } else { json!({"txid": "1"}) };
        json!({"ok": "true", "result": result}).to_string()
    }
    let (d, sock) = fake_daemon(decide);
    let good = write_manifest(d.path(), "good.toml", MANIFEST);
    let other = write_manifest(d.path(), "other.toml", &MANIFEST.replace("name = \"m\"", "name = \"n\""));
    assert_eq!(stdout(&txp(&sock, &["run", &good]))["outcome"]["outcome"], "committed");
    assert_eq!(txp(&sock, &["run", &other]).status.code(), Some(1));
    assert_eq!(stdout(&txp(&sock, &["run", "--no-wait", &other]))["txid"], "1");
    // An invalid manifest never reaches the daemon.
    let bad = write_manifest(d.path(), "bad.toml", "[txn");
    assert!(stderr(&txp(&sock, &["run", &bad])).contains("parse"));
}

#[test]
fn daemon_errors_and_unreachable_daemons_are_reported() {
    let (_d, sock) = fake_daemon(|_| json!({"ok": "false", "error": "nope"}).to_string());
    assert!(stderr(&txp(&sock, &["list"])).contains("nope"));
    let (_d2, garbage) = fake_daemon(|_| "not json".to_string());
    assert!(stderr(&txp(&garbage, &["list"])).contains("decode response"));
    assert!(stderr(&txp(Path::new("/nonexistent.sock"), &["list"])).contains("connect /nonexistent.sock"));
}

#[tokio::test]
async fn offline_commands_need_no_daemon() {
    let d = tempfile::tempdir().unwrap();
    let nowhere = Path::new("/nonexistent.sock");
    let root = d.path().join("site");
    std::fs::create_dir(&root).unwrap();
    let put = MANIFEST.replace("/srv/r", &root.display().to_string()).replace("kind = \"fs.delete\"", "kind = \"fs.put\"\ncontent = \"hi\"");
    let good = write_manifest(d.path(), "put.toml", &put);
    let v = stdout(&txp(nowhere, &["validate", &good]));
    assert_eq!((v["name"].as_str(), v["steps"].clone(), v["timeout_secs"].as_u64()), (Some("m"), json!(["s"]), Some(600)));
    assert!(stderr(&txp(nowhere, &["validate", "/nonexistent.toml"])).contains("No such file"));

    let data = d.path().join("data").display().to_string();
    assert_eq!(stdout(&txp(nowhere, &["embedded", &data, &good]))["outcome"]["outcome"], "committed");
    assert_eq!(std::fs::read_to_string(root.join("x")).unwrap(), "hi");
    let failing = write_manifest(d.path(), "fail.toml", &put.replace("content = \"hi\"", "source = \"/nonexistent\""));
    assert_eq!(txp(nowhere, &["embedded", &data, &failing]).status.code(), Some(1));

    // A log with a transaction left open: wal-dump lists what recovery would do.
    let open_txn = txp_core::LogRecord::Begin { txid: txp_core::TxId(9), name: "left".into(), manifest_digest: "d".into(), submitter: None, participants: vec![] };
    let wal_dir = d.path().join("other/wal");
    let (wal, _) = txp_wal::Wal::open(std::sync::Arc::new(txp_wal::RealDisk), &wal_dir, txp_wal::WalConfig::default()).unwrap();
    wal.append(open_txn).await.unwrap();
    wal.shutdown();
    let out = txp(nowhere, &["wal-dump", &d.path().join("other").display().to_string()]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"kind\":\"begin\""));
    assert!(String::from_utf8_lossy(&out.stderr).contains("recovery_actions=1\n  AbortStarted"), "{}", String::from_utf8_lossy(&out.stderr));
}

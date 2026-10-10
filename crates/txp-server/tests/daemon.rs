//! End-to-end tests of `txpd`: start the binary on a temporary socket and
//! speak its newline-delimited JSON protocol.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const TXPD: &str = env!("CARGO_BIN_EXE_txpd");

struct Daemon {
    child: Child,
    socket: PathBuf,
    root: PathBuf,
    data: PathBuf,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// Start txpd with `args`; its socket comes from `TXP_SOCKET`.
    fn start(args: &[&str], env: &[(&str, &str)]) -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let (socket, root, data) = (dir.path().join("run/txpd.sock"), dir.path().join("site"), dir.path().join("data"));
        std::fs::create_dir(&root).unwrap();
        let child = Command::new(TXPD)
            .arg("--data-dir")
            .arg(&data)
            .args(args)
            .env("TXP_SOCKET", &socket)
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !socket.exists() {
            assert!(start.elapsed() < Duration::from_secs(10), "txpd did not come up");
            std::thread::sleep(Duration::from_millis(20));
        }
        Daemon { child, socket, root, data, _dir: dir }
    }

    fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.socket).unwrap()
    }

    /// Send raw `bytes`, read one reply line ("" at end of stream).
    fn raw(&self, bytes: &[u8]) -> String {
        let mut s = self.connect();
        s.write_all(bytes).unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        line
    }

    fn request(&self, req: Value) -> Value {
        serde_json::from_str(&self.raw(format!("{req}\n").as_bytes())).unwrap()
    }

    fn manifest(&self, content: &str) -> String {
        format!(
            "[txn]\nname = \"e2e\"\n[[resource]]\nid = \"site\"\nkind = \"fs.tree\"\npath = \"{}\"\n[[step]]\nid = \"put\"\nkind = \"fs.put\"\nresource = \"site\"\npath = \"index.html\"\ncontent = \"{content}\"\n",
            self.root.display()
        )
    }

    fn wait(self) -> (Output, PathBuf) {
        (self.child.wait_with_output().unwrap(), self.socket)
    }
}

fn ok(v: &Value) -> &Value {
    assert_eq!(v["ok"], "true", "{v}");
    &v["result"]
}

fn err(v: &Value) -> &str {
    assert_eq!(v["ok"], "false", "{v}");
    v["error"].as_str().unwrap()
}

#[test]
fn serves_every_request_kind_then_shuts_down_on_request() {
    let d = Daemon::start(&[], &[("TXP_SANDBOX_HELPER", "/nonexistent")]);
    let r = d.request(json!({"op": "run", "manifest": d.manifest("hello"), "wait": true}));
    assert_eq!(ok(&r)["outcome"]["outcome"], "committed");
    assert_eq!(std::fs::read_to_string(d.root.join("index.html")).unwrap(), "hello");
    let txid = ok(&d.request(json!({"op": "run", "manifest": d.manifest("again")})))["txid"].as_str().unwrap().to_string();
    let start = Instant::now();
    while ok(&d.request(json!({"op": "status", "txid": txid})))["phase"] != "done" {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(err(&d.request(json!({"op": "status", "txid": "00ff"}))).starts_with("unknown transaction"));
    assert!(err(&d.request(json!({"op": "run", "manifest": "[txn"}))).starts_with("manifest: parse"));
    assert_eq!(ok(&d.request(json!({"op": "list"}))).as_array().unwrap().len(), 2);
    for op in ["in_doubt", "orphans"] {
        assert_eq!(ok(&d.request(json!({"op": op}))), &json!([]), "{op}");
    }
    assert_eq!(ok(&d.request(json!({"op": "locks"})))["held"], json!([]));
    assert_eq!(ok(&d.request(json!({"op": "wal_stats"})))["fsyncs"], 0, "two one-phase commits");
    assert!(ok(&d.request(json!({"op": "checkpoint"})))["snapshot_lsn"].as_u64().unwrap() > 0);
    assert_eq!(ok(&d.request(json!({"op": "self_test"})))["sandbox_helper"], Value::Null);
    assert_eq!(ok(&d.request(json!({"op": "recovery"})))["in_doubt"], json!([]));

    // Blank lines are skipped and bad requests answered, on one connection.
    let mut s = d.connect();
    s.write_all(b"\n{\"op\": \"nope\"}\n{\"op\": \"list\"}\n").unwrap();
    let mut lines = BufReader::new(&s).lines();
    let bad: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert!(err(&bad).starts_with("bad request: "));
    let list: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert!(ok(&list).is_array());
    // Bytes that are not UTF-8 end the connection.
    assert_eq!(d.raw(b"\xff\xfe\n"), "");

    // A checkpoint that cannot be written is reported, not fatal.
    use std::os::unix::fs::PermissionsExt;
    let wal = d.data.join("wal");
    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o555)).unwrap();
    let r = d.request(json!({"op": "checkpoint"}));
    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o755)).unwrap();
    if !nix::unistd::geteuid().is_root() {
        assert!(err(&r).starts_with("log: io: "), "{r}");
    }

    assert_eq!(ok(&d.request(json!({"op": "shutdown"}))), "shutting down");
    let (out, socket) = d.wait();
    assert!(out.status.success());
    assert!(!socket.exists(), "socket removed on the way out");
    // (tracing logs to stdout)
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(log.contains("txp-sandbox not found next to txpd"), "{log}");
}

#[test]
fn strangers_may_look_but_not_touch() {
    if nix::unistd::geteuid().is_root() {
        return; // root may always do everything
    }
    // The daemon's owner is whoever ran sudo; here that is someone else.
    let d = Daemon::start(&[], &[("SUDO_UID", "54321"), ("SUDO_GID", "54321")]);
    assert!(err(&d.request(json!({"op": "run", "manifest": d.manifest("x")}))).starts_with("unauthorized: uid"));
    assert!(err(&d.request(json!({"op": "checkpoint"}))).contains("may not checkpoint"));
    assert!(err(&d.request(json!({"op": "shutdown"}))).contains("may not shut down"));
    assert!(ok(&d.request(json!({"op": "list"}))).is_array(), "introspection stays open");
    // Ctrl-C stops it cleanly too.
    Command::new("kill").args(["-INT", &d.child.id().to_string()]).status().unwrap();
    let (out, socket) = d.wait();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!socket.exists());
}

fn txpd(args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    Command::new(TXPD).arg("--data-dir").arg(dir.path().join("data")).args(args).output().unwrap()
}

#[test]
fn startup_options_and_failures() {
    let out = txpd(&["--recover-only", "--allow-anyone", "--allow-uid", "5", "--socket", "/nonexistent/s"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--allow-anyone: any peer"));
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("s").display().to_string();
    for (args, want) in [
        (vec!["--socket", sock.as_str(), "--socket-mode", "zz"], "socket mode"),
        (vec!["--socket", "/"], "bind /"),
    ] {
        let out = txpd(&args);
        assert!(!out.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains(want), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    // A data directory that cannot be created.
    let file = dir.path().join("file");
    std::fs::write(&file, "").unwrap();
    let out = Command::new(TXPD).arg("--data-dir").arg(&file).arg("--recover-only").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("open engine"));
}

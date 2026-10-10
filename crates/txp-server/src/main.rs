//! `txpd`: the transactional process daemon.
//!
//! Startup runs the host self-test, opens the engine (which replays the
//! decision log and re-drives unfinished transactions), then listens on a
//! Unix socket for newline-delimited JSON requests (`txp_server::Request`).
//! Each connection is served on its own task; a `run` request with `wait`
//! blocks only that connection. `Shutdown` requests and Ctrl-C stop
//! admission, drain the log and remove the socket.
//!
//! Configuration: `--data-dir` / `TXP_DATA_DIR`, `--socket` / `TXP_SOCKET`,
//! `--socket-mode`, `--allow-uid` / `--allow-anyone` (submission policy), and
//! `--recover-only` to replay and exit.
//!
//! Submitting a manifest runs code, so it is authenticated by the connecting
//! peer's credentials (`SO_PEERCRED`): only root, the daemon owner and any
//! `--allow-uid` may submit, and a non-root submitter's process steps are
//! pinned to that submitter's own uid/gid. Read-only introspection is open to
//! any peer that can open the socket.

#![warn(missing_docs)]

use anyhow::Context;
use clap::Parser;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use txp_engine::{Engine, EngineConfig, Submitter};
use txp_server::{default_socket, Request, Response};

/// txpd — transactional process daemon (single node).
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Data directory (decision log, participant journals).
    #[arg(long, env = "TXP_DATA_DIR", default_value = "/var/lib/txp")]
    data_dir: PathBuf,
    /// Unix socket path.
    #[arg(long, env = "TXP_SOCKET", default_value_os_t = default_socket())]
    socket: PathBuf,
    /// Socket mode (octal). Peer-credential authorization still applies, so a
    /// wider mode only widens who may reach read-only introspection.
    #[arg(long, default_value = "0660")]
    socket_mode: String,
    /// Extra peer uids allowed to submit transactions (repeatable). Root and
    /// the daemon owner are always allowed.
    #[arg(long = "allow-uid")]
    allow_uid: Vec<u32>,
    /// Allow any peer that can open the socket to submit (reproduces the
    /// pre-authorization behaviour; insecure).
    #[arg(long)]
    allow_anyone: bool,
    /// Only run recovery and the self-test, then exit.
    #[arg(long)]
    recover_only: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?))
        .with_target(false)
        .init();
    let args = Args::parse();

    let caps = txp_proc::host_capabilities();
    tracing::info!(?caps, "host self-test");
    for w in caps.warnings() {
        tracing::warn!("{w}");
    }

    let mut ecfg = EngineConfig::new(&args.data_dir);
    ecfg.auth.allow_uids.extend(args.allow_uid.iter().copied());
    ecfg.auth.allow_anyone = args.allow_anyone;
    if args.allow_anyone {
        tracing::warn!("--allow-anyone: any peer that can open the socket may submit transactions");
    }
    tracing::info!(owner_uid = ecfg.auth.owner_uid, allow_uids = ?ecfg.auth.allow_uids, allow_anyone = ecfg.auth.allow_anyone, "submission policy");
    let (engine, report) = Engine::open(ecfg).await.context("open engine")?;
    tracing::info!(?report, "recovery complete");
    if args.recover_only {
        engine.shutdown().await;
        return Ok(());
    }

    let _ = std::fs::remove_file(&args.socket);
    if let Some(p) = args.socket.parent() {
        std::fs::create_dir_all(p)?;
    }
    let listener = UnixListener::bind(&args.socket).with_context(|| format!("bind {}", args.socket.display()))?;
    let mode = u32::from_str_radix(args.socket_mode.trim_start_matches("0o"), 8).context("socket mode")?;
    std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(mode))?;
    tracing::info!(socket = %args.socket.display(), data_dir = %args.data_dir.display(), "txpd listening");

    let engine = Arc::new(engine);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let socket_path = args.socket.clone();
    loop {
        tokio::select! {
            r = listener.accept() => {
                let (stream, _) = r?;
                let engine = engine.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(stream, engine, shutdown_tx).await {
                        tracing::debug!(%e, "connection ended");
                    }
                });
            }
            _ = shutdown_rx.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    tracing::info!("shutting down: stopping admission, draining");
    engine.shutdown().await;
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

async fn serve(stream: tokio::net::UnixStream, engine: Arc<Engine>, shutdown: tokio::sync::mpsc::Sender<()>) -> anyhow::Result<()> {
    // Authenticate the peer before reading anything it sends. (A connected
    // Unix socket always has peer credentials; without them, hang up.)
    let c = stream.peer_cred().context("peer credentials")?;
    let who = Submitter { uid: c.uid(), gid: c.gid(), pid: c.pid() };
    tracing::debug!(uid = who.uid, gid = who.gid, pid = ?who.pid, "client connected");
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                write(&mut w, Response::err(format!("bad request: {e}"))).await?;
                continue;
            }
        };
        let resp = handle(req, &engine, &shutdown, &who).await;
        write(&mut w, resp).await?;
    }
    Ok(())
}

async fn write(w: &mut tokio::net::unix::OwnedWriteHalf, resp: Response) -> anyhow::Result<()> {
    let mut s = serde_json::to_string(&resp)?;
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    Ok(())
}

async fn handle(req: Request, engine: &Engine, shutdown: &tokio::sync::mpsc::Sender<()>, who: &Submitter) -> Response {
    match req {
        Request::Run { manifest, wait } => match engine.submit(&manifest, Some(*who)) {
            Err(e) => Response::err(e),
            Ok((txid, rx)) => {
                if wait {
                    let out = rx.await.ok();
                    Response::ok(serde_json::json!({ "txid": txid, "outcome": out, "status": engine.status(txid) }))
                } else {
                    Response::ok(serde_json::json!({ "txid": txid }))
                }
            }
        },
        Request::Status { txid } => match engine.status(txid) {
            Some(s) => Response::ok(s),
            None => Response::err(format!("unknown transaction {txid}")),
        },
        Request::List => Response::ok(engine.list()),
        Request::InDoubt => Response::ok(engine.in_doubt()),
        Request::Orphans => Response::ok(engine.orphan_journals()),
        Request::Locks => Response::ok(serde_json::json!({ "held": engine.locks().held(), "wait_for": engine.locks().wait_for_graph() })),
        Request::WalStats => {
            let s = engine.wal().stats();
            Response::ok(serde_json::json!({ "batches": s.batches, "records": s.records, "fsyncs": s.fsyncs, "max_batch": s.max_batch, "batch_hist": s.batch_hist }))
        }
        Request::Checkpoint => {
            if !engine.authorized(who.uid) {
                return Response::err(format!("unauthorized: uid {} may not checkpoint", who.uid));
            }
            match engine.checkpoint().await {
                Ok(lsn) => Response::ok(serde_json::json!({ "snapshot_lsn": lsn })),
                Err(e) => Response::err(e),
            }
        }
        Request::SelfTest => Response::ok(txp_proc::host_capabilities()),
        Request::Recovery => Response::ok(engine.recovery_report()),
        Request::Shutdown => {
            if !engine.authorized(who.uid) {
                return Response::err(format!("unauthorized: uid {} may not shut down the daemon", who.uid));
            }
            let _ = shutdown.try_send(());
            Response::ok("shutting down")
        }
    }
}

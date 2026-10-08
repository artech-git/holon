//! `txp`: command-line client for the transactional process daemon.
//!
//! Most subcommands send one JSON request over the daemon's Unix socket
//! (see `txp_server::Request`) and pretty-print the JSON reply. Three run
//! without a daemon: `validate` parses a manifest locally, `wal-dump`
//! replays a decision log directory offline, and `embedded` runs a
//! manifest against an in-process engine.
//!
//! The socket path comes from `--socket` or `TXP_SOCKET`. `run` and
//! `embedded` exit non-zero when the transaction did not commit.

#![warn(missing_docs)]

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use txp_core::TxId;
use txp_server::{default_socket, Request, Response};

/// txp — client for the transactional process daemon.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(long, env = "TXP_SOCKET", default_value_os_t = default_socket(), global = true)]
    socket: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Validate a manifest locally without submitting it.
    Validate { file: PathBuf },
    /// Submit a manifest and (by default) wait for the outcome.
    Run {
        file: PathBuf,
        #[arg(long)]
        no_wait: bool,
    },
    Status { txid: String },
    List,
    /// Decided-but-unfinished transactions.
    InDoubt,
    /// Participant journals the log no longer explains.
    Orphans,
    Locks,
    WalStats,
    Checkpoint,
    SelfTest,
    Recovery,
    Shutdown,
    /// Dump a decision log directory offline (no daemon needed).
    WalDump { data_dir: PathBuf },
    /// Run a manifest in-process with an embedded engine (no daemon; fs
    /// steps only unless root). Useful for scripts and tests.
    Embedded { data_dir: PathBuf, file: PathBuf },
}

async fn call(socket: &PathBuf, req: Request) -> anyhow::Result<serde_json::Value> {
    let stream = tokio::net::UnixStream::connect(socket).await.with_context(|| format!("connect {}", socket.display()))?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut reader = BufReader::new(r);
    let mut resp = String::new();
    reader.read_line(&mut resp).await?;
    match serde_json::from_str::<Response>(&resp).context("decode response")? {
        Response::Ok { result } => Ok(result),
        Response::Err { error } => anyhow::bail!("{error}"),
    }
}

fn print(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let s = &args.socket;
    match args.cmd {
        Cmd::Validate { file } => {
            let text = std::fs::read_to_string(&file)?;
            let m = txp_manifest::Manifest::parse(&text)?;
            let order: Vec<_> = m.ordered_steps()?.iter().map(|s| s.id.clone()).collect();
            print(&serde_json::json!({ "name": m.txn.name, "timeout_secs": m.timeout()?.as_secs(), "resources": m.resources.len(), "steps": order, "digest": txp_manifest::Manifest::digest(&text) }));
        }
        Cmd::Run { file, no_wait } => {
            let manifest = std::fs::read_to_string(&file)?;
            txp_manifest::Manifest::parse(&manifest)?;
            let v = call(s, Request::Run { manifest, wait: !no_wait }).await?;
            print(&v);
            if let Some(o) = v.get("outcome").and_then(|o| o.get("outcome")).and_then(|o| o.as_str())
                && o != "committed" {
                    std::process::exit(1);
                }
        }
        Cmd::Status { txid } => {
            let txid = TxId::parse(&txid).context("bad txid")?;
            print(&call(s, Request::Status { txid }).await?)
        }
        Cmd::List => print(&call(s, Request::List).await?),
        Cmd::InDoubt => print(&call(s, Request::InDoubt).await?),
        Cmd::Orphans => print(&call(s, Request::Orphans).await?),
        Cmd::Locks => print(&call(s, Request::Locks).await?),
        Cmd::WalStats => print(&call(s, Request::WalStats).await?),
        Cmd::Checkpoint => print(&call(s, Request::Checkpoint).await?),
        Cmd::SelfTest => print(&call(s, Request::SelfTest).await?),
        Cmd::Recovery => print(&call(s, Request::Recovery).await?),
        Cmd::Shutdown => print(&call(s, Request::Shutdown).await?),
        Cmd::WalDump { data_dir } => {
            let rec = txp_wal::recovery::recover(&txp_wal::RealDisk, &data_dir.join("wal"))?;
            for (lsn, r) in &rec.records {
                println!("{lsn:>8}  {}", serde_json::to_string(r)?);
            }
            eprintln!("next_lsn={} segments={} recovery_actions={}", rec.next_lsn, rec.segments.len(), rec.table.recovery_actions().len());
            for a in rec.table.recovery_actions() {
                eprintln!("  {a:?}");
            }
        }
        Cmd::Embedded { data_dir, file } => {
            let text = std::fs::read_to_string(&file)?;
            let (engine, report) = txp_engine::Engine::open(txp_engine::EngineConfig::new(&data_dir)).await?;
            eprintln!("recovery: {}", serde_json::to_string(&report)?);
            let (txid, out) = engine.run(&text).await?;
            print(&serde_json::json!({ "txid": txid, "outcome": out, "status": engine.status(txid) }));
            engine.shutdown().await;
            if out != txp_engine::TxOutcome::Committed {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

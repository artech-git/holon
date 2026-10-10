//! txp-crashtest: for every crash point, run a transaction in a child
//! process that `_exit`s at that point, then recover and check that the
//! participants' observable state is all-or-nothing and consistent with the
//! durable decision.

#![warn(missing_docs)]

use clap::{Parser, Subcommand};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Command;
use txp_core::TxPhase;
use txp_engine::crash::POINTS;
use txp_engine::{Engine, EngineConfig, TxOutcome};
use txp_sim::Scenario;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Driver: iterate over crash points.
    Run {
        #[arg(long, default_value = "/var/tmp/txp-crashtest")]
        base: PathBuf,
        #[arg(long, default_value_t = 3)]
        iterations: usize,
        #[arg(long)]
        with_process: bool,
    },
    /// Child: run one transaction (crash point via TXP_CRASH_AT).
    Child { base: PathBuf, #[arg(long)] with_process: bool },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Args::parse().cmd {
        Cmd::Child { base, with_process } => {
            let sc = Scenario { base: base.clone(), roots: vec![base.join("root0"), base.join("root1")], data_dir: base.join("data") };
            let (engine, _) = Engine::open(EngineConfig::new(&sc.data_dir)).await?;
            let (_txid, out) = engine.run(&sc.manifest(with_process)).await?;
            engine.shutdown().await;
            println!("{}", serde_json::to_string(&out)?);
            Ok(())
        }
        Cmd::Run { base, iterations, with_process } => {
            let exe = std::env::current_exe()?;
            let mut failures = 0;
            let mut summary = Vec::new();
            for iter in 0..iterations {
                for point in POINTS.iter().chain(std::iter::once(&"none")) {
                    let _ = std::fs::remove_dir_all(&base);
                    std::fs::create_dir_all(&base)?;
                    let sc = Scenario::new(&base, 2);
                    let mut cmd = Command::new(&exe);
                    cmd.arg("child").arg(&base);
                    if with_process {
                        cmd.arg("--with-process");
                    }
                    if *point != "none" {
                        cmd.env("TXP_CRASH_AT", point);
                    }
                    let out = cmd.output()?;
                    let crashed = out.status.signal() == Some(9);
                    let child_out = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    // Recover.
                    let (engine, report) = Engine::open(EngineConfig::new(&sc.data_dir)).await?;
                    let table = engine.wal().table();
                    let undone: Vec<_> = table.lock().entries().filter(|e| e.phase != TxPhase::Done).map(|e| e.txid).collect();
                    let committed = table.lock().entries().any(|e| !e.commit_set.is_empty());
                    engine.shutdown().await;
                    let visible = sc.check(with_process);
                    let leftovers = sc.leftovers();
                    let ok = match visible {
                        None => false,
                        Some(v) => {
                            // Visible effects must match the durable decision.
                            // (1PC is not used here: two participants.)
                            if committed { v } else { !v }
                        }
                    } && undone.is_empty()
                        && leftovers.is_empty();
                    if !ok {
                        failures += 1;
                    }
                    let line = format!(
                        "iter={iter} point={point:<22} crashed={crashed:<5} committed={committed:<5} visible={visible:?} undone={} leftovers={} recovery={} child={child_out} => {}",
                        undone.len(),
                        leftovers.len(),
                        serde_json::to_string(&report)?,
                        if ok { "OK" } else { "VIOLATION" }
                    );
                    println!("{line}");
                    summary.push(line);
                }
            }
            let _ = TxOutcome::Committed;
            if failures > 0 {
                anyhow::bail!("{failures} atomicity violations");
            }
            println!("all crash points consistent");
            Ok(())
        }
    }
}

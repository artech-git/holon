//! Crash points for kill -9 style testing. `TXP_CRASH_AT=<name>` makes the
//! process send itself `SIGKILL` when it reaches that point, so it dies
//! without running destructors or flushing anything. Recovery then has to
//! clean up.

/// Every recognised crash point, in protocol order. The crash-test harness
/// iterates over these.
pub const POINTS: &[&str] = &[
    "after_begin",
    "after_stage",
    "after_prepare",
    "before_commit_record",
    "after_commit_record",
    "mid_commit_fanout",
    "before_done",
    "after_done",
    "before_abort_record",
    "mid_abort_fanout",
];

/// Kill the process with `SIGKILL` if `TXP_CRASH_AT` names `point`.
pub fn maybe(point: &str) {
    if let Ok(v) = std::env::var("TXP_CRASH_AT")
        && v == point {
            eprintln!("txp: simulated crash at {point}");
            let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGKILL);
            // Not reached: SIGKILL cannot be caught or ignored.
            std::process::abort();
        }
}

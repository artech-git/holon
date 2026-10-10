#!/usr/bin/env bash
# Line coverage of the whole workspace, root-only suites included. Exits
# non-zero if any line of any `src/` file never ran.
#
# Needs cargo-llvm-cov (`cargo install cargo-llvm-cov`, `rustup component add
# llvm-tools`) and passwordless sudo for scripts/test-root.sh.
#
# Profiles are collected in LLVM's continuous mode (`%c` + runtime counter
# relocation): counters live in an mmap'd file, so processes that abort, are
# SIGKILLed or exec another program (the WAL's fatal paths, crash points, the
# sandbox helper) still record what they ran. `%m` merges every process of a
# binary into one pool file, so the sandbox's init stage (always PID 1 in its
# namespace) does not overwrite itself.
#
# The gate counts each source line once, covered if any test ran it (as
# `llvm-cov show`, lcov and Codecov do). The summary table also reports
# regions and functions; its "Lines" column counts a line once per function
# spanning it, so an error-mapping closure that never ran (`.map_err(|e| ...)`
# for a syscall that cannot fail here) shows there as a missed line although
# the line itself ran. Two more blind spots: `#[async_trait]` method bodies
# are only mapped through the closures inside them, and `tests/`, `examples/`
# are not measured at all.
#
#   scripts/coverage.sh                     # summary table + gate
#   scripts/coverage.sh --html              # + target/llvm-cov/html
set -euo pipefail
cd "$(dirname "$0")/.."
eval "$(cargo llvm-cov show-env --sh)"
export RUSTFLAGS="${RUSTFLAGS:-} -Cllvm-args=-runtime-counter-relocation"
export LLVM_PROFILE_FILE="$CARGO_LLVM_COV_TARGET_DIR/holon-%p-%12m%c.profraw"
cargo llvm-cov clean --workspace
cargo test --workspace --quiet
scripts/test-root.sh --quiet
cargo llvm-cov report "$@"
lcov="$CARGO_LLVM_COV_TARGET_DIR/llvm-cov/lcov.info"
mkdir -p "$(dirname "$lcov")"
cargo llvm-cov report --lcov --output-path "$lcov"
awk -F'[:,]' '
    /^SF:/ { file = $2 }
    /^DA:/ { line = file ":" $2; if (!(line in seen)) { seen[line]; total++ }
             if ($3 > 0 && !(line in hit)) { hit[line]; covered++ } }
    END {
        for (line in seen) if (!(line in hit)) print "never ran: " line
        printf "line coverage: %d/%d (%.2f%%)\n", covered, total, total ? 100 * covered / total : 100
        exit covered < total
    }' "$lcov"

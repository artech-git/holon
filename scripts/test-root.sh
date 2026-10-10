#!/usr/bin/env bash
# Run the root-only test suites (every `tests/*_root.rs` in the workspace):
# build as the normal user, run each test binary as root (namespaces,
# overlayfs, cgroups and mounts need CAP_SYS_ADMIN). Never run cargo itself
# as root: it would leave root-owned files in the target directory.
#
# The sandbox suites reach the sandbox through the txp-sandbox helper, which
# cargo builds alongside them; TXP_SANDBOX_HELPER points the tests at it.
# LLVM_PROFILE_FILE is passed through so scripts/coverage.sh can use this.
set -euo pipefail
cd "$(dirname "$0")/.."
mapfile -t ARTIFACTS < <(cargo test --workspace --no-run --message-format=json 2>/dev/null | python3 -c '
import json,sys
helper = None
for line in sys.stdin:
    try: m=json.loads(line)
    except Exception: continue
    if m.get("reason")!="compiler-artifact" or not m.get("executable"): continue
    t = m["target"]
    if t["kind"]==["test"] and t["name"].endswith("_root"): print("test", m["executable"])
    # The bin target is also built as a unit-test harness; skip that one.
    if t["name"]=="txp-sandbox" and t["kind"]==["bin"] and not m["profile"]["test"]: helper = m["executable"]
print("helper", helper)')
HELPER=""; TESTS=()
for a in "${ARTIFACTS[@]}"; do
    case $a in helper\ *) HELPER=${a#helper } ;; test\ *) TESTS+=("${a#test }") ;; esac
done
ENV=(TXP_SANDBOX_HELPER="$HELPER")
[[ -n ${LLVM_PROFILE_FILE:-} ]] && ENV+=(LLVM_PROFILE_FILE="$LLVM_PROFILE_FILE")
for bin in "${TESTS[@]}"; do
    echo "running $bin as root"
    sudo env "${ENV[@]}" "$bin" --test-threads=1 "$@"
done

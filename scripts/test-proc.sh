#!/usr/bin/env bash
# Build the txp-proc tests as the normal user, run the binary as root
# (the sandbox needs CAP_SYS_ADMIN for namespaces, overlayfs and cgroups).
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=$(cargo test -p txp-proc --no-run --message-format=json 2>/dev/null | python3 -c '
import json,sys
for line in sys.stdin:
    try: m=json.loads(line)
    except Exception: continue
    if m.get("reason")=="compiler-artifact" and m.get("executable") and m["target"]["name"]=="sandbox_root":
        print(m["executable"])')
echo "running $BIN as root"
sudo "$BIN" --test-threads=1 "$@"

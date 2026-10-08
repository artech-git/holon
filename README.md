# txp — transactional processes (MVP)

All-or-nothing execution of filesystem changes and sandboxed OS processes,
coordinated by a Tokio daemon whose only commit point is one fsynced
decision record in its own write-ahead log.

```text
 txp run manifest.toml ──▶ txpd ──▶ lock ▶ Begin ▶ stage steps ▶ prepare ▶ [Commit record, fdatasync] ▶ publish ▶ Done
                                     │                                │
                                     │      fs participant: same-filesystem staging, rename/RENAME_EXCHANGE redo list
                                     │      proc participant: mount+pid+net+ipc+uts namespaces, overlayfs upper per step,
                                     │                        cgroup v2 `cgroup.kill`, Landlock read-only outside the overlay
                                     └── presumed abort: no record ⇒ aborted; abort/done records are lazy
```

This repository implements **Phase 1 plus the crash-testing subset of Phase 2**
of the design document (`compass_artifact_…md`). See
[`docs/STATUS.md`](docs/STATUS.md) for the exact coverage and deviations.

## What is guaranteed

| Participant | Abort | Commit | Visible before commit |
|---|---|---|---|
| `fs.tree` root (files, dir swaps, deletes) | true rollback (staging discarded) | crash-atomic via idempotent redo; single-file and single-swap publishes are instantaneous | never |
| sandboxed process (fs-only effects) | `cgroup.kill` + discard upper | same as files | never (private mount ns, no network interfaces, Landlock, seccomp denylist) |

Every crash point of the coordinator (`kill -9` equivalents between every
protocol step) has been checked by `txp-crashtest`; the protocol itself is
model-checked in TLA+ (`spec/`).

## Layout

```
crates/txp-core        pure state machines: ids, LogRecord, TxnTable + recovery rules, typestate, DurableCommit token
crates/txp-wal         segmented WAL, CRC32C framing, group-commit writer thread, torn-tail recovery, checkpoints, SimDisk
crates/txp-lock        strict 2PL over hierarchical keys, conservative ordering, wait-for graph
crates/txp-participant Participant trait, Capabilities, PartError, local journal, registry
crates/txp-fs          fs participant: staging, RedoOp publish (rename / RENAME_EXCHANGE, inode-based idempotency)
crates/txp-proc        proc participant: namespaces, overlayfs, cgroup v2, Landlock, upperdir → RedoOp translation
crates/txp-manifest    TOML manifests, validation, topological step order
crates/txp-engine      coordinator: per-txn tasks, 2PC presumed abort, 1PC, read-only, recovery, crash points
crates/txp-server      txpd (JSON over Unix socket)
crates/txp-cli         txp
crates/txp-sim         txp-crashtest harness + atomicity checker
spec/                  TwoPhasePA.tla + cfg, TLC runner
examples/              manifests
scripts/               test-proc.sh (root tests), tlc.sh
```

## Running

The sandbox requires Linux (cgroup v2, overlayfs, Landlock, seccomp), so
everything is built and run on a Linux host or VM.

```sh
cargo test --workspace            # unit, property, WAL crash-sim, engine, crash-point harness
./scripts/test-proc.sh            # sandbox tests (builds as you, runs the binary with sudo)
./scripts/tlc.sh                  # TLA+ model check (needs ~/tla/tla2tools.jar + a JRE)

# daemon (root: namespaces, overlayfs and cgroups need CAP_SYS_ADMIN)
# Submission is authorized by peer credentials: root and the daemon owner
# (the sudo invoker) may submit out of the box; add --allow-uid <uid> for
# other users, or --allow-anyone to disable the check.
sudo env TXP_DATA_DIR=/var/lib/txp TXP_SOCKET=/run/txpd.sock \
     target/debug/txpd --socket-mode 0666 &
export TXP_SOCKET=/run/txpd.sock
txp self-test
txp validate examples/rebuild-and-publish.toml
txp run examples/rebuild-and-publish.toml      # exit 0 iff committed
txp run examples/failing-build.toml            # aborts; target untouched
txp list | txp status <txid> | txp in-doubt | txp orphans | txp locks | txp wal-stats
txp checkpoint; txp shutdown
txp wal-dump /var/lib/txp                      # offline log inspection
sudo target/debug/txp-crashtest run --with-process   # crash every point with a sandboxed step
```

A root submitter's process steps run as `SUDO_UID:SUDO_GID` (the user who
invoked sudo) unless the step sets `user = "uid:gid"`. A non-root submitter is
pinned to its own uid/gid: that is the default, and a `user` requesting any
other identity is rejected. The managed root must be writable by the effective
uid, exactly as it would be outside the sandbox.

## Manifest

```toml
[txn]
name = "rebuild-and-publish"
timeout = "10m"

[[resource]]
id = "site"; kind = "fs.tree"; path = "/srv/site"; mode = "write"

[[step]]
id = "build"; kind = "process"
argv = ["make", "site"]; cwd = "/srv/site"
mounts = [{ resource = "site" }]        # staged overlay appears at the real path
network = "deny"                        # the only option today
timeout = "5m"

[[step]]
id = "stamp"; kind = "fs.put"; resource = "site"; after = ["build"]
path = "DEPLOY"; content = "$txid\n"
```

Step kinds: `process`, `fs.put` (`content` or `source`), `fs.delete`,
`fs.replace_tree` (`source` directory swapped in with `RENAME_EXCHANGE`).
A later process step mounting the same resource sees the earlier steps'
staged output (stacked overlay lowers). `$txid` is substituted in `content`,
`argv` and `env`; processes also get `TXP_TXID` and `TXP_STEP`.

## Protocol in one paragraph

Locks are taken up front in canonical order (deadlock-free). `Begin` is
written unforced. Steps stage into private state. If exactly one participant
staged anything it is committed in one phase (its local journal record is the
commit point; zero coordinator fsyncs). Otherwise all staged participants
`prepare` concurrently (fsync their staging + journal), the coordinator
writes the forced `Commit` record through the group-commit writer, obtains a
`DurableCommit` token (the only way into phase two), fans out `commit` with
unbounded retries, writes `Done`, and releases locks. Any failure before the
decision aborts: fan out `abort`, then the lazy `Abort`/`Done` records.
Recovery replays the log: `Commit` without `Done` re-drives commit, anything
else is presumed aborted. `abort` is total and reports a local 1PC commit,
which a recovering coordinator adopts with an audited `ForceResolve` record.

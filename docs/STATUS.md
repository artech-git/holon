# Implementation status vs. the design document

Verified on: Ubuntu 25.04, Linux 6.14 (cgroup v2, overlayfs,
Landlock ABI 6), Rust nightly 1.101 (2026-10-04). Date: 2026-10-06.

## Done (MVP = Phase 1 + crash-testing subset of Phase 2)

| Design item | Status | Where |
|---|---|---|
| Invariants I1–I6 encoded in types / state machine | ✅ `TxnTable::apply` rejects abort-after-commit; `DurableCommit` is the only way into phase two | txp-core |
| TLA+ model: presumed abort, coordinator crash, participant crash, duplicated messages, 1PC, read-only vote | ✅ TLC: 3 RMs, all actions covered, invariants + I2 action property hold; negative test reproduces the I2 counterexample | spec/ |
| Custom segmented WAL, CRC32C framing, torn-tail truncation vs. corruption refusal, preallocation + fdatasync, fsync-error ⇒ abort process | ✅ | txp-wal |
| Group commit (one write, one fdatasync per batch), adaptive batching, no timers | ✅ batch histogram exposed via `txp wal-stats` | txp-wal |
| Checkpoint (atomic snapshot) + segment truncation | ✅ | txp-wal |
| Simulated disk with lost unsynced sectors / torn writes / EIO | ✅ (WAL-level; engine-level DST is Phase 2) | txp-wal::sim |
| Typestate driver + persisted phase enum; recovery rules | ✅ | txp-core, txp-engine |
| Participant trait: total idempotent commit/abort, local journal, recover() | ✅ | txp-participant |
| Tokio cancellation safety: protocol in its own task, log writes on a dedicated thread, retries after decision unbounded | ✅ | txp-engine |
| fs participant: same-fs staging, rename / `RENAME_EXCHANGE`, redo list persisted at prepare, idempotent replay (inode check) | ✅ | txp-fs |
| proc participant: mount/pid/net/ipc/uts namespaces, overlayfs upper per step with stacked lowers, cgroup v2 `cgroup.kill`, timeout, Landlock fail-closed, unprivileged uid, upperdir → publish translation (files, dirs, opaque dirs, whiteouts, symlinks; rejects hard links/devices/fifos/redirect/metacopy) | ✅ | txp-proc |
| 1PC fast path, read-only vote, pipelined prepares | ✅ 1PC verified to issue zero coordinator fsyncs | txp-engine |
| Strict 2PL at coordinator, hierarchical keys, conservative canonical-order acquisition, wait-for graph API | ✅ | txp-lock |
| Manifest format, validation, topological order | ✅ TOML | txp-manifest |
| Daemon + CLI; status, list, in-doubt, orphans, locks, wal stats, checkpoint, self-test, offline wal dump | ✅ | txp-server, txp-cli |
| Submission authorization (`SO_PEERCRED` + allow policy), per-submitter uid pinning, submitter recorded in `Begin`, seccomp syscall denylist | ✅ root test asserts the step runs under seccomp filter mode | txp-server, txp-engine, txp-core, txp-proc |
| Phase-1 exit criteria: crash at every protocol point never yields a partial publish; recovery re-drives Commit-without-Done; 1PC issues exactly one forced write (the participant's); group commit batches >1 under load | ✅ `txp-crashtest`, `wal_tests::group_commit_batches_under_concurrency`, engine tests | txp-sim |
| 100% line coverage of every `src/` file (3990/3990), root-only suites included: fault-injected participants (`engine_faults.rs`), every WAL `fatal()` abort path in a child process, txpd/txp end to end, sandbox helper protocol | ✅ `scripts/coverage.sh` (cargo-llvm-cov in continuous mode, so aborting, SIGKILLed and exec'ing processes record too) | all |

## Deviations from the document (and why)

- **API is newline-JSON over a Unix socket, not gRPC/tonic.** The VM has no
  `protoc`; the protocol surface is tiny and versionable. gRPC arrives with the
  remote participant protocol in Phase 4.
- **The daemon must run as root for process steps.** Ubuntu's AppArmor
  (`kernel.apparmor_restrict_unprivileged_userns=1`) blocks unprivileged user
  namespaces in the VM. The design allows a privileged v1. fs-only manifests
  work unprivileged.
- **Egress is "deny" only** (network namespace without interfaces). The
  deferred-egress proxy / outbox is Phase 3.
- **seccomp denylist applied** (txp-proc `seccomp.rs`), additive to namespaces
  + Landlock (fs) + unprivileged uid + `no_new_privs`: a `seccompiler`-built
  filter refuses a fixed set of administrative / exploit-primitive syscalls
  with `EPERM` (a foreign syscall ABI kills the process), installed fail-closed
  in the confined init process. A strict allowlist is not attempted (it would
  break ordinary build tools).
- **No `unsafe` code** (`unsafe_code = "forbid"` workspace-wide). Syscalls go
  through `nix`; Landlock, seccomp and xattrs through the `landlock`,
  `seccompiler` and `xattr` crates. Because running code between `fork` and
  `exec` needs `unsafe` `pre_exec`, the daemon instead starts the
  **`txp-sandbox` helper binary** (txp-proc), which unshares the namespaces and
  re-executes itself as PID 1 to confine and `exec` the step. It must be
  installed next to `txpd` (or named by `TXP_SANDBOX_HELPER`); `txpd` warns at
  startup and `self-test` reports `sandbox_helper` when it is missing.
- **Crash testing uses deterministic crash points (`TXP_CRASH_AT`) and a
  simulated disk at the WAL layer**, not yet whole-engine deterministic
  simulation (turmoil/madsim) or a dm-flakey VM harness.
- **1PC safety addition:** `Participant::abort` returns `AbortOutcome` so a
  recovering coordinator adopts a participant's local 1PC commit instead of
  reporting a presumed abort (found while writing the TLA+ model).
- **Directory attribute fidelity:** `Mkdir` redo ops carry uid/gid/mode of
  the staged directory (found during the end-to-end run).

## Not done (per the plan's own deferral list)

Postgres (`PREPARE TRANSACTION`), queue outbox and deferred HTTP effects,
TCC/compensation classes, interactive transactions with wound-wait, Btrfs
backend, Raft (openraft) and remote agents, STAGING/parallel commits,
io_uring, Prometheus/OTLP export, trace validation of TLC traces against the
Rust state machine, loom/shuttle/stateright, VM dm-flakey harness, orphan
journal garbage collection (listed by `txp orphans`, not auto-resolved).

## Known rough edges

- `cgroup.events` is polled (5 ms) while waiting for a killed cgroup to
  empty; an inotify watch is the planned upgrade.
- Overlay copy-up of large files is a full copy (inherent to overlayfs).
- Multi-path publishes are crash-atomic but not instantaneous for
  unmanaged readers; use a single swappable directory for that.
- The socket mode defaults to `0660`. Submission is now authorized by peer
  credentials (`SO_PEERCRED`), so a wider mode only widens who may reach
  read-only introspection; submitting requires root, the daemon owner or an
  `--allow-uid`, and a non-root submitter's process steps are pinned to its own
  uid/gid. `--allow-anyone` restores the old open behaviour.

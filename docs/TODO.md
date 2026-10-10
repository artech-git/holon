# Production-readiness TODO

Gaps between the current Phase-1 MVP and a daemon that can run under real load
and untrusted local clients. Feature-roadmap deferrals (Raft, Postgres,
egress proxy, seccomp, …) live in [STATUS.md](STATUS.md); this list is the
operational / security / release-engineering work those deferrals don't cover.

Tiers are rough priority: 0 blocks any production use; 5 is depth behind the
correctness claims. Check items off as they land.

## Tier 0 — Security & multi-tenancy (hard blockers) — DONE

- [x] Peer authentication on the socket: `serve()` captures `SO_PEERCRED`
      (`UnixStream::peer_cred`) and `AuthPolicy` gates submission to root, the
      daemon owner and any `--allow-uid` (`--allow-anyone` restores the old
      open behaviour). Read-only introspection stays open; `Run`, `Checkpoint`
      and `Shutdown` are gated. (txp-engine `AuthPolicy`, txp-server/src/main.rs)
- [x] Pin the uid a submitter's process steps run as: a non-root submitter is
      pinned to its own uid/gid (`RunAsPolicy`); a `user = "0:0"` from a
      non-root/non-owner peer is rejected. Root (or in-process) submitters keep
      free choice. (txp-engine/src/plan.rs)
- [x] Audit trail: `LogRecord::Begin` now carries an `Option<Submitter>`
      (uid/gid/pid), `#[serde(default)]` so old logs still replay.
      (txp-core/src/record.rs)
- [x] seccomp-bpf denylist for sandboxed steps: a `seccompiler` filter
      (`libc::SYS_*`, portable across x86_64/aarch64/riscv64) installed in the
      confined init process after `no_new_privs`, fail-closed. Verified active on the
      step process (mode 2) by a root test. (txp-proc/src/seccomp.rs)

Follow-ups deferred out of Tier 0: supplementary-group checks for a pinned
submitter (only the primary gid is honoured today); a connection-level reject
for unauthorized peers (currently only privileged ops are gated); making the
seccomp list configurable.

## Tier 1 — Availability & durability architecture

- [ ] Document the single-node ceiling explicitly: RPO/RTO, single-disk
      assumption, "node down ⇒ in-doubt txns stuck until same host replays".
- [ ] Wire-protocol version field on every request + negotiation
      (txp-server/src/lib.rs has none today).
- [ ] On-disk format version headers on WAL + checkpoint + journals, and an
      upgrade/migration path. Add now, before any deployed format exists.

## Tier 2 — Resource safety / DoS

- [ ] Cap request/manifest size — `BufReader::lines()` reads unbounded lines
      and can OOM the daemon (txp-server/src/main.rs:104).
- [ ] Admission control: semaphore on in-flight transactions, connection-count
      limit, idle-connection timeout. (Every connection is an unbounded
      `tokio::spawn` today.)
- [ ] Per-step cgroup v2 resource limits: `memory.max`, `pids.max`, `cpu.max`.
      `cgroup.kill` handles teardown but no caps are set (txp-proc).
- [ ] Staging-space quota + pre-flight disk-free check (overlay copy-up is a
      full copy; a large build can fill the data disk).
- [ ] Automatic orphan-journal GC — currently listed by `txp orphans` but never
      resolved, so journals leak on disk.

## Tier 3 — Operability & observability

- [ ] Metrics export (Prometheus/OTLP): commit latency, in-doubt count, fsync
      rate, batch sizes, staging usage. `wal-stats` is CLI-pull-only today.
- [ ] Health/readiness endpoint (systemd notify / k8s probe / LB check):
      "up and past recovery?".
- [ ] Structured (JSON) log option + txid-correlated spans across a txn
      lifecycle; configurable log level/format.
- [ ] Bounded graceful shutdown: drain deadline + forced-stop path so a wedged
      txn can't hang shutdown (txp-server/src/main.rs:97).
- [ ] Packaging/deploy: systemd unit, container image, config file (not just
      flags/env).

## Tier 4 — Release engineering / supply chain

- [ ] CI: run `cargo test`, `clippy`, the crash harness, and the TLA+ check on
      every change (no `.github/workflows` exists yet).
- [ ] Pin toolchain (`rust-toolchain.toml`), set MSRV, add `clippy.toml`.
- [ ] `cargo-audit` / `cargo-deny` gate + SBOM for the root daemon.
- [ ] CHANGELOG + semver discipline, tied to the format-versioning above.

## Tier 5 — Verification depth (behind the atomicity claims)

- [ ] Whole-engine deterministic simulation (madsim/turmoil) beyond WAL-layer
      sim disk + deterministic crash points.
- [ ] `loom`/`shuttle` for engine + lock-manager concurrency.
- [ ] Real-disk fault injection (dm-flakey VM harness).
- [ ] Fuzz the three untrusted parsers: WAL framing/recovery, TOML manifest,
      JSON wire protocol.
- [ ] Trace-validate TLC traces against the Rust state machine (close the gap
      between "spec is correct" and "code matches spec").

## Scope limits that may gate a given use case

- [ ] Egress is deny-only — most real builds fetch dependencies
      (deferred-egress proxy / outbox is Phase 3).
- [ ] Only same-filesystem files + processes are transactional; no external
      participant (e.g. Postgres) means no cross-resource DB+fs atomicity.

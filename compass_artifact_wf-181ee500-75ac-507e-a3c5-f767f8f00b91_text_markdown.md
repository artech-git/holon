# Transactional Processes in Rust: A Phased Implementation Plan (Single-Machine → Raft-Replicated)

Build it as a Tokio daemon whose only commit point is one fsynced decision record in a custom write-ahead log, running presumed-abort two-phase commit across a generic `Participant` trait. OS processes become participants by running sandboxed against a private, staged filesystem view with network egress denied or deferred: commit means atomically publishing the staged artifacts, abort means discarding them. In Phase 4 the same log becomes the Raft log (openraft), so the coordinator is no longer a single point of blocking.

## TL;DR
- **What's achievable:** true all-or-nothing for files and directories under managed roots, for sandboxed OS processes whose side effects are confined to staged filesystem state, and for Postgres (`PREPARE TRANSACTION`). Queues and outbound RPCs can only be **deferred** until after commit, via an outbox with idempotency keys. Irreversible external effects must be denied, deferred, or explicitly marked as compensation-only. Generic kernel-level rollback (TxOS-style) is not realistic: TxOS needed a modified 2.6.22 kernel and Microsoft deprecated TxF.
- **Protocol:** Phase 1 uses 2PC with presumed abort, plus one-phase-commit and read-only fast paths and group-committed decision records in a local WAL. Phase 4 uses the same 2PC, with the coordinator's decision log replicated by Raft (Spanner-style, the practical form of Gray–Lamport Paxos Commit). A parallel-commits-style STAGING optimization comes later. Reject 3PC and Calvin as primary protocols.
- **How to make it correct:** write a TLA+ model first (start from the TwoPhase/PaxosCommit specs in tlaplus/Examples). Put every I/O source (disk, network, clock, RNG, process spawning) behind traits so the coordinator runs under deterministic simulation with fsync-failure and torn-write injection from day one. Use loom/shuttle for concurrency primitives and an atomicity checker over the observable state of every participant.

## Key Findings

1. **Kernel-level transactional execution is a dead end for a userspace project; staging plus atomic publish is the realistic path.** TxOS showed that system transactions work: the TxOS project describes it as supporting "transactional semantics for 150 system calls" on a modified Linux 2.6.22.6. But Porter & Witchel's HotOS 2009 paper reports that it adds "roughly 8,600 lines of code to the kernel and require[s] about 14,000 lines of minor changes to kernel code". That is not something you can ship. Microsoft's Transactional NTFS documentation states: "Many scenarios that TxF was developed for can be achieved through simpler and more readily available techniques. Furthermore, TxF may not be available in future versions of Microsoft Windows." The lesson: don't try to make arbitrary syscalls transactional. Constrain where effects can land (a private overlay or staging directory, no network) and make commit a small set of atomic publish operations.

2. **Speculator is the right mental model for external effects.** Speculator (Nightingale, Chen & Flinn, SOSP 2005) guarantees correctness by "preventing speculative processes from externalizing output, for example, sending a network message or writing to the screen, until the speculations on which that output depends have proven to be correct." Your daemon should apply exactly that rule to egress: buffer it or deny it until the decision record is durable.

3. **The decision log is the whole product, so own it.** fsyncgate showed that after a failed `fsync`, Linux (since 4.13's errseq_t) marks the dirty pages clean and reports the error only once, so a retried fsync can falsely succeed.\[1\] PostgreSQL's fix, shipped in the February 14, 2019 minor releases (11.2 and backports), was to "panic instead of retrying after fsync() failure" by default (via the new `data_sync_retry` setting, default off) and recover from the WAL. A coordinator must treat any fsync/fdatasync error on the decision log as fatal. That argues for a small custom WAL with explicit failure semantics instead of a general-purpose KV store.

4. **Existing coordinators have weak isolation by design.** Seata's own docs state that with local databases at Read Committed or above, "the default isolation level of the global transaction is read uncommitted"; read committed requires `SELECT FOR UPDATE`.\[2\]\[3\] This project can do better by running a coordinator-level lock manager over declared resources.

5. **Rust ecosystem status (verified October 2026):** openraft is active (0.10.0-alpha.36 on Sep 29, 2026; 0.9.25 is a bug-fix release that fixes "one safety defect, one state-divergence defect"), but it is still pre-1.0.\[4\]\[5\] tikv/raft-rs last published 0.7.0 on 2023-03-07.\[6\]\[7\] redb is "Stable and maintained" (4.1.0, April 2026).\[8\]\[9\] fjall 3.0 shipped January 2, 2026.\[10\] sled's latest stable is still 0.34.7 (2021), and its 1.0 alpha README says "sled is beta".\[11\]\[12\] okaywal warns "Please do not use in any production projects".\[13\]\[14\]

## Architecture Overview

```
                ┌─────────────────────────── txpd (Tokio daemon) ───────────────────────────┐
 CLI / gRPC ───▶│ API (tonic) ─▶ Admission/backpressure ─▶ Txn Manager (per-txn actor tasks) │
 manifests      │                                   │                                       │
                │        ┌──────────────────────────┼──────────────────────────┐            │
                │        ▼                          ▼                          ▼            │
                │   Lock Manager             Decision Log (WAL)        Participant Registry  │
                │  (2PL, wound-wait,         group commit, fdatasync,   (trait objects per    │
                │   wait-for graph)          checkpoint/truncate        resource kind)        │
                │                                   │                          │            │
                │                                   │        ┌────────┬────────┼────────┐   │
                │                                   │        ▼        ▼        ▼        ▼   │
                │                                   │     FsPart   ProcPart  PgPart  Outbox  │
                │                                   │   (staging) (sandbox) (2PC)  (queues, │
                │                                   │                            RPC, HTTP) │
                │                         Recovery on startup: replay log → re-drive        │
                └───────────────────────────────────────────────────────────────────────────┘
 Phase 4: Decision Log ⇒ openraft RaftLogStorage over the same segment WAL; 3 or 5 replicas;
          remote participants speak a versioned gRPC participant protocol.
```

Core invariants. Encode these in TLA+ first, then in the Rust types.

- **I1 (single commit point):** a transaction is committed iff a `Commit{txid}` record is durable in the decision log, which in Phase 4 means Raft-committed.
- **I2 (log-before-act):** no participant receives `commit` before I1 holds. No participant is told to `abort` after a `Commit` record exists.
- **I3 (durable promise):** a participant that voted `Prepared` can commit or abort its staged state after a crash, and cannot unilaterally abort once prepared.
- **I4 (presumed abort):** no record for a txid means it is aborted. Abort records are written lazily, and only to garbage-collect participant state.
- **I5 (invisibility):** staged state is unobservable to non-transactional readers before commit (within the stated isolation scope).
- **I6 (idempotent completion):** `commit(txid)` and `abort(txid)` may be delivered any number of times, in any order relative to restarts.

## Phase 0: Specification, Formal Model, Threat Model (2–4 weeks)

**Goals:** pin down semantics before writing code. Every later phase is judged against this spec.

**Deliverables**
- `spec/` directory with a TLA+ model of presumed-abort 2PC, extended with: coordinator crash/restart reading the last log record; participant crash after prepare; duplicate and reordered messages; the 1PC fast path; the read-only vote. Start from the TwoPhase, TCommit and PaxosCommit modules in the tlaplus/Examples repo (from Gray & Lamport's paper).\[15\] Check that your extension refines `TCommit`, the way the paper proves Paxos Commit refines 2PC.\[16\]
- A second model for the Phase 4 STAGING optimization, written now so you know whether it is worth it.
- A semantics document defining "transaction", "participant", "visible", the isolation level per resource class, and a taxonomy of effects: *stageable*, *deferrable*, *compensatable*, *forbidden*.
- A manifest format draft (see Phase 1).

**Key decisions**
- **Presumed abort vs presumed commit:** choose presumed abort. With presumed abort, the coordinator doesn't need to force-write a "begin/collecting" record before prepare, and aborts need no forced writes. Presumed commit has to force a collecting record up front to avoid ambiguity. Since many of your transactions will abort (processes fail, users call them off), presumed abort fits the workload.
- **Model checker:** TLC for the protocol spec. Use stateright later (Phase 2) to model-check the actual Rust state machine code (stateright 0.31.0, July 2025, no explicit maintenance statement in its README).\[17\]

**Exit criteria:** TLC passes with at least 3 participants, coordinator crash, and message duplication, at a meaningful state depth. Known counterexamples (e.g., abort-after-commit when I2 is removed) are reproduced as negative tests.

**Risk:** spec drift from the code. Mitigate by generating test traces from TLC and replaying them against the Rust state machine (trace validation).

## Phase 1: Single-Machine Core (8–12 weeks)

### 1.1 Goals
WAL, coordinator state machine, participant trait, filesystem and OS-process participants, daemon with gRPC and CLI, crash recovery.

### 1.2 Commit protocol for this phase
Use **2PC with presumed abort, 1PC, and read-only optimizations**, with participants in-process.

- **1PC:** if exactly one participant is not read-only, skip the prepare round. Ask that participant to commit directly, then write a `Done` record lazily. The participant's own commit record serves as the commit point. This is the same idea as CockroachDB's "one-phase commit fast-path", which skips the transaction record when all writes are on one range.\[18\]
- **Read-only vote:** a participant that staged nothing votes `ReadOnly` and drops out of phase 2.
- **Group commit:** a single "log writer" task drains an MPSC queue of pending records, writes them with one `pwritev`, issues one `fdatasync`, then resolves each waiter's oneshot. Batching is adaptive: flush when the queue empties or a byte or latency budget is hit. No timers on the hot path.

### 1.3 The decision log (custom WAL)

**Recommendation:** write a custom segmented WAL (around 2–3k lines) for the decision log. Do not build it on sled (still beta) or okaywal (explicitly not for production).\[12\]\[13\] Use redb ("Stable and maintained"; stable file format) only for the *secondary* catalog: transaction metadata, manifests, participant registry, and the outbox index.\[9\] These are rewritten from the log on recovery anyway. Considered and rejected:
- **fjall 3.x:** a good LSM with optional serializable transactions, but it brings compaction I/O and its own durability policy into your commit path.\[19\]
- **raft-engine:** built for Multi-Raft logs in TiKV. Its last crates.io release is 0.4.2 (2024-04-26) and master depends on raft from git, so it is a heavy, TiKV-shaped dependency.\[20\]\[21\]
- **RocksDB bindings:** a C++ dependency and no control over error semantics.

**Record framing**
```
segment file: [SegmentHeader{magic, version, segment_id, base_lsn, created_epoch}] then records:
record: [len: u32][crc32c: u32 over (lsn..payload)][lsn: u64][kind: u8][txid: u128][payload...]
padding to 4 KiB optional per batch (see torn writes)
```
- **Checksums:** CRC32C per record, plus a per-batch trailer `{batch_first_lsn, batch_len, crc}`. On recovery, scan forward. Stop at the first bad length or CRC *in the last segment only*, and truncate there; that is a torn tail. A bad CRC in a non-tail segment is media corruption: refuse to start and require operator action.
- **Torn writes:** assume sector atomicity at most, never page atomicity. Never rewrite a durable region in place. For the segment header, use an A/B double-write or an immutable header.
- **fsync semantics on Linux:**
  - Preallocate segments with `fallocate` and zero them, so steady-state appends change no metadata and `fdatasync` suffices.
  - On segment creation, `fsync` the file *and* the parent directory.
  - Treat any `EIO` from `fdatasync` as fatal: crash the process and recover from what is on disk. Never retry.
  - Optionally open with `O_DIRECT`, which fsyncgate's postmortem points to as the long-term fix, to avoid trusting page-cache bytes after errors.\[22\]\[23\] Start buffered + fdatasync for simplicity, and keep O_DIRECT behind a flag.
- **Checkpoint and truncation:** periodically write a snapshot of the in-memory transaction table: active txns, their state, participant lists. Write it to `snap.tmp` (or an `O_TMPFILE` + `linkat`), fsync, rename, and fsync the directory. Then delete segments whose max LSN is below `min(snapshot_lsn, oldest-in-flight txn first LSN)`. Done transactions under presumed abort leave nothing behind.

**Log record kinds**
```rust
pub enum LogRecord {
    Begin    { txid: TxId, manifest_digest: [u8; 32], participants: Vec<ParticipantId> }, // non-forced
    Prepared { txid: TxId, votes: Vec<(ParticipantId, Vote)> },  // optional, non-forced (debug/ops)
    Commit   { txid: TxId, participants: Vec<ParticipantId> },   // FORCED: the commit point
    Abort    { txid: TxId },                                     // non-forced (presumed abort)
    Done     { txid: TxId },                                     // non-forced; enables truncation
    Outbox   { txid: TxId, effect_id: EffectId, payload_ref: BlobRef }, // part of commit batch
}
```

### 1.4 Coordinator state machine (typestate plus a persisted enum)

Use **two representations**. A serializable enum is what recovery reads. Typestate wrappers are what the live driver uses, so illegal transitions don't compile.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TxPhase { Started, Preparing, Committing, Aborting, Done }

pub struct Txn<S> { id: TxId, plan: Arc<Plan>, parts: Vec<PartHandle>, _s: PhantomData<S> }
pub struct Started; pub struct Preparing; pub struct Committing; pub struct Aborting;

impl Txn<Started> {
    pub fn begin_prepare(self) -> Txn<Preparing> { self.cast() }
    pub fn abort(self) -> Txn<Aborting> { self.cast() }
}
impl Txn<Preparing> {
    /// Only way to obtain Txn<Committing>: proof that the Commit record is durable.
    pub fn decide_commit(self, proof: DurableCommit) -> Txn<Committing> { debug_assert_eq!(proof.txid, self.id); self.cast() }
    pub fn decide_abort(self) -> Txn<Aborting> { self.cast() }
}
/// Unforgeable token: constructed only by the WAL writer after fdatasync returned Ok.
pub struct DurableCommit { txid: TxId, lsn: Lsn, _private: () }
```

Recovery rules, keyed by the last record seen for a txid:
- No record, or only `Begin`: abort. Tell all known participants `abort` (idempotent).
- `Commit` without `Done`: re-drive commit to every participant until all ack, then write `Done`.
- `Abort` without `Done`: re-drive abort.
- `Done`: nothing to do.

Participants also recover on their own: for each locally prepared txid, ask the coordinator `resolve(txid)`. The answer is "committed" or "unknown ⇒ aborted", per presumed abort.

### 1.5 The Participant trait

```rust
#[async_trait::async_trait]
pub trait Participant: Send + Sync + 'static {
    /// Stable identity, persisted in the decision log.
    fn id(&self) -> ParticipantId;
    fn capabilities(&self) -> Capabilities; // {atomic, deferred, compensating, read_only_capable, one_phase}

    /// Execute/stage work for this txn. Must leave no externally visible trace.
    async fn stage(&self, tx: &TxCtx, step: &StepSpec) -> Result<StageReport, PartError>;

    /// Durable promise: after Ok(Prepared), commit MUST be possible after a crash,
    /// and the participant MUST NOT unilaterally abort.
    async fn prepare(&self, tx: &TxCtx) -> Result<Vote, PartError>; // Vote::{Prepared, ReadOnly}

    /// Idempotent; may be called repeatedly, including after restart. Must not fail
    /// for "already committed"; transient errors are retried forever with backoff.
    async fn commit(&self, txid: TxId) -> Result<(), PartError>;
    async fn abort(&self, txid: TxId) -> Result<(), PartError>;

    /// One-phase path: stage+prepare+commit collapsed; participant-local commit is the commit point.
    async fn commit_one_phase(&self, tx: &TxCtx) -> Result<Outcome, PartError> { Err(PartError::Unsupported) }

    /// Startup: enumerate txns this participant holds in prepared/in-doubt state.
    async fn recover(&self) -> Result<Vec<(TxId, LocalState)>, PartError>;
}

pub enum PartError {
    Transient(anyhow::Error),            // retry with backoff
    VoteNo(Reason),                      // during stage/prepare only => abort txn
    Conflict(ResourceKey),               // isolation conflict => abort/retry txn
    Fatal(anyhow::Error),                // participant broken; mark in-doubt, page operator
    Unsupported,
}
```

Design rules:
- `commit`/`abort` are *total* functions over any prior state, including "never heard of it". That handles out-of-order abort, the "null compensation"/"suspension" problems DTM solves with barrier tables, inside the framework instead of pushing it onto users.
- Each participant keeps a tiny local log (or redb table) of `(txid → Staged | Prepared | Committed | Aborted)`. The prepare is durable only when that entry *and* the staged data are fsynced.

### 1.6 Tokio cancellation safety

- A client RPC future can be dropped at any `.await`. **Never drive the protocol from the request future.** The API handler submits the transaction to a per-txn actor task (`tokio::spawn`) and awaits a `oneshot` for the outcome. Dropping the request drops only the receiver.
- The commit decision is a message to the single WAL-writer task. Once enqueued, its durability doesn't depend on any caller. The `DurableCommit` token is the only way to reach the phase-2 code.
- The phase-2 fan-out runs under a `JoinSet` owned by the actor. Retries are infinite with jittered backoff and are bounded only by operator intervention. Abort and timeout paths use `tokio_util::sync::CancellationToken` *only before* the decision.
- Adapters must document which of their awaits are cancel-safe. Use `tokio::select!` only on cancel-safe branches (channel `recv`, `sleep`). Never use it on `pwrite`/`fdatasync` futures (run those on a dedicated blocking thread or `spawn_blocking`).
- Graceful shutdown: stop admission, let in-flight transactions reach a decision or abort, flush the WAL, exit. A crash at any point must be equivalent to this.

### 1.7 Filesystem participant

The staging root must be on the **same filesystem** as the target, so publishing is a rename and never a copy.
- **Single file:** write to `O_TMPFILE` in the target directory, `fsync`, then at commit `linkat(AT_EMPTY_PATH)` (or `renameat2` over the target). Fsync the directory.
- **Directory tree replace:** stage the full tree as a sibling. At commit, `renameat2(RENAME_EXCHANGE)` swaps old and new in one atomic step, and the old tree is garbage-collected after `Done`.
- **Multi-path updates:** POSIX has no atomic multi-rename. Commit becomes a **redo list** persisted at prepare time. Commit replays it idempotently (each step is "rename if source exists, else verify target content hash"). After a crash, recovery rolls forward. Atomicity across a crash is guaranteed; *instantaneous* visibility is not. Readers that need a consistent cut must take shared locks through the daemon, or the managed tree must be one swappable directory or symlink. On Btrfs, subvolume snapshot plus swap gives an atomic whole-tree cut; offer it as an optional backend.
- **Prepare durability:** fsync staged files and directories, then write the redo list, fsync, and record `Prepared` locally.

### 1.8 OS-process participant (sandboxed execution)

This is the novel part. The process runs inside a **private staged view** and never touches the real world until commit.

1. **Containment:**
   - New user, mount, PID, IPC and network namespaces (`clone3` with `CLONE_INTO_CGROUP`), in a dedicated cgroup v2 leaf.
   - Abort and timeout use `cgroup.kill`. It has been available since Linux 5.14, kills the whole subtree with SIGKILL, and the kernel docs say it "will deal with concurrent forks appropriately and is protected against migrations".\[24\]\[25\] That beats walking the process tree with signals.
   - Track the leader with a pidfd (poll it from Tokio via `AsyncFd`) to avoid PID-reuse races.
2. **Filesystem staging:**
   - Mount overlayfs with `lowerdir` = the real managed tree(s) (read-only), and per-transaction `upperdir`/`workdir` on the same filesystem as the target. The kernel docs require the workdir to be on the same filesystem as the upperdir.\[26\]\[27\]
   - Never share an upper or work dir between mounts; the kernel docs call that behavior undefined.\[27\]
   - Use `fsync=strict` if you want copy-ups durable before prepare. The default "auto" mode doesn't fsync directories or metadata-only copy-ups.\[26\]
   - Commit translates the upperdir into filesystem-participant operations: regular files become renames; whiteouts and opaque dirs become deletes and directory swaps. The publish then reuses §1.7's redo machinery.
   - Abort means unmount and `rm -rf` the upper.
3. **Confinement:**
   - Use a seccomp filter (deny `mount`, `ptrace`, `keyctl`, raw sockets, and so on) plus Landlock rulesets.
   - Landlock's TCP bind/connect control needs ABI v4 (Linux 6.7). UDP support arrives at ABI v10 per current kernel docs.\[28\]\[29\] Gate on the ABI at runtime and fail closed: if the kernel can't enforce the policy, refuse the transaction instead of silently degrading.\[30\]\[31\]
   - A network namespace with *no* interfaces is the simplest egress denial.
4. **Deferred egress:**
   - If a step needs to "send" something (HTTP call, queue publish), expose a Unix socket or loopback proxy *inside* the sandbox that records requests into the outbox and returns a synthetic ack (Speculator's "buffer" strategy).\[32\]
   - Steps that need real responses before commit (e.g., reading a remote API) can be allowed as **read-only egress** through an allowlisted proxy, explicitly marked as breaking repeatability.
5. **CRIU:** not needed for the MVP. Abort is "kill and discard", which needs no restore. CRIU becomes relevant only if you later want to *pause* a prepared long-running process across daemon restarts. Treat that as a non-goal.

**Process exit semantics:** exit code 0 means the stage succeeded. Non-zero, a signal, or a timeout means VoteNo. A process can't be prepared while still running: the step must have exited before prepare. Long-lived daemons as participants are a non-goal.

### 1.9 Manifest format and API

TOML (or YAML) manifests for batch transactions, plus a gRPC streaming API for interactive transactions.

```toml
[txn]
name = "rebuild-and-publish"
timeout = "10m"
isolation = "serializable"          # per declared resources

[[resource]]
id = "site"; kind = "fs.tree"; path = "/srv/site"; mode = "write"

[[resource]]
id = "db";   kind = "postgres"; dsn_ref = "secrets/main"; mode = "write"

[[step]]
id = "build"; kind = "process"; argv = ["make", "-C", "/work", "site"]
mounts = [{ resource = "site", at = "/out", mode = "rw-staged" }]
network = "deny"

[[step]]
id = "record"; kind = "sql"; resource = "db"; after = ["build"]
sql = "INSERT INTO deploys(id, sha) VALUES ($txid, $build.output.sha)"

[[effect]]
id = "notify"; kind = "http.deferred"; after_commit = true
request = { method = "POST", url = "https://hooks.example/deploy", idempotency_key = "$txid" }
```

API surface:
- `Begin` / `AddStep` / `Commit` / `Abort` / `Status` / `Watch` (server stream).
- Admin: `ListInDoubt`, `ForceResolve(txid, Commit|Abort, reason)` (with an audit record in the WAL), `Locks`, `WaitForGraph`.
- CLI `txp` built with clap, plus a REST gateway only if needed.

**Exit criteria for Phase 1:**
- `kill -9` at a randomized point in 10k scripted runs never yields a partial publish.
- Recovery re-drives all `Commit`-without-`Done` transactions.
- A 1-participant transaction issues exactly one forced write (the participant's) on the 1PC path.
- Group commit is demonstrated (batch-size histogram > 1 under concurrent load).

**Main risks:**
- overlayfs edge cases: copy-up of large files is a full copy; rename-dir semantics; xattrs on the upper filesystem.
- Rootless namespace or cgroup delegation requirements.
- Semantic gaps between "upperdir diff" and the intended publish (hard links, special files). Define a rejected-file-type list and fail the vote on violations.

## Phase 2: Isolation, Recovery Hardening, Deterministic Simulation (8–10 weeks)

### 2.1 Concurrency control

**Recommendation:**
- **Strict two-phase locking at the coordinator** over *declared logical resources* (paths/subtrees, queue names, KV key ranges, `pg:table/key` predicates), held through `Done`.
- **Conservative acquisition for batch manifests:** all locks are acquired up front in a canonical total order (resource-ID sort), which makes deadlocks impossible by construction.
- **Wound-wait** for interactive transactions that acquire locks incrementally: an older transaction wounds (aborts) a younger holder, and a younger requester waits.
- Keep a **wait-for graph** built in the lock manager purely for observability and as a backstop detector with cycle-breaking. Timeouts are the last resort, not the mechanism.

Lock manager design:
- Hierarchical intention locks (IS/IX/S/SIX/X) over a path trie, so locking `/srv/site` conflicts correctly with `/srv/site/img/a.png`.
- One sharded `HashMap<ResourceKey, LockQueue>` behind a `parking_lot::Mutex` per shard. Waiters are oneshot channels, so no polling.
- Lock state is **not** persisted. On restart, prepared transactions reacquire their locks from the participant `recover()` lists *before* admission reopens. That is the classic restart-locking step, and skipping it is a well-known source of isolation bugs.

**Isolation you can honestly offer:**

| Resource class | Isolation offered | Mechanism |
|---|---|---|
| Daemon-managed files/trees, queues, KV | Serializable (strict 2PL) among transactions; staged state invisible to non-txn readers | Coordinator locks + staging |
| Unmanaged readers of managed trees | Read-committed-like (they see pre- or post-publish state; may see mid-publish for multi-path redo unless single-swap layout) | Atomic rename/exchange |
| Postgres participant | Delegated to Postgres. Recommend `SERIALIZABLE` locally *plus* coordinator locks on declared logical keys, because local SSI at each site doesn't by itself guarantee global serializability across sites | `PREPARE TRANSACTION` |
| Process internals | N/A (process exits before prepare) | Sandbox |
| External RPC / email | None (effect deferred to post-commit) | Outbox |

Don't promise a global MVCC snapshot across heterogeneous participants. With files and processes in the mix, there is no shared timestamp authority that Postgres will honor. A future option is *coordinator-managed MVCC* for daemon-owned KV and queues only.

### 2.2 Deterministic simulation testing (DST)

Make the core runtime-agnostic through traits: `Disk` (open/pwrite/fdatasync/rename/fsync_dir), `Net`, `Clock`, `Rng`, `Spawner`, `ProcessLauncher`. In production these are Tokio and real syscalls. In simulation:
- **turmoil** for network partitions, delays and multi-host clusters on one thread. Its unstable fs feature provides simulated filesystems "for crash-consistency testing" with `fs_sync_probability`.\[33\]
- A **custom simulated disk** that keeps a durable image and a volatile page-cache image, and on "crash" keeps only synced bytes plus a random subset of unsynced *sectors*. It injects `EIO` on fdatasync (then verifies the process panics), torn writes at sector granularity, and misdirected or lost writes.
- **Determinism leaks:** the S2 team found turmoil's runtime-level seam insufficient because transitive crates read time and entropy directly, and Rust's `HashMap` is randomly seeded. They built **mad-turmoil**, which overrides `clock_gettime` via libc symbol interposition. Adopt that pattern, or use **madsim** (RisingWave's simulator, with simulated tonic, etcd and Kafka clients) if you prefer swapping the whole runtime via `--cfg madsim`.\[34\]\[35\]
- **Recommendation:** turmoil + mad-turmoil + your own disk sim, because the daemon is Tokio-native and turmoil is owned by the tokio-rs org. Use FoundationDB-style BUGGIFY points (madsim exposes a `buggify` module) to widen rare paths.\[36\]
- Run in CI nightly at FoundationDB scale: FoundationDB's "Simulation and Testing" documentation states that "Simulation runs tens of thousands of simulations every night".
- Process participants can't run inside the simulator. Model them with a `SimProcess` that produces upperdir diffs, and test the real sandbox separately in VM-based crash tests (below).

### 2.3 Other verification layers

- **loom** for the lock manager, the group-commit queue, and the `DurableCommit` handoff.
- **shuttle** (awslabs, 0.9.4, actively developed) for randomized schedule exploration of larger async components. Its README warns "Shuttle is not sound (a passing Shuttle test does not prove the code is correct)", so pair it with loom for small kernels.\[37\]\[38\]
- **stateright** to model-check the *Rust* coordinator plus participant actors, using the same state-transition functions the daemon uses (the core crate is pure: `fn step(state, event) -> (state, Vec<Effect>)`).
- **proptest** for WAL encode/decode, recovery-scan truncation, and manifest validation. Use state-machine tests that run random op sequences with random crash points against a reference model.
- **Crash-recovery harness:** a QEMU/KVM VM with `dm-flakey`/`dm-log-writes` to replay block-level write logs and crash at every flush boundary (ALICE-style). This is where real overlayfs, rename and fsync behavior gets validated.
- **Atomicity checker:** after each simulated or real run, a checker reads the final state of every participant (file hashes, PG rows, outbox contents) and asserts for each txid: all effects present ⇔ `Commit` durable; none present ⇔ not committed. Add an Elle/Jepsen-style history checker for isolation anomalies on the daemon-managed KV and queue resources.

**Exit criteria:**
- TLC, stateright and DST all green.
- One million or more DST seeds with no invariant violations.
- Every fixed bug has a pinned seed regression test.
- The VM crash harness passes for ext4 and XFS (and Btrfs if supported).

## Phase 3: Database, Queue and External-Effect Adapters (6–8 weeks)

### 3.1 Postgres (true 2PC)
- `BEGIN; ...; PREPARE TRANSACTION '<gid>'` at prepare; `COMMIT PREPARED`/`ROLLBACK PREPARED` (callable "from any session") at phase 2.\[39\]\[40\]
- GID = `txp:<cluster>:<txid>`. It must be under 200 bytes.\[41\]
- `recover()` = `SELECT gid FROM pg_prepared_xacts WHERE gid LIKE 'txp:<cluster>:%'`.
- Operational guardrails:
  - `max_prepared_transactions` defaults to 0 (feature disabled).\[42\]\[43\]\[44\] The daemon must check it at startup and refuse to register the participant otherwise.
  - Prepared transactions keep their locks and interfere with VACUUM, so export a metric for the oldest prepared xact age and alert on it.\[39\]\[41\]
  - `transaction_timeout` (PG 17+) stops counting at PREPARE, so Postgres won't save you from orphans.\[39\] Your in-doubt tooling must.
- Use `tokio-postgres` with one dedicated connection per in-flight transaction until PREPARE, since the session is released at PREPARE.\[39\]
- 1PC path: plain `COMMIT` when Postgres is the only writer.

### 3.2 Queues
- Default: **deferred** publish via a transactional outbox stored in the decision log batch (`Outbox` records ride in the same forced write as `Commit`). After commit, an outbox dispatcher publishes with an idempotency key `(txid, effect_id)` and marks it delivered. Delivery is at-least-once; consumers dedupe, or the broker's idempotent-producer features are used.
- If the queue is daemon-owned (an embedded queue), it gets true atomicity and strict 2PL like the KV.
- Consuming from a queue inside a transaction means acks are deferred to commit. Messages stay leased with visibility timeouts longer than the transaction timeout.

### 3.3 RPC / HTTP / irreversible effects
- **Deferred** (preferred): `after_commit = true` effects go through the outbox. The guarantee is "will eventually happen exactly once given an idempotent receiver".
- **Reservation (TCC)** where the remote supports it: prepare = try/reserve, commit = confirm, abort = cancel. Model it as a normal participant with `Capabilities::compensating = false, atomic = true-ish`, and document that isolation is whatever the remote gives.
- **Compensation-only** (explicit opt-in, flagged in the manifest as `irreversible = true` with a `compensate` step): only allowed as the *last* step, executed after all other participants have voted Prepared. That shrinks the window in which a compensation is ever needed to "the effect succeeded but the commit record write failed". Surface such transactions as `CompensatedCommit` or `Compensating` in status, never as clean aborts.
- **Deny**: anything else that egresses (SMTP, payments without auth/capture) is rejected at manifest validation.

### 3.4 Guarantee matrix by participant type

| Participant | Abort guarantee | Commit guarantee | Visibility before commit | Notes |
|---|---|---|---|---|
| File / dir (managed root, same FS) | **True rollback** (discard staging) | Atomic across crash (redo roll-forward); instantaneous for single-file or single-swap layout | None | Multi-path publish not instantaneous for unmanaged readers |
| OS process (sandboxed, fs-only effects) | **True rollback** (`cgroup.kill` + discard upper) | Same as files | None (private mount ns, no net) | Must exit before prepare; no long-lived daemons |
| OS process with network | Deny, or **deferred** via proxy/outbox | Effects delivered after commit | None for writes; reads allowed if allowlisted | Fail closed if Landlock ABI is insufficient |
| Postgres | **True rollback** (`ROLLBACK PREPARED`) | True 2PC | Per PG isolation + coordinator locks | Requires `max_prepared_transactions > 0`\[43\] |
| Daemon-owned KV/queue | **True rollback** | True | None (strict 2PL) | Best isolation story |
| External queue | **Deferred** (never sent) | At-least-once + idempotency key | None | Consumer dedupe required |
| RPC with reserve/confirm | Cancel (semantic rollback) | Confirm | Remote-defined | TCC |
| Irreversible RPC/email/payment | **Compensation only** or deny | Effect is the commit | Visible immediately | Last-step only, flagged |

## Phase 4: Distributed Coordinator and Remote Participants (10–14 weeks)

### 4.1 Protocol choice
**2PC whose coordinator state machine is replicated by Raft.** Gray & Lamport show that Paxos Commit "uses 2F+1 coordinators and makes progress if at least F+1 of them are working", and that classic 2PC is the F=0 special case.\[45\] A Raft-replicated coordinator gets the same non-blocking-on-coordinator-failure property with one consensus group and much less protocol surface. Participants still block if they are partitioned from *every* coordinator replica, which is inherent: no atomic commit protocol avoids blocking under arbitrary partitions.

- **Why not 3PC:** it assumes bounded message delays and fails under network partitions and asynchronous timing, so it trades blocking for unsafety in exactly the failure modes you care about. That is why production systems put consensus under 2PC instead.
- **Why not Calvin as the core:** Calvin removes distributed commit by ordering transactions deterministically up front and executing them deterministically. It requires knowing read/write sets ahead of time ("dependent transactions" are not natively supported), and OS processes aren't deterministic.\[46\]\[47\] *Borrow* the idea for batch manifests: declared resources plus a sequenced log give deadlock-free, contention-friendly lock acquisition.
- **Why not Percolator as the core:** Percolator/TiKV put the transaction record (primary lock) inside a transactional MVCC store with a timestamp oracle. Your participants aren't one KV store. Use it only for the daemon-owned KV if you ever shard it.
- **Later optimization (Phase 5): parallel commits.** CockroachDB's STAGING record lists in-flight writes, and the transaction is committed once all of them are durable, which "cuts the commit latency of a transaction in half, from two rounds of consensus down to one".\[48\]\[49\] The analog here: write `Staging{txid, participants}` to Raft *concurrently* with prepares. The transaction is implicitly committed once every participant's prepare is durable, and recovery must query participants to decide. Only do this after the TLA+ model of it passes. It moves the commit point off a single record, which complicates in-doubt tooling. Note also the CockroachDB issue showing that unresolved STAGING intents produce spurious lock-timeout errors for other transactions.\[50\]

### 4.2 Raft library

**Recommendation:** **openraft**, pinned to the 0.9.x line (0.9.25 is a bug-fix-only release backporting nine fixes), moving to 0.10 once it goes stable (its "Release v0.10.0 Stable" tracking issue is still open).\[5\]\[51\] Reasons:
- Async and runtime-pluggable, so it fits Tokio.
- Storage and network are traits, so you implement `RaftLogStorage` directly over the Phase 1 segment WAL and keep one fsync path.
- Actively maintained, and it is the engine under Databend's meta-service.\[4\]

Tradeoff: it is pre-1.0, and a safety defect was fixed as recently as September 2026.\[5\] Wrap it behind your own `ReplicatedLog` trait and run it under your DST harness with turmoil partitions.

**Alternative:** tikv/raft-rs (0.7.0, March 2023). It is the most production-proven core (TiKV), but it is a synchronous `Ready`-loop "core Consensus Module only" where you build log, state machine and transport yourself.\[6\]\[52\]\[53\] Its crates.io releases are stale, and TiKV appears to consume it from git (an inference). Choose it if you want full control of the I/O loop, e.g., for an io_uring-driven design later.

Membership: use Raft joint consensus for 3→5 changes, and add a learner before voting.\[54\] Coordinator leadership comes with a **leader lease** only for read-only status queries; all decisions go through the log.

### 4.3 Migration without rewriting the core
- The core crate is a pure state machine: `apply(LogRecord) -> Vec<Effect>`. Phase 1 feeds it from the local WAL. Phase 4 feeds it from Raft `apply`. Effects such as "send commit to P3" are executed only by the **leader**. A new leader re-derives outstanding effects from the replicated state, which is exactly Phase 1's recovery procedure.
- `DurableCommit` becomes "Raft-committed at index i". The type signature stays the same.
- Participants that were in-process become either in-process (on the leader node, only for node-local resources; see the risk below) or **remote participant agents** (`txp-agent`) running the same adapters next to their resources.

### 4.4 Remote participant protocol (gRPC)
- Messages: `Stage`, `Prepare`, `Commit`, `Abort`, `Resolve(txid)` (participant → coordinator), `Recover`, `Heartbeat`.
- Every message carries `(cluster_id, coordinator_term, txid, participant_id, attempt)`. Participants reject messages from stale terms for *new* stage/prepare work, but accept `Commit`/`Abort` from any term, because the decision is immutable once logged.
- **Idempotency:** the participant-local log is the dedupe table. Responses are pure functions of local state, so duplicates are harmless.
- **Failure detection:** use timeouts only to decide *abort before the decision* (presumed abort). After the decision, never time out: retry forever, and surface the transaction as in-doubt after N minutes.
- **Clocks:** correctness must not depend on clocks. Use logical timestamps (txid = `(raft_term, index)` or a hybrid logical clock) for wound-wait priority. Wall-clock is used only for timeouts and metrics. There is no TrueTime equivalent, so don't offer external consistency across independent participants.

### 4.5 Node-local resources
Files and processes are physically on a host. In the distributed phase, an agent on each host owns its node-local participants. The coordinator cluster owns only decisions and locks. Locks also move into the replicated state machine, or are partitioned per agent for node-local resources. Default: the agent holds locks for its own resources, and the coordinator holds locks for cross-host logical resources.

**Exit criteria:**
- Jepsen-style runs (partitions, leader kills, clock skew, disk faults via DST and real VMs) with zero atomicity violations.
- Leader failover during the commit fan-out completes all transactions.
- Membership change under load succeeds.

**Risks:**
- openraft API churn.
- Snapshot size if transaction tables grow. Keep only non-Done transactions in the snapshot.
- Partitioned agents holding prepared Postgres transactions and locks for long periods. Mitigate with in-doubt tooling and alerts.

## Phase 5: Performance and Operability (ongoing)

**Benchmarks to define (criterion + a custom load generator):**
1. Commit latency p50/p99 for 1PC, 2-participant and 5-participant transactions, separated into WAL fdatasync time, participant prepare time and fan-out.
2. Decision-log throughput vs concurrency (expect it to be bound by fsync latency; the group-commit batch-size histogram is the key metric).
3. Abort cost for process participants (`cgroup.kill` + overlay teardown) and copy-up cost vs file size.
4. Lock-manager throughput under contention (Zipfian keys); wound rate.
5. Recovery time vs log size and in-flight transaction count.
6. Phase 4: commit latency with 3 vs 5 replicas, with and without STAGING.

**Techniques:**
- Group commit everywhere: decision log, participant-local logs, and outbox dispatch batching.
- No polling: lock waits, completion and outbox use channels; pidfd and cgroup `cgroup.events` are watched through `AsyncFd`/inotify.
- Pipelining: prepares are sent concurrently, and the commit record is written the moment the last vote arrives.
- Read-only and 1PC paths.

**io_uring:** it matters only once you are syscall-bound, not fsync-bound. Likely candidates:
- Chaining write+fdatasync in a single submission.
- Batching many small participant-log appends.
- Overlay publish (many renames).

Defer it. Keep the `Disk` trait so a `tokio-uring`/monoio backend can be added without touching the core.

**Observability:**
- `tracing` spans per txid (span fields: txid, phase, participant), OTLP export.
- Prometheus metrics: in-flight by phase, in-doubt count, oldest prepared age per participant, fsync latency histogram, batch size, lock wait time, wound/deadlock counts.
- `txp doctor` for in-doubt analysis: for each in-doubt transaction, show the log records, the votes, the participant-local states, and a recommended action.
- `ForceResolve` writes an audited heuristic-decision record and *refuses* any decision that contradicts a durable `Commit`.

**Configuration:** a single TOML file with per-participant blocks, hot-reload only for non-safety settings, and a startup self-test that checks kernel features: cgroup v2 with `cgroup.kill`, overlayfs, Landlock ABI, `max_prepared_transactions`.

## Recommended Workspace Layout

```
txp/
├── spec/                 # TLA+ (TwoPhase-PA, Staging), trace-validation scripts
├── crates/
│   ├── txp-core/         # pure state machines, LogRecord, TxId, typestate, no I/O, no tokio
│   ├── txp-io/           # Disk/Net/Clock/Rng/ProcessLauncher traits + real impls
│   ├── txp-wal/          # segment WAL, group commit writer, recovery scan, snapshots
│   ├── txp-lock/         # hierarchical lock manager, wound-wait, wait-for graph
│   ├── txp-participant/  # Participant trait, Capabilities, PartError, local participant log
│   ├── txp-fs/           # staging, O_TMPFILE/linkat, renameat2 EXCHANGE, redo publish
│   ├── txp-proc/         # namespaces, cgroup v2, pidfd, overlayfs, seccomp, landlock, egress proxy
│   ├── txp-pg/           # PREPARE TRANSACTION adapter
│   ├── txp-outbox/       # deferred effects, dispatcher, idempotency
│   ├── txp-engine/       # coordinator actors, recovery, admission/backpressure
│   ├── txp-proto/        # protobuf: client API + participant protocol (versioned)
│   ├── txp-server/       # txpd binary (tonic), config, metrics
│   ├── txp-cli/          # txp binary (clap)
│   ├── txp-agent/        # Phase 4 remote participant host
│   ├── txp-raft/         # Phase 4 openraft integration over txp-wal
│   └── txp-sim/          # DST harness: turmoil + mad-turmoil + SimDisk + SimProcess + checkers
└── bench/                # criterion + load generator
```
This keeps the library option open (core, wal, lock and participant are embeddable) while the deliverable stays an application.

## How This Design Addresses the Eight Gaps

| Gap in existing coordinators | Prior-art status | This design |
|---|---|---|
| 1. No consensus of their own | Seata/DTM keep state in a shared SQL DB | Own WAL (Phase 1) → own Raft group (Phase 4); commit point is a record you control |
| 2. Weak isolation | Seata AT default global isolation is read uncommitted; saga/TCC none\[2\] | Strict 2PL over declared resources, hierarchical locks, staging invisibility; per-class isolation table |
| 3. Atomicity only via blocking XA | XA quality varies per DB | Generic durable-promise trait; PG via native 2PC; files/processes via staging; coordinator blocking removed by Raft |
| 4. Narrow participant notion | SQL DBs / RPC with hand compensations | Files, sandboxed OS processes, queues, daemon KV, deferred effects, explicit compensation class |
| 5. Correctness burden on users | Barrier tables, idempotency left to users | Total, idempotent commit/abort; framework-owned dedupe; outbox idempotency keys |
| 6. Thin verification | Rare DST/TLA+ | TLA+ refinement of TCommit, stateright, DST with disk faults, loom/shuttle, VM crash harness, atomicity checker |
| 7. Performance afterthought | DB polling | Group commit, 1PC, read-only, pipelined prepares, no polling, later STAGING and io_uring |
| 8. Poor in-doubt/deadlock handling | Timeouts | Wound-wait + conservative ordering, wait-for graph API, `ListInDoubt`/`ForceResolve` with audit, prepared-age alerts |

## Recommendations: MVP Cut

**In the MVP (end of Phase 1 plus the DST subset of Phase 2):**
- Single node.
- Custom WAL with group commit.
- Presumed-abort 2PC + 1PC + read-only.
- Filesystem participant (single-file and dir-swap; the multi-path redo can follow).
- Process participant with overlay + `cgroup.kill` + netns-deny.
- Coordinator-level strict 2PL with conservative ordering for manifests.
- gRPC + CLI, in-doubt listing, the TLA+ model, turmoil-based DST with the simulated disk.

**Defer:**
- Postgres (Phase 3; it is easy but adds operational hazards).
- The queue outbox, interactive transactions with wound-wait, Btrfs backend, Raft, STAGING, io_uring.

**Explicit non-goals:**
- Kernel modifications.
- Transparent transactionality for arbitrary unconfined processes.
- Long-running daemons as participants.
- Global MVCC snapshots across heterogeneous participants.
- External consistency across independent systems.
- Windows/macOS.
- CRIU-based pause/resume.

## Caveats and Open Questions

- **Multi-path filesystem atomicity** is crash-atomic, not visibility-atomic, unless you constrain layouts to a single swappable directory or symlink, or use Btrfs snapshots. Decide early whether managed roots must be "swap-shaped".
- **Overlay publish fidelity:** translating upperdir whiteouts, opaque dirs, xattrs, hard links and device nodes back into real operations is fiddly. Restrict the allowed file types in v1.
- **Rootless operation:** user namespaces, overlay-in-userns and cgroup delegation vary by distro and kernel. The daemon may need to run privileged in v1.
- **Landlock UDP and MPTCP:** UDP restrictions are documented at ABI v10. An August 2026 patch series proposes MPTCP at ABI v12, but that is a proposed patch, not an established kernel feature.\[28\]\[55\] Treat netns-without-interfaces as the baseline.
- **openraft maturity:** it is active but pre-1.0, so budget for API churn. raft-rs is a fallback with stale releases.
- **Global serializability with Postgres SSI participants** isn't guaranteed by local serializability alone. Rely on coordinator-level locks for cross-participant invariants.
- **Crate data:** shuttle 0.9.4's exact release date couldn't be verified, and sled's rewrite status is unclear (no releases since October 2024).\[56\] Re-verify crate versions at implementation time.

## Sources

1. [resume() and open trust page-cache bytes after a failed fdatasync (fsyncgate) · Issue #231 · gustavoamigo/bytecaskdb](https://github.com/gustavoamigo/bytecaskdb/issues/231)
2. [In-Depth Analysis of Seata AT Mode Transaction Isolation Levels and Global Lock Design](https://seata.apache.org/blog/seata-at-lock/)
3. [What Is Seata?](https://seata.apache.org/docs/overview/what-is-seata/)
4. [OpenRaft — async Rust library // Lib.rs](https://lib.rs/crates/openraft)
5. [build(deps): bump openraft from 0.9.21 to 0.9.25 in /backend by dependabot\[bot\] · Pull Request #257 · zyvorai/fabric](https://github.com/zyvorai/fabric/pull/257)
6. [Releases · tikv/raft-rs](https://github.com/tikv/raft-rs/releases)
7. [raft 0.5.0 - Docs.rs](https://docs.rs/crate/raft/0.5.0)
8. [redb — Rust database // Lib.rs](https://lib.rs/crates/redb)
9. [redb/README.md at master · cberner/redb](https://github.com/cberner/redb/blob/master/README.md)
10. [fjall-rs](https://fjall-rs.github.io/)
11. [sled 0.16.5 - Docs.rs](https://docs.rs/crate/sled/0.16.5)
12. [sled - crates.io: Rust Package Registry](https://crates.io/crates/sled/1.0.0-alpha.1)
13. <https://lib.rs/crates/okaywal>
14. [okaywal - Rust](https://khonsulabs.github.io/okaywal/main/okaywal/index.html)
15. [Examples/specifications/transaction\_commit at master · tlaplus/Examples](https://github.com/tlaplus/Examples/tree/master/specifications/transaction_commit)
16. [Consensus on Transaction Commit](https://blog.acolyer.org/2016/01/13/consensus-on-transaction-commit/)
17. [stateright 0.9.0 - Docs.rs](https://docs.rs/crate/stateright/0.9.0)
18. [How Pipelining consensus writes speeds up distributed SQL transactions](https://www.cockroachlabs.com/blog/transaction-pipelining/)
19. [fjall - Rust](https://docs.rs/fjall)
20. [raft-engine/Cargo.toml at master · tikv/raft-engine](https://github.com/tikv/raft-engine/blob/master/Cargo.toml)
21. [raft-engine 0.1.0-pre-alpha - Docs.rs](https://docs.rs/crate/raft-engine/0.1.0-pre-alpha)
22. [Starting from PostgreSQL’s fsync Failure](https://medium.com/@baotiao/starting-from-postgresqls-fsync-failure-840af156585c)
23. [All Your GUCs in a Row: data\_sync\_retry — The Build](https://thebuild.com/blog/all-your-gucs-in-a-row-datasyncretry/)
24. [Linux Security and Isolation APIs Control Groups (cgroups): Introduction](https://www.man7.org/training/download/secisol_cgroups_v2_slides.pdf)
25. [Control Group v2 — The Linux Kernel documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html)
26. [Overlay Filesystem — The Linux Kernel documentation](https://docs.kernel.org/filesystems/overlayfs.html)
27. [Overlay Filesystem — The Linux Kernel documentation](https://www.kernel.org/doc/html/v5.8/filesystems/overlayfs.html)
28. [Landlock: unprivileged access control — The Linux Kernel documentation](https://docs.kernel.org/userspace-api/landlock.html)
29. [Linux Landlock - Nono Docs](https://nono.sh/docs/cli/internals/landlock)
30. [Landlock-Sharp - Network rules](https://docs.curiosity.ai/landlock-sharp/guides/network-rules)
31. [Landlock: the Unprivileged Sandbox Built Into Modern Kernels - Big Iron](https://www.bigiron.cc/guides/landlock-the-unprivileged-sandbox-built-into-modern-kernels)
32. [SOSP 2005 Speculative Execution in a Distributed File System Ed Nightingale](https://pages.cs.wisc.edu/~dusseau/Classes/CS739/Questions/nightingale05.slides-S09.ppt)
33. [Turmoil — Rust network library // Lib.rs](https://lib.rs/crates/turmoil)
34. [Deterministic simulation testing for async Rust - S2.dev](https://s2.dev/blog/dst)
35. [Deterministic Simulation: A New Era of Distributed System Testing (Part 1 of 2)](https://risingwave.com/blog/deterministic-simulation-a-new-era-of-distributed-system-testing/)
36. [madsim - Rust](https://docs.rs/madsim)
37. <https://lib.rs/crates/shuttle>
38. [shuttle - crates.io: Rust Package Registry](https://crates.io/crates/shuttle)
39. [All Your GUCs in a Row: max\_prepared\_transactions — The Build](https://thebuild.com/blog/all-your-gucs-in-a-row-max_prepared_transactions/)
40. [PostgreSQL: Documentation: 18: PREPARE TRANSACTION](https://www.postgresql.org/docs/current/sql-prepare-transaction.html)
41. [PostgreSQL: Documentation: 8.3: PREPARE TRANSACTION](https://www.postgresql.org/docs/8.3/sql-prepare-transaction.html)
42. [max\_prepared\_transactions - pgPedia - a PostgreSQL Encyclopedia](https://pgpedia.info/m/max_prepared_transactions.html)
43. [PostgreSQL Documentation: max\_prepared\_transactions parameter](https://postgresqlco.nf/doc/en/param/max_prepared_transactions/)
44. [prepared transactions and their dangers](https://www.cybertec-postgresql.com/en/prepared-transactions/)
45. [\[PDF\] Consensus on transaction commit](https://www.semanticscholar.org/paper/Consensus-on-transaction-commit-Gray-Lamport/2e702e5f18330f8aab4c976bb4b06e7c6b767121)
46. [Calvin: Fast Distributed Transactions for Partitioned Database Systems](http://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf)
47. [Calvin: Fast Distributed Transactions for Partitioned Database Systems](https://muratbuffalo.blogspot.com/2022/04/calvin-fast-distributed-transactions.html)
48. [Parallel Commits: An atomic commit protocol for globally distributed transactions](https://www.cockroachlabs.com/blog/parallel-commits/)
49. [Transaction Layer - CockroachDB](https://www.cockroachlabs.com/docs/stable/architecture/transaction-layer.html)
50. [concurrency: committed transaction's unresolved intents can cause spurious lock timeout errors · Issue #165097 · cockroachdb/cockroach](https://github.com/cockroachdb/cockroach/issues/165097)
51. [Issues · databendlabs/openraft](https://github.com/databendlabs/openraft/issues)
52. [raft rs](https://github.com/Fullstop000/raft-rs)
53. [raft 0.7.0 - Docs.rs](https://docs.rs/crate/raft/latest/source/README.md)
54. [raft - Rust](https://tikv.github.io/doc/raft/index.html)
55. [\[PATCH 0/6\] landlock: Support MPTCP bind and...](https://ratatoskr.run/netdev/2026/08/17480108/t)
56. [sled — Rust concurrency library // Lib.rs](https://lib.rs/crates/sled)

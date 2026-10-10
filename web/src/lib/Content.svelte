<script>
  import CodeBlock from './CodeBlock.svelte';
  import Callout from './Callout.svelte';
  import ArchDiagram from './diagrams/ArchDiagram.svelte';
  import ProtocolDiagram from './diagrams/ProtocolDiagram.svelte';
  import AtomicityDiagram from './diagrams/AtomicityDiagram.svelte';
  import * as snip from './snippets.js';
</script>

<!-- 01 OVERVIEW -->
<section id="overview">
  <h2><span class="num">01</span> Overview</h2>
  <p class="sub">txp gives a build-and-publish job the same guarantee a database gives a write: it either happens completely or not at all — across files <em>and</em> the processes that produce them.</p>

  <p class="lede" style="margin-top:26px">The problem it solves is the half-finished deploy. A script that rebuilds a site, writes files, then dies partway through leaves a directory that is neither the old version nor the new one. txp removes that state entirely.</p>

  <p>A transaction is described by a <strong>manifest</strong> — a TOML file listing the resources it touches and the ordered steps it runs. You submit it with <code>txp run</code>; the daemon <code>txpd</code> acquires locks, runs every step against <em>private, staged</em> copies of the resources, and only makes the result visible if the whole set succeeds. The single commit point is one <code>fdatasync</code>'d record in the daemon's own write-ahead log. Anything that goes wrong before that record — a failing build, a killed step, a power loss — aborts the transaction and leaves the managed tree exactly as it was.</p>

  <div class="grid c3">
    <div class="card">
      <div class="ic"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M20 6 9 17l-5-5"/></svg></div>
      <h4>Atomic by construction</h4>
      <p>Steps stage into overlay / same-filesystem staging. Publish is a <span class="k">rename</span> or <span class="k">RENAME_EXCHANGE</span> — instantaneous for single swaps, idempotent redo for multi-path sets.</p>
    </div>
    <div class="card">
      <div class="ic"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 2 4 5v6c0 5 3.5 8.5 8 11 4.5-2.5 8-6 8-11V5z"/></svg></div>
      <h4>Crash-safe</h4>
      <p>A custom segmented WAL with CRC32C framing and group commit. Every coordinator crash point has been exercised by a deterministic harness; recovery re-drives or presumes abort.</p>
    </div>
    <div class="card">
      <div class="ic"><svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zM2 12h20"/></svg></div>
      <h4>Sandboxed processes</h4>
      <p>Each process step runs in private mount/pid/net/ipc/uts namespaces with no network interfaces, Landlock read-only outside its overlay, and a seccomp denylist.</p>
    </div>
  </div>

  <Callout type="info">
    This repository implements <b>Phase&nbsp;1 plus the crash-testing subset of Phase&nbsp;2</b> of the design document. It is a working MVP, not yet a multi-node production system — see <a href="#status">Status &amp; roadmap</a> for the exact coverage, deviations, and known limits.
  </Callout>
</section>

<hr class="div" />

<!-- 02 GUARANTEES -->
<section id="guarantees">
  <h2><span class="num">02</span> What is guaranteed</h2>
  <p class="sub">Two kinds of participant, two precise contracts. Nothing a transaction does is ever visible before it commits.</p>

  <div class="tbl-wrap">
    <table>
      <thead><tr><th>Participant</th><th>On abort</th><th>On commit</th><th>Visible before commit</th></tr></thead>
      <tbody>
        <tr>
          <td><code>fs.tree</code> root<br><span style="color:var(--ink-mute);font-size:13px">files, dir swaps, deletes</span></td>
          <td class="yes">True rollback — staging is discarded</td>
          <td>Crash-atomic via idempotent redo; single-file and single-swap publishes are instantaneous</td>
          <td class="no">Never</td>
        </tr>
        <tr>
          <td>Sandboxed process<br><span style="color:var(--ink-mute);font-size:13px">fs-only effects</span></td>
          <td class="yes"><code>cgroup.kill</code> + discard upper dir</td>
          <td>Same as files (overlay upperdir → redo list)</td>
          <td class="no">Never — private mount ns, no network interfaces, Landlock, seccomp denylist</td>
        </tr>
      </tbody>
    </table>
  </div>

  <AtomicityDiagram />

  <p>Every crash point of the coordinator — <code>kill -9</code> equivalents inserted between every protocol step — has been checked by the <code>txp-crashtest</code> harness, and the protocol itself is model-checked in TLA⁺ under <code>spec/</code>.</p>

  <Callout type="warn">
    <b>Scope.</b> Only same-filesystem files and processes are transactional. Process effects outside the managed tree (network writes, external databases) are <em>not</em> covered — egress is deny-only today, and there is no external participant, so there is no cross-resource DB+fs atomicity yet.
  </Callout>
</section>

<hr class="div" />

<!-- 03 ARCHITECTURE -->
<section id="architecture">
  <h2><span class="num">03</span> Architecture</h2>
  <p class="sub">A Rust workspace of eleven focused crates: pure state machines at the core, I/O and OS mechanism at the edges, a coordinator in the middle, and a thin daemon + CLI on top.</p>

  <ArchDiagram />

  <div class="tbl-wrap">
    <table>
      <thead><tr><th>Crate</th><th>Responsibility</th></tr></thead>
      <tbody>
        <tr><td><code>txp-core</code></td><td>Pure state machines: ids, <code>LogRecord</code>, <code>TxnTable</code> + recovery rules, typestate driver, the <code>DurableCommit</code> token</td></tr>
        <tr><td><code>txp-wal</code></td><td>Segmented WAL, CRC32C framing, group-commit writer thread, torn-tail recovery, checkpoints, simulated disk</td></tr>
        <tr><td><code>txp-lock</code></td><td>Strict 2PL over hierarchical keys, conservative canonical-order acquisition, wait-for graph</td></tr>
        <tr><td><code>txp-participant</code></td><td>The <code>Participant</code> trait, <code>Capabilities</code>, <code>PartError</code>, local journal, registry</td></tr>
        <tr><td><code>txp-fs</code></td><td>Filesystem participant: staging, <code>RedoOp</code> publish (rename / <code>RENAME_EXCHANGE</code>, inode-based idempotency)</td></tr>
        <tr><td><code>txp-proc</code></td><td>Process participant: namespaces, overlayfs, cgroup&nbsp;v2, Landlock, seccomp, upperdir → <code>RedoOp</code> translation</td></tr>
        <tr><td><code>txp-manifest</code></td><td>TOML manifests, validation, topological step ordering</td></tr>
        <tr><td><code>txp-engine</code></td><td>The coordinator: per-txn tasks, 2PC presumed abort, 1PC fast path, read-only, recovery, crash points, auth policy</td></tr>
        <tr><td><code>txp-server</code></td><td><code>txpd</code> — the daemon (newline-JSON over a Unix socket)</td></tr>
        <tr><td><code>txp-cli</code></td><td><code>txp</code> — the command-line client</td></tr>
        <tr><td><code>txp-sim</code></td><td><code>txp-crashtest</code> harness + atomicity checker</td></tr>
      </tbody>
    </table>
  </div>

  <p>Alongside the crates: <code>spec/</code> holds the TLA⁺ model (<code>TwoPhasePA.tla</code>) and its TLC runner, <code>examples/</code> holds manifests, and <code>scripts/</code> holds the root sandbox tests and the TLC driver.</p>

  <Callout type="tip">
    The design keeps the <b>commit decision</b> in a crate with no I/O at all (<code>txp-core</code>). The only way a transaction enters phase two is by holding a <code>DurableCommit</code> token, which can only be minted after the forced commit record lands — the type system enforces the invariant that nothing publishes before the log says so.
  </Callout>
</section>

<hr class="div" />

<!-- 04 REQUIREMENTS -->
<section id="requirements">
  <h2><span class="num">04</span> Requirements</h2>
  <p class="sub">The sandbox depends on Linux kernel mechanisms, so txp is built and run on a Linux host or VM.</p>

  <div class="grid c2">
    <div class="card">
      <h4>Platform</h4>
      <p>Linux with <b>cgroup&nbsp;v2</b>, <b>overlayfs</b>, <b>Landlock</b>, and <b>seccomp</b>. The daemon needs <code>CAP_SYS_ADMIN</code> (run as root) for process steps — namespaces, overlayfs and cgroups require it. Filesystem-only manifests run unprivileged.</p>
    </div>
    <div class="card">
      <h4>Toolchain</h4>
      <p>A recent Rust toolchain (workspace edition <b>2024</b>). Build with Cargo. No <code>protoc</code> required — the wire protocol is newline-delimited JSON, not gRPC.</p>
    </div>
  </div>

  <Callout type="warn">
    On Ubuntu, <code>kernel.apparmor_restrict_unprivileged_userns=1</code> blocks unprivileged user namespaces, which is why the daemon runs privileged (a privileged v1 is allowed by the design). Filesystem-only manifests still work without root.
  </Callout>

  <h3 class="sec">Build</h3>
  <CodeBlock lang="sh" code={snip.buildCmd} />
</section>

<hr class="div" />

<!-- 05 QUICKSTART -->
<section id="quickstart">
  <h2><span class="num">05</span> Quickstart</h2>
  <p class="sub">From a built workspace to an atomic deploy in five steps.</p>

  <ol class="steps">
    <li>
      <h4>Start the daemon</h4>
      <p>Process steps need <code>CAP_SYS_ADMIN</code>, so <code>txpd</code> runs as root. Point it at a data directory (its WAL and journals) and a socket path.</p>
      <CodeBlock lang="sh" code={snip.daemonStart} />
    </li>
    <li>
      <h4>Confirm it is healthy</h4>
      <p><code>self-test</code> drives a tiny transaction end to end.</p>
      <CodeBlock lang="sh" code={snip.selfTest} />
    </li>
    <li>
      <h4>Validate a manifest</h4>
      <p>Checks syntax, resource references, and the topological order of steps — without running anything.</p>
      <CodeBlock lang="sh" code={snip.validateCmd} />
    </li>
    <li>
      <h4>Run it</h4>
      <p><code>txp run</code> exits <code>0</code> if and only if the transaction committed. On any failure the managed tree is untouched.</p>
      <CodeBlock lang="sh" code={snip.runCmd} />
    </li>
    <li>
      <h4>Inspect and operate</h4>
      <p>Query live state, then checkpoint or shut down cleanly.</p>
      <CodeBlock lang="sh" code={snip.inspectCmd} />
    </li>
  </ol>
</section>

<hr class="div" />

<!-- 06 DAEMON -->
<section id="daemon">
  <h2><span class="num">06</span> Running the daemon</h2>
  <p class="sub"><code>txpd</code> speaks newline-delimited JSON over a Unix socket and authorizes submitters by their peer credentials.</p>

  <h3 class="sec">Configuration</h3>
  <div class="tbl-wrap">
    <table>
      <thead><tr><th>Setting</th><th>How</th><th>Meaning</th></tr></thead>
      <tbody>
        <tr><td><code>TXP_DATA_DIR</code></td><td>env</td><td>Where the WAL, checkpoints and participant journals live</td></tr>
        <tr><td><code>TXP_SOCKET</code></td><td>env</td><td>Unix socket path for both the daemon and the <code>txp</code> client</td></tr>
        <tr><td><code>--socket-mode</code></td><td>flag</td><td>Permission bits on the socket (defaults to <code>0660</code>)</td></tr>
        <tr><td><code>--allow-uid &lt;uid&gt;</code></td><td>flag</td><td>Permit an additional uid to submit transactions</td></tr>
        <tr><td><code>--allow-anyone</code></td><td>flag</td><td>Disable the submission check entirely (restores the old open behaviour)</td></tr>
      </tbody>
    </table>
  </div>

  <h3 class="sec">Who may submit</h3>
  <p>Submission is authorized by <code>SO_PEERCRED</code>. Out of the box, <strong>root</strong> and the <strong>daemon owner</strong> (the user who invoked <code>sudo</code>) may submit; add others with <code>--allow-uid</code>. Read-only introspection (<code>list</code>, <code>status</code>, <code>locks</code>, …) stays open; the privileged operations <code>Run</code>, <code>Checkpoint</code> and <code>Shutdown</code> are gated.</p>

  <h3 class="sec">Which identity steps run as</h3>
  <ul class="ck">
    <li>A <b>root submitter</b>'s process steps run as <code>SUDO_UID:SUDO_GID</code> (the user who invoked sudo) unless a step sets <code>user = "uid:gid"</code>.</li>
    <li>A <b>non-root submitter</b> is pinned to its own uid/gid — that is the default, and a <code>user</code> requesting any other identity is rejected.</li>
    <li>The managed root must be writable by the effective uid, exactly as it would be outside the sandbox.</li>
  </ul>
</section>

<hr class="div" />

<!-- 07 MANIFEST -->
<section id="manifest">
  <h2><span class="num">07</span> Manifest format</h2>
  <p class="sub">A transaction is a TOML document with one <code>[txn]</code> header, one or more <code>[[resource]]</code> declarations, and an ordered list of <code>[[step]]</code> entries.</p>

  <CodeBlock lang="toml" file="manifest.toml" code={snip.manifestExample} />

  <h3 class="sec">Resources</h3>
  <div class="tbl-wrap">
    <table>
      <thead><tr><th>Field</th><th>Meaning</th></tr></thead>
      <tbody>
        <tr><td><code>id</code></td><td>Name referenced by steps' <code>resource</code> / <code>mounts</code></td></tr>
        <tr><td><code>kind</code></td><td><code>fs.tree</code> — a managed filesystem subtree</td></tr>
        <tr><td><code>path</code></td><td>The real on-disk path the tree lives at</td></tr>
        <tr><td><code>mode</code></td><td><code>write</code> for a tree the transaction modifies (read-only otherwise)</td></tr>
      </tbody>
    </table>
  </div>

  <h3 class="sec">Step kinds</h3>
  <div class="tbl-wrap">
    <table>
      <thead><tr><th>Kind</th><th>Does</th><th>Key fields</th></tr></thead>
      <tbody>
        <tr><td><code>process</code></td><td>Runs a command in a sandbox against staged overlays of its mounted resources</td><td><code>argv</code>, <code>cwd</code>, <code>mounts</code>, <code>network</code>, <code>timeout</code>, <code>user</code>, <code>env</code></td></tr>
        <tr><td><code>fs.put</code></td><td>Writes a single file into a resource</td><td><code>resource</code>, <code>path</code>, and <code>content</code> <em>or</em> <code>source</code></td></tr>
        <tr><td><code>fs.delete</code></td><td>Deletes a path within a resource</td><td><code>resource</code>, <code>path</code></td></tr>
        <tr><td><code>fs.replace_tree</code></td><td>Swaps a whole directory in with <code>RENAME_EXCHANGE</code></td><td><code>resource</code>, <code>source</code> (directory)</td></tr>
      </tbody>
    </table>
  </div>

  <h3 class="sec">Ordering &amp; stacking</h3>
  <p>Steps declare dependencies with <code>after = ["id", …]</code>; the manifest is validated into a topological order. A later <code>process</code> step that mounts the same resource sees the earlier steps' staged output — overlays stack as lower layers, so a pipeline of build stages composes naturally.</p>

  <h4 class="sec">Variable substitution</h4>
  <p><code>$txid</code> is substituted in <code>content</code>, <code>argv</code> and <code>env</code>. Process steps additionally receive <code>TXP_TXID</code> and <code>TXP_STEP</code> in their environment.</p>
</section>

<hr class="div" />

<!-- 08 EXAMPLES -->
<section id="examples">
  <h2><span class="num">08</span> Worked examples</h2>
  <p class="sub">Two manifests ship in <code>examples/</code> — one that commits, one that proves the abort path.</p>

  <h3 class="sec">Rebuild and publish <span class="pill done">commits</span></h3>
  <p>Build a site inside a sandbox and publish it atomically together with a deploy stamp in a second resource. Nothing under <code>/srv/site</code> changes unless every step succeeds.</p>
  <CodeBlock lang="toml" file="examples/rebuild-and-publish.toml" code={snip.rebuildToml} />
  <p>The <code>build</code> step's output is staged; <code>verify</code> reads that staged output through a stacked overlay; only once both pass does the coordinator publish the new <code>/srv/site</code> and the stamp in <code>/srv/deploylog</code> together.</p>

  <h3 class="sec">Failing build <span class="pill wip">aborts</span></h3>
  <p>The build writes files and then exits non-zero. The whole transaction aborts and <code>/srv/site</code> is left untouched — including the <code>rm -rf src</code> the staged step performed, which never reaches the real tree.</p>
  <CodeBlock lang="toml" file="examples/failing-build.toml" code={snip.failingToml} />
</section>

<hr class="div" />

<!-- 09 CLI -->
<section id="cli">
  <h2><span class="num">09</span> CLI commands</h2>
  <p class="sub">The <code>txp</code> client talks to a running <code>txpd</code> over <code>$TXP_SOCKET</code>. Introspection commands are open; <code>run</code>, <code>checkpoint</code> and <code>shutdown</code> require an authorized submitter.</p>

  <div class="tbl-wrap">
    <table>
      <thead><tr><th>Command</th><th>What it does</th></tr></thead>
      <tbody>
        <tr><td><code>txp self-test</code></td><td>Drive a built-in transaction end to end to confirm the daemon is healthy</td></tr>
        <tr><td><code>txp validate &lt;manifest&gt;</code></td><td>Parse and validate a manifest (references, step order) without running it</td></tr>
        <tr><td><code>txp run &lt;manifest&gt;</code></td><td>Submit and run a transaction; exit <code>0</code> iff it committed <span class="pill todo">gated</span></td></tr>
        <tr><td><code>txp list</code></td><td>List all known transactions and their phases</td></tr>
        <tr><td><code>txp status &lt;txid&gt;</code></td><td>Show one transaction's phase and per-step state</td></tr>
        <tr><td><code>txp in-doubt</code></td><td>List transactions committed but not yet done (awaiting recovery drive)</td></tr>
        <tr><td><code>txp orphans</code></td><td>List orphaned participant journals on disk</td></tr>
        <tr><td><code>txp locks</code></td><td>Show the lock table / wait-for graph</td></tr>
        <tr><td><code>txp wal-stats</code></td><td>WAL statistics, including the group-commit batch-size histogram</td></tr>
        <tr><td><code>txp checkpoint</code></td><td>Take an atomic snapshot and truncate WAL segments <span class="pill todo">gated</span></td></tr>
        <tr><td><code>txp shutdown</code></td><td>Stop the daemon gracefully <span class="pill todo">gated</span></td></tr>
        <tr><td><code>txp wal-dump &lt;dir&gt;</code></td><td>Offline inspection of a WAL directory — no daemon needed</td></tr>
      </tbody>
    </table>
  </div>

  <h3 class="sec">Testing &amp; verification binaries</h3>
  <CodeBlock lang="sh" code={snip.testBins} />
</section>

<hr class="div" />

<!-- 10 PROTOCOL -->
<section id="protocol">
  <h2><span class="num">10</span> The protocol</h2>
  <p class="sub">A presumed-abort two-phase commit with a one-phase fast path and a read-only vote — the whole of which fits in a paragraph, and is model-checked in TLA⁺.</p>

  <ProtocolDiagram />

  <ol class="steps">
    <li><h4>Lock</h4><p>Locks are taken up front in canonical order, which makes the acquisition deadlock-free (conservative strict 2PL over hierarchical keys).</p></li>
    <li><h4>Begin</h4><p>A <code>Begin</code> record is written <em>unforced</em> — it is cheap and only matters if we later commit. It carries the submitter (uid/gid/pid) for the audit trail.</p></li>
    <li><h4>Stage</h4><p>Every step runs against private staging: overlay upperdirs for processes, same-filesystem staging for files. Nothing is visible yet.</p></li>
    <li><h4>Decide</h4><p>If exactly one participant staged anything, it commits in <b>one phase</b> — its local journal record <em>is</em> the commit point, costing zero coordinator fsyncs. Otherwise all staged participants <code>prepare</code> concurrently (fsync their staging + journal), and the coordinator writes the <b>forced <code>Commit</code> record</b> through the group-commit writer.</p></li>
    <li><h4>Publish</h4><p>Obtaining a <code>DurableCommit</code> token — the only way into phase two — the coordinator fans out <code>commit</code> with unbounded retries, writes <code>Done</code>, and releases the locks.</p></li>
  </ol>

  <h3 class="sec">Abort &amp; recovery</h3>
  <p>Any failure before the decision aborts: the coordinator fans out <code>abort</code>, then writes the lazy <code>Abort</code>/<code>Done</code> records. On restart, recovery replays the log — a <code>Commit</code> without a <code>Done</code> is re-driven to completion; <strong>anything else is presumed aborted</strong>, which is why abort and done records can be lazy. A participant's <code>abort</code> is total and can report that it had already locally committed under 1PC; a recovering coordinator adopts that outcome with an audited <code>ForceResolve</code> record.</p>

  <Callout type="tip">
    <b>Why "presumed abort" matters:</b> the common case — a transaction that never reaches a commit decision — writes nothing durable that must be cleaned up. The forced fsync happens once, only for transactions that actually commit, and group commit amortizes even that across concurrent transactions into a single write and a single <code>fdatasync</code>.
  </Callout>
</section>

<hr class="div" />

<!-- 11 SECURITY -->
<section id="security">
  <h2><span class="num">11</span> Security model</h2>
  <p class="sub">Tier&nbsp;0 hardening — the hard blockers for running under untrusted local clients — is complete. A sandboxed step is confined by five independent mechanisms, and the daemon authenticates every submitter.</p>

  <h3 class="sec">Submission &amp; identity <span class="pill done">done</span></h3>
  <ul class="ck">
    <li><b>Peer authentication.</b> <code>serve()</code> captures <code>SO_PEERCRED</code> via <code>UnixStream::peer_cred</code>; an <code>AuthPolicy</code> gates submission to root, the daemon owner, and any <code>--allow-uid</code>. Read-only introspection stays open; <code>Run</code>, <code>Checkpoint</code> and <code>Shutdown</code> are gated.</li>
    <li><b>Pinned run-as uid.</b> A non-root submitter's process steps are pinned to its own uid/gid (<code>RunAsPolicy</code>); a <code>user = "0:0"</code> from a non-root / non-owner peer is rejected. Root and in-process submitters keep free choice.</li>
    <li><b>Audit trail.</b> <code>LogRecord::Begin</code> carries an <code>Option&lt;Submitter&gt;</code> (uid/gid/pid), <code>#[serde(default)]</code> so older logs still replay.</li>
  </ul>

  <h3 class="sec">Sandbox confinement <span class="pill done">done</span></h3>
  <div class="grid c2">
    <div class="card"><h4>Namespaces</h4><p>Private <b>mount</b>, <b>pid</b>, <b>net</b>, <b>ipc</b> and <b>uts</b> namespaces. The net namespace has no interfaces — egress is deny-only.</p></div>
    <div class="card"><h4>overlayfs</h4><p>An overlay <b>upperdir per step</b>, with earlier steps stacked as lowers. The managed path appears at its real location inside the sandbox; the upperdir is translated into the publish redo list.</p></div>
    <div class="card"><h4>cgroup v2</h4><p><code>cgroup.kill</code> tears the whole step process tree down on abort or timeout — no escapees.</p></div>
    <div class="card"><h4>Landlock</h4><p>Fail-closed, read-only outside the overlay — a step cannot reach the rest of the filesystem even with a valid path.</p></div>
  </div>
  <p style="margin-top:18px"><b>No <code>unsafe</code> code.</b> The workspace forbids it. Syscalls go through <code>nix</code>, and Landlock, seccomp and xattrs through safe crates. Rather than running setup code between <code>fork</code> and <code>exec</code> in the multithreaded daemon, <code>txpd</code> starts the <code>txp-sandbox</code> helper (installed next to it), which unshares the namespaces and re-executes itself as PID&nbsp;1 to mount, drop privileges, confine and <code>exec</code> the step. Setup failures come back over a socket, so they are never confused with the step's own exit status.</p>
  <p style="margin-top:18px"><b>seccomp-bpf denylist.</b> A filter generated with <code>seccompiler</code> (portable across x86-64 / aarch64 / riscv64) is installed in the confined init process after <code>no_new_privs</code>, fail-closed, refusing a fixed set of administrative and exploit-primitive syscalls with <code>EPERM</code>. It is additive to the namespaces, Landlock and the unprivileged uid; a strict allowlist is deliberately not attempted, since it would break ordinary build tools. A root test asserts the step actually runs under seccomp filter mode&nbsp;2.</p>

  <Callout type="info">
    <b>Deferred out of Tier&nbsp;0:</b> supplementary-group checks for a pinned submitter (only the primary gid is honoured today), a connection-level reject for unauthorized peers (currently only privileged ops are gated), and making the seccomp list configurable.
  </Callout>
</section>

<hr class="div" />

<!-- 12 VERIFICATION -->
<section id="verification">
  <h2><span class="num">12</span> Verification</h2>
  <p class="sub">The atomicity claims rest on three independent checks: a formal model of the protocol, a deterministic crash harness over the coordinator, and a simulated disk under the WAL.</p>

  <div class="grid c3">
    <div class="card"><span class="k">TLA⁺</span><h4>Protocol model</h4><p><code>TwoPhasePA.tla</code> models presumed-abort 2PC: coordinator crash/recovery from the durable log, participant crash before prepare, message duplication, the read-only vote, and the 1PC fast path with outcome-reporting abort. TLC checks it for <code>RM = &#123;r1,r2,r3&#125;</code> in seconds.</p></div>
    <div class="card"><span class="k">crash points</span><h4>Deterministic harness</h4><p><code>txp-crashtest</code> injects a <code>kill -9</code> equivalent at every protocol step (via <code>TXP_CRASH_AT</code>) and asserts recovery never yields a partial publish — including with a live sandboxed process step.</p></div>
    <div class="card"><span class="k">sim disk</span><h4>WAL fault injection</h4><p>A simulated disk drops unsynced sectors, tears writes, and returns <code>EIO</code>; the WAL must truncate torn tails, refuse genuine corruption, and turn an fsync error into an abort.</p></div>
  </div>

  <h3 class="sec">The negative test</h3>
  <p>Confidence in the model comes from watching it fail on purpose: comment out the guard <code>tcState = "collecting"</code> in <code>TCCommit</code> — allowing a commit decision after an abort was sent — and TLC produces a <code>Consistent</code> counterexample, the exact I2 (no commit-after-abort) violation the real code's <code>TxnTable::apply</code> rejects.</p>

  <CodeBlock lang="sh" code={snip.tlcCmd} />

  <p style="margin-top:4px;font-size:14px;color:var(--ink-mute)">Trace validation (replaying TLC traces against the Rust state machine) and a refinement proof against <code>TCommit</code> are Phase&nbsp;2 work.</p>
</section>

<hr class="div" />

<!-- 13 STATUS -->
<section id="status">
  <h2><span class="num">13</span> Status &amp; roadmap</h2>
  <p class="sub">An honest map of what is implemented, where the implementation deviates from the design document and why, and what is deliberately deferred.</p>

  <h3 class="sec">Done — Phase 1 + crash-testing subset of Phase 2</h3>
  <ul class="ck">
    <li>Invariants <b>I1–I6</b> encoded in the type system / state machine (<code>txp-core</code>)</li>
    <li>TLA⁺ model with TLC coverage of all actions, invariants and the I2 action property (<code>spec/</code>)</li>
    <li>Custom segmented WAL: CRC32C framing, torn-tail truncation vs. corruption refusal, preallocation + <code>fdatasync</code>, fsync-error ⇒ abort (<code>txp-wal</code>)</li>
    <li>Group commit — one write and one <code>fdatasync</code> per batch, adaptive batching, no timers (<code>txp-wal</code>)</li>
    <li>Checkpoint (atomic snapshot) + segment truncation; WAL-level simulated disk</li>
    <li>Typestate driver, persisted phase enum, recovery rules (<code>txp-core</code>, <code>txp-engine</code>)</li>
    <li>Participant trait: total idempotent commit/abort, local journal, <code>recover()</code> (<code>txp-participant</code>)</li>
    <li>Tokio cancellation safety — protocol in its own task, log writes on a dedicated thread, post-decision retries unbounded</li>
    <li>fs participant: same-fs staging, rename / <code>RENAME_EXCHANGE</code>, redo list persisted at prepare, idempotent replay</li>
    <li>proc participant: full namespace set, overlayfs with stacked lowers, cgroup v2, timeout, Landlock, unprivileged uid, upperdir → publish translation</li>
    <li>1PC fast path (verified zero coordinator fsyncs), read-only vote, pipelined prepares</li>
    <li>Strict 2PL at the coordinator, hierarchical keys, conservative acquisition, wait-for graph</li>
    <li>Manifest format + validation + topological order; daemon + CLI; Tier 0 authorization, uid pinning, seccomp</li>
  </ul>

  <h3 class="sec">Deviations from the design document</h3>
  <div class="tbl-wrap">
    <table>
      <thead><tr><th>Area</th><th>What &amp; why</th></tr></thead>
      <tbody>
        <tr><td>API transport</td><td>Newline-JSON over a Unix socket, not gRPC/tonic — the VM has no <code>protoc</code>, the surface is tiny and versionable. gRPC arrives with the remote participant protocol in Phase 4.</td></tr>
        <tr><td>Privilege</td><td>The daemon runs as root for process steps — Ubuntu's AppArmor blocks unprivileged user namespaces. fs-only manifests run unprivileged.</td></tr>
        <tr><td>Egress</td><td>"deny" only (network namespace without interfaces). The deferred-egress proxy / outbox is Phase 3.</td></tr>
        <tr><td>seccomp</td><td>A denylist, additive to namespaces + Landlock + unprivileged uid + <code>no_new_privs</code>; a strict allowlist would break ordinary build tools.</td></tr>
        <tr><td>Crash testing</td><td>Deterministic crash points + a WAL-layer sim disk, not yet whole-engine deterministic simulation or a dm-flakey VM harness.</td></tr>
        <tr><td>1PC safety add</td><td><code>Participant::abort</code> returns an <code>AbortOutcome</code> so a recovering coordinator adopts a local 1PC commit — found while writing the TLA⁺ model.</td></tr>
      </tbody>
    </table>
  </div>

  <h3 class="sec">Roadmap — production-readiness tiers</h3>
  <details class="tier">
    <summary><span class="tl pill done">Tier 0</span> Security &amp; multi-tenancy — <span style="color:var(--accent)">done</span> <span class="chev">›</span></summary>
    <ul>
      <li>Peer authentication (<code>SO_PEERCRED</code> + <code>AuthPolicy</code>), run-as uid pinning, submitter in the audit log, seccomp denylist — all landed.</li>
    </ul>
  </details>
  <details class="tier">
    <summary><span class="tl pill todo">Tier 1</span> Availability &amp; durability architecture <span class="chev">›</span></summary>
    <ul>
      <li>Document the single-node ceiling (RPO/RTO, single-disk assumption, in-doubt-until-same-host-replays).</li>
      <li>Wire-protocol version field + negotiation on every request.</li>
      <li>On-disk format version headers (WAL, checkpoint, journals) + migration path.</li>
    </ul>
  </details>
  <details class="tier">
    <summary><span class="tl pill todo">Tier 2</span> Resource safety / DoS <span class="chev">›</span></summary>
    <ul>
      <li>Cap request / manifest size (unbounded line reads can OOM the daemon).</li>
      <li>Admission control: in-flight semaphore, connection-count limit, idle timeout.</li>
      <li>Per-step cgroup limits (<code>memory.max</code>, <code>pids.max</code>, <code>cpu.max</code>); staging-space quota + pre-flight disk-free check.</li>
      <li>Automatic orphan-journal GC.</li>
    </ul>
  </details>
  <details class="tier">
    <summary><span class="tl pill todo">Tier 3</span> Operability &amp; observability <span class="chev">›</span></summary>
    <ul>
      <li>Metrics export (Prometheus/OTLP): commit latency, in-doubt count, fsync rate, batch sizes, staging usage.</li>
      <li>Health / readiness endpoint; structured JSON logs with txid-correlated spans.</li>
      <li>Bounded graceful shutdown; packaging (systemd unit, container image, config file).</li>
    </ul>
  </details>
  <details class="tier">
    <summary><span class="tl pill todo">Tier 4</span> Release engineering / supply chain <span class="chev">›</span></summary>
    <ul>
      <li>CI running <code>cargo test</code>, clippy, the crash harness and the TLA⁺ check.</li>
      <li>Pinned toolchain + MSRV + <code>clippy.toml</code>; <code>cargo-audit</code>/<code>cargo-deny</code> + SBOM; CHANGELOG + semver.</li>
    </ul>
  </details>
  <details class="tier">
    <summary><span class="tl pill todo">Tier 5</span> Verification depth <span class="chev">›</span></summary>
    <ul>
      <li>Whole-engine deterministic simulation (madsim/turmoil); <code>loom</code>/<code>shuttle</code> for concurrency.</li>
      <li>Real-disk fault injection (dm-flakey); fuzz the WAL, TOML and JSON parsers; trace-validate TLC against the Rust state machine.</li>
    </ul>
  </details>

  <Callout type="warn">
    <b>Known rough edges.</b> <code>cgroup.events</code> is polled every 5&nbsp;ms while draining a killed cgroup (an inotify watch is planned). Overlay copy-up of large files is a full copy (inherent to overlayfs). Multi-path publishes are crash-atomic but not instantaneous for unmanaged readers — use a single swappable directory where that matters.
  </Callout>
</section>

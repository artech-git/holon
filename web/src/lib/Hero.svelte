<div class="hero">
  <div class="wrap">
    <span class="eyebrow"><span class="dot"></span> Phase&nbsp;1 MVP · Tier&nbsp;0 hardened · Rust 2024</span>
    <h1>All-or-nothing for the <span class="g">filesystem and the processes</span> that touch it.</h1>
    <p class="lead">
      <b style="color:var(--ink)">txp</b> runs filesystem changes and sandboxed OS processes as a single transaction.
      Every step stages into private state; nothing becomes visible until one fsynced decision record commits the whole set — or
      a crash at any point leaves the target untouched. Coordinated by a Tokio daemon over a presumed-abort two-phase commit protocol.
    </p>
    <div class="cta">
      <a class="btn primary" href="#quickstart">Get started
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M5 12h14M13 6l6 6-6 6"/></svg>
      </a>
      <a class="btn ghost" href="#protocol">How the protocol works</a>
    </div>
    <div class="badges">
      <span class="badge"><b>Linux-only</b> sandbox</span>
      <span class="badge">cgroup&nbsp;v2 · overlayfs</span>
      <span class="badge">Landlock + seccomp</span>
      <span class="badge">Custom WAL · CRC32C</span>
      <span class="badge">TLA⁺ model-checked</span>
      <span class="badge">MIT&nbsp;OR&nbsp;Apache-2.0</span>
    </div>

    <div class="flow" aria-hidden="true">
      <div class="bar"><i></i><i></i><i></i><span>the happy path</span></div>
<pre><b>txp run manifest.toml</b> ──▶ <b>txpd</b> ──▶ lock ▶ Begin ▶ stage steps ▶ prepare ▶ <b>[Commit record · fdatasync]</b> ▶ publish ▶ Done
                                 │                                  │
                                 │   <span class="mut">fs participant: same-filesystem staging, rename / RENAME_EXCHANGE redo list</span>
                                 │   <span class="mut">proc participant: mount+pid+net+ipc+uts namespaces, overlayfs upper per step,</span>
                                 │   <span class="mut">                  cgroup v2 cgroup.kill, Landlock read-only outside the overlay</span>
                                 └── <span class="mut">presumed abort: no record ⇒ aborted; abort/done records are lazy</span></pre>
    </div>
  </div>
</div>

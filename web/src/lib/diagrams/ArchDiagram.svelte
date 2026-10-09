<figure class="fig">
  <div class="figbox">
    <svg viewBox="0 0 760 486" role="img"
         aria-label="Layered crate architecture: the txp CLI and txpd daemon sit on the txp-engine coordinator, which sits on the participant, WAL and lock crates, all of which reduce to the I/O-free txp-core that holds the commit decision.">
      <defs>
        <marker id="arA" markerWidth="9" markerHeight="9" refX="7" refY="4.5" orient="auto">
          <polygon points="0,0 9,4.5 0,9" class="d-arrow" />
        </marker>
      </defs>

      <text x="156" y="13" font-size="11.5" class="d-tm">each layer depends only on the ones below it — the commit decision lives in a crate with no I/O</text>

      <!-- dependency arrows on the left rail -->
      <line x1="84" y1="95" x2="84" y2="112" class="d-conn" marker-end="url(#arA)" />
      <line x1="84" y1="187" x2="84" y2="204" class="d-conn" marker-end="url(#arA)" />
      <line x1="84" y1="279" x2="84" y2="296" class="d-conn" marker-end="url(#arA)" />
      <line x1="84" y1="371" x2="84" y2="388" class="d-conn" marker-end="url(#arA)" />

      <!-- ===== Row 1 — Interface ===== -->
      <rect x="24" y="43" width="120" height="30" rx="8" class="d-pill" />
      <text x="84" y="62" font-size="12" text-anchor="middle" class="d-td">Interface</text>
      <rect x="156" y="22" width="588" height="72" rx="11" class="d-band" />
      <rect x="176" y="38" width="170" height="40" rx="8" class="d-box" />
      <text x="261" y="54" font-size="13" text-anchor="middle" class="d-t">txp</text>
      <text x="261" y="69" font-size="10.5" text-anchor="middle" class="d-tm">command-line client</text>
      <rect x="366" y="38" width="358" height="40" rx="8" class="d-box" />
      <text x="545" y="54" font-size="13" text-anchor="middle" class="d-t">txpd</text>
      <text x="545" y="69" font-size="10.5" text-anchor="middle" class="d-tm">daemon — newline-JSON over a Unix socket</text>

      <!-- ===== Row 2 — Coordinator ===== -->
      <rect x="24" y="135" width="120" height="30" rx="8" class="d-pill" />
      <text x="84" y="154" font-size="12" text-anchor="middle" class="d-td">Coordinator</text>
      <rect x="156" y="114" width="588" height="72" rx="11" class="d-band" />
      <rect x="176" y="130" width="548" height="40" rx="8" class="d-box" />
      <text x="450" y="146" font-size="13" text-anchor="middle" class="d-t">txp-engine</text>
      <text x="450" y="161" font-size="10.5" text-anchor="middle" class="d-tm">per-txn tasks · 2PC presumed abort · 1PC fast path · recovery · auth policy</text>

      <!-- ===== Row 3 — Participants ===== -->
      <rect x="24" y="227" width="120" height="30" rx="8" class="d-pill" />
      <text x="84" y="246" font-size="12" text-anchor="middle" class="d-td">Participants</text>
      <rect x="156" y="206" width="588" height="72" rx="11" class="d-band" />
      <rect x="170" y="222" width="128" height="40" rx="8" class="d-box" />
      <text x="234" y="238" font-size="11.5" text-anchor="middle" class="d-t">txp-fs</text>
      <text x="234" y="252" font-size="9.5" text-anchor="middle" class="d-tm">staging · publish</text>
      <rect x="314" y="222" width="128" height="40" rx="8" class="d-box" />
      <text x="378" y="238" font-size="11.5" text-anchor="middle" class="d-t">txp-proc</text>
      <text x="378" y="252" font-size="9.5" text-anchor="middle" class="d-tm">sandbox · overlay</text>
      <rect x="458" y="222" width="128" height="40" rx="8" class="d-box" />
      <text x="522" y="238" font-size="11.5" text-anchor="middle" class="d-t">txp-manifest</text>
      <text x="522" y="252" font-size="9.5" text-anchor="middle" class="d-tm">TOML · validation</text>
      <rect x="602" y="222" width="128" height="40" rx="8" class="d-box" />
      <text x="666" y="238" font-size="11.5" text-anchor="middle" class="d-t">txp-participant</text>
      <text x="666" y="252" font-size="9.5" text-anchor="middle" class="d-tm">trait · journal</text>

      <!-- ===== Row 4 — Durability & concurrency ===== -->
      <rect x="24" y="319" width="120" height="30" rx="8" class="d-pill" />
      <text x="84" y="338" font-size="12" text-anchor="middle" class="d-td">Durability</text>
      <rect x="156" y="298" width="588" height="72" rx="11" class="d-band" />
      <rect x="176" y="314" width="270" height="40" rx="8" class="d-box" />
      <text x="311" y="330" font-size="12.5" text-anchor="middle" class="d-t">txp-wal</text>
      <text x="311" y="345" font-size="10" text-anchor="middle" class="d-tm">segmented WAL · CRC32C · group commit</text>
      <rect x="466" y="314" width="258" height="40" rx="8" class="d-box" />
      <text x="595" y="330" font-size="12.5" text-anchor="middle" class="d-t">txp-lock</text>
      <text x="595" y="345" font-size="10" text-anchor="middle" class="d-tm">strict 2PL · hierarchical keys · wait-for</text>

      <!-- ===== Row 5 — Core (highlighted) ===== -->
      <rect x="24" y="411" width="120" height="30" rx="8" class="d-box-hi" />
      <text x="84" y="430" font-size="12" text-anchor="middle" class="d-tab">Core</text>
      <rect x="156" y="390" width="588" height="72" rx="11" class="d-band" />
      <rect x="176" y="406" width="548" height="40" rx="8" class="d-box-hi" />
      <text x="450" y="422" font-size="13" text-anchor="middle" class="d-ta">txp-core</text>
      <text x="450" y="437" font-size="10.5" text-anchor="middle" class="d-td">pure state machines · recovery rules · the DurableCommit token (no I/O)</text>
    </svg>
  </div>
  <figcaption class="figcap">
    The workspace reads top-to-bottom as a dependency stack. <b>Nothing publishes without a <code>DurableCommit</code> token</b>,
    and that token can only be minted inside <code>txp-core</code> after the forced commit record lands — so the
    invariant is enforced by the type system, not by discipline. <code>txp-sim</code> and <code>spec/</code> verify the whole stack.
  </figcaption>
</figure>

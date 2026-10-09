<figure class="fig">
  <div class="figbox">
    <svg viewBox="0 0 820 316" role="img"
         aria-label="Atomicity model: step writes land in a private overlay upperdir while the managed tree is untouched; on commit a single atomic rename makes the new tree live, on abort the upperdir is discarded and the tree is unchanged.">
      <defs>
        <marker id="arX" markerWidth="9" markerHeight="9" refX="7.5" refY="4.5" orient="auto">
          <polygon points="0,0 9,4.5 0,9" class="d-arrow" />
        </marker>
        <marker id="arXh" markerWidth="9" markerHeight="9" refX="7.5" refY="4.5" orient="auto">
          <polygon points="0,0 9,4.5 0,9" class="d-arrow-hi" />
        </marker>
      </defs>

      <!-- ===== during the transaction ===== -->
      <text x="170" y="22" font-size="11.5" text-anchor="middle" class="d-td">during the transaction</text>

      <rect x="48" y="40" width="244" height="58" rx="9" class="d-private" />
      <text x="170" y="64" font-size="12" text-anchor="middle" class="d-ta">overlay upperdir</text>
      <text x="170" y="81" font-size="10" text-anchor="middle" class="d-tm">staged writes — private, invisible</text>

      <rect x="48" y="150" width="244" height="58" rx="9" class="d-box" />
      <text x="170" y="174" font-size="12" text-anchor="middle" class="d-t">managed tree (lowerdir)</text>
      <text x="170" y="191" font-size="10" text-anchor="middle" class="d-tm">unchanged · readers see the old tree</text>

      <!-- step writes arrow into the upper -->
      <line x1="10" y1="69" x2="46" y2="69" class="d-conn" marker-end="url(#arX)" />
      <text x="28" y="60" font-size="9.5" text-anchor="middle" class="d-tm">step</text>
      <text x="28" y="88" font-size="9.5" text-anchor="middle" class="d-tm">writes</text>

      <!-- stacked relationship -->
      <line x1="170" y1="98" x2="170" y2="150" class="d-dash-soft" />
      <text x="214" y="128" font-size="9.5" class="d-tm">stacked overlay</text>

      <!-- ===== decision ===== -->
      <path d="M380 124 L430 150 L380 176 L330 150 Z" class="d-box" />
      <text x="380" y="154" font-size="11" text-anchor="middle" class="d-ts">commit?</text>
      <line x1="292" y1="150" x2="328" y2="150" class="d-conn" marker-end="url(#arX)" />

      <!-- ===== commit path ===== -->
      <line x1="430" y1="138" x2="520" y2="92" class="d-conn-hi" marker-end="url(#arXh)" />
      <text x="474" y="104" font-size="10" class="d-tab">yes</text>
      <rect x="524" y="60" width="256" height="60" rx="9" class="d-box-hi" />
      <text x="652" y="84" font-size="12" text-anchor="middle" class="d-ta">rename / RENAME_EXCHANGE</text>
      <text x="652" y="102" font-size="10" text-anchor="middle" class="d-td">new tree goes live — one atomic step</text>

      <!-- ===== abort path ===== -->
      <line x1="430" y1="162" x2="520" y2="208" class="d-conn" marker-end="url(#arX)" />
      <text x="474" y="200" font-size="10" class="d-tam">no / fail</text>
      <rect x="524" y="180" width="256" height="60" rx="9" class="d-box-a" />
      <text x="652" y="204" font-size="12" text-anchor="middle" class="d-tam">discard upperdir</text>
      <text x="652" y="222" font-size="10" text-anchor="middle" class="d-td">tree untouched — true rollback</text>

      <!-- baseline note -->
      <text x="410" y="288" font-size="11" text-anchor="middle" class="d-tm">the managed tree only ever flips from the whole old state to the whole new state — never a partial one</text>
    </svg>
  </div>
  <figcaption class="figcap">
    Every step writes into a <b>private overlay</b> stacked over the real tree, so nothing is visible mid-transaction.
    Commit publishes the staged result with a single <code>rename</code> / <code>RENAME_EXCHANGE</code>; abort discards the
    upperdir and the tree is byte-for-byte what it was. The same model backs both filesystem and sandboxed-process steps.
  </figcaption>
</figure>

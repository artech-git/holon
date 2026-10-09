<figure class="fig">
  <div class="figbox">
    <svg viewBox="0 0 820 372" role="img"
         aria-label="Protocol timeline: the coordinator writes one forced Commit record (a single fdatasync) that divides the presumed-abort region from the committed region; Begin and Done records are unforced.">
      <defs>
        <marker id="arP" markerWidth="9" markerHeight="9" refX="7.5" refY="4.5" orient="auto">
          <polygon points="0,0 9,4.5 0,9" class="d-arrow" />
        </marker>
        <marker id="arPh" markerWidth="9" markerHeight="9" refX="7.5" refY="4.5" orient="auto">
          <polygon points="0,0 9,4.5 0,9" class="d-arrow-hi" />
        </marker>
      </defs>

      <!-- the commit point -->
      <text x="432" y="18" font-size="12" text-anchor="middle" class="d-tab">▼ the commit point — one forced fdatasync</text>
      <line x1="432" y1="26" x2="432" y2="228" class="d-dash" />

      <!-- region labels -->
      <text x="248" y="44" font-size="11.5" text-anchor="middle" class="d-tm">before the line: no Commit record ⇒ presumed abort</text>
      <text x="618" y="44" font-size="11.5" text-anchor="middle" class="d-tab">after: Commit-without-Done ⇒ recovery re-drives</text>

      <!-- lane labels -->
      <text x="12" y="118" font-size="11" class="d-td">Coordinator</text>
      <text x="12" y="196" font-size="11" class="d-td">WAL log</text>

      <!-- coordinator spine -->
      <line x1="96" y1="112" x2="772" y2="112" class="d-conn" marker-end="url(#arP)" />

      <!-- coordinator steps -->
      <rect x="100" y="93" width="58" height="38" rx="8" class="d-box" />
      <text x="129" y="116" font-size="11.5" text-anchor="middle" class="d-t">lock</text>
      <rect x="172" y="93" width="66" height="38" rx="8" class="d-box" />
      <text x="205" y="116" font-size="11.5" text-anchor="middle" class="d-t">Begin</text>
      <rect x="252" y="93" width="62" height="38" rx="8" class="d-box" />
      <text x="283" y="116" font-size="11.5" text-anchor="middle" class="d-t">stage</text>
      <rect x="328" y="93" width="84" height="38" rx="8" class="d-box" />
      <text x="370" y="116" font-size="11.5" text-anchor="middle" class="d-t">prepare</text>
      <rect x="444" y="91" width="150" height="42" rx="8" class="d-box-hi" />
      <text x="519" y="108" font-size="11.5" text-anchor="middle" class="d-ta">Commit</text>
      <text x="519" y="123" font-size="9.5" text-anchor="middle" class="d-tab">fsync · group-commit</text>
      <rect x="608" y="93" width="78" height="38" rx="8" class="d-box" />
      <text x="647" y="116" font-size="11.5" text-anchor="middle" class="d-t">publish</text>
      <rect x="700" y="93" width="64" height="38" rx="8" class="d-box" />
      <text x="732" y="116" font-size="11.5" text-anchor="middle" class="d-t">Done</text>

      <!-- writes into the WAL -->
      <line x1="205" y1="131" x2="205" y2="170" class="d-dash-soft" />
      <line x1="519" y1="133" x2="519" y2="168" class="d-conn-hi" marker-end="url(#arPh)" />
      <line x1="732" y1="131" x2="732" y2="170" class="d-dash-soft" />

      <!-- WAL records -->
      <rect x="172" y="170" width="66" height="34" rx="7" class="d-box" />
      <text x="205" y="187" font-size="10.5" text-anchor="middle" class="d-ts">Begin</text>
      <text x="205" y="199" font-size="9" text-anchor="middle" class="d-tm">unforced</text>
      <rect x="444" y="168" width="150" height="38" rx="7" class="d-box-hi" />
      <text x="519" y="185" font-size="10.5" text-anchor="middle" class="d-tab">Commit</text>
      <text x="519" y="198" font-size="9" text-anchor="middle" class="d-tab">forced · fdatasync</text>
      <rect x="700" y="170" width="64" height="34" rx="7" class="d-box" />
      <text x="732" y="187" font-size="10.5" text-anchor="middle" class="d-ts">Done</text>
      <text x="732" y="199" font-size="9" text-anchor="middle" class="d-tm">lazy</text>

      <!-- abort branch (left region) -->
      <line x1="283" y1="131" x2="283" y2="258" class="d-conn" marker-end="url(#arP)" />
      <rect x="96" y="258" width="300" height="42" rx="8" class="d-box-a" />
      <text x="246" y="275" font-size="10.5" text-anchor="middle" class="d-tam">fail before the line → fan out abort</text>
      <text x="246" y="289" font-size="9.5" text-anchor="middle" class="d-tm">lazy Abort/Done, release locks, tree untouched</text>

      <!-- commit fan-out (right region) -->
      <line x1="647" y1="131" x2="647" y2="258" class="d-conn-hi" marker-end="url(#arPh)" />
      <rect x="444" y="258" width="328" height="42" rx="8" class="d-box-hi" />
      <text x="608" y="275" font-size="10.5" text-anchor="middle" class="d-tab">fan out commit — idempotent, unbounded retries</text>
      <text x="608" y="289" font-size="9.5" text-anchor="middle" class="d-tm">participants publish, then the coordinator writes Done</text>
    </svg>
  </div>
  <figcaption class="figcap">
    One <b>forced</b> <code>Commit</code> record (a single <code>fdatasync</code>, batched by group commit) is the whole decision.
    Everything left of the line is presumed aborted on recovery; everything right is re-driven to <code>Done</code>.
    In the <b>1-participant fast path</b> the participant's own journal record is the commit point — zero coordinator fsyncs.
  </figcaption>
</figure>

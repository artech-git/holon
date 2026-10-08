# Protocol specification

`TwoPhasePA.tla` models the presumed-abort 2PC that `txp-engine` implements,
including coordinator crash/recovery from the durable log, participant crash
before prepare, message duplication, the read-only vote, and the one-phase
fast path with outcome-reporting abort.

Run TLC (needs a JRE and `tla2tools.jar`):

```sh
scripts/tlc.sh            # ≈ a few seconds for RM = {r1, r2, r3}
```

Negative test: comment out the guard `tcState = "collecting"` in `TCCommit`
(allowing a commit decision after an abort was sent) and TLC produces a
`Consistent` counterexample — the I2 violation.

Trace validation (replaying TLC traces against the Rust state machine) and a
refinement proof against `TCommit` are Phase 2 work.

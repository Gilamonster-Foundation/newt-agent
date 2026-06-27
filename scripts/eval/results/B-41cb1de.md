# Data set B — main 41cb1de (baseline + #661 compaction/summarizer series)

- **Date:** 2026-06-26
- **Codebase:** `41cb1de` = `68c9b2c` + #666 (progressive-disclosure compaction)
  + #667 (summarizer → embedded engine) + #668 (knowledge-base compaction test).
- **Driver/planner/crew:** newt @ 41cb1de; authoring `nemotron-3-nano:30b`; crew
  `[crews.home]` (planner dgx1, navigator/triage gnuc); `--max-leaves 12`.

## Deterministic grade (main's own binary)
`{"top_dgx_subs":8,"dgx_help_subs":8,"rolled_up":false,"disclosure":true,"pass":false}`
— identical to A. No regression in the #548 surface.

## Autonomous eval
- Grounding (byte-identical to A): gh 2343 + repo 744 + grep 1508 (located
  `help_lines` in `newt-tui`).
- Plan: **11 leaves**, richer than A — explicitly named `help_lines` and
  `newt-tui/src/lib.rs`, mirrored the rollup for `/models`, cited grep'd line
  numbers. Mis-targeting bug: it placed `help_lines` in `newt-cli/src/crew.rs`
  (the multi-LLM crew module) rather than `newt-tui/src/lib.rs`.
- Execution: all **11 leaves [Done]**, chained, "✓ plan complete."
  **Wall-clock: 9103 s (~2.5 h).**
- **Net change:** `README.md` (−211/+34, gutted) + a `newt-cli/Cargo.toml` tweak.
  `help_lines` (newt-tui/src/lib.rs): **0 lines changed.**
- **Grade:** `{"run":"B","top_dgx_subs":8,"pass":false}` — #548 NOT implemented.

## vs A
Identical grader outcome (`top_dgx_subs 8`, fail). Richer plan but worse,
destructive execution (README gut vs A's harmless orphan module) — most likely
run variance. #661 did not move the autonomous #548 needle.

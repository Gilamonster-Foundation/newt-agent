# Data set A — baseline (checkout 68c9b2c, before the separately-landed features)

- **Date:** 2026-06-26
- **Checkout:** `68c9b2c` (this branch `eval/548-grader` is cut from here; the
  newer `main` merges are intentionally excluded for the baseline).
- **Driver / planner / crew:** local `newt` @ 68c9b2c; authoring model
  `nemotron-3-nano:30b`; crew `[crews.home]` (planner dgx1, navigator/triage gnuc).

## 1. Grader sanity on the unmodified baseline binary
```json
{"issue":548,"top_dgx_subs":8,"dgx_help_subs":8,"rolled_up":false,"disclosure":true,"pass":false}
```
FAIL — as required (#548's top-level rollup is not implemented in the baseline;
`/dgx help` disclosure already works).

## 2. Autonomous `--one-shot` eval on this checkout
- Grounding fired: gh issue read (2343 chars) + repo context (744) + code-grep
  (1508, located `help_lines` in newt-tui).
- Plan: 9 on-topic leaves (rollup help for /dgx, hierarchical, progressive
  disclosure). Some file mis-targeting (a leaf aimed at `crew.rs`, which is the
  multi-LLM crew, not help).
- Execution: all 9 leaves `[Done]`, chained, **"✓ plan complete."** The loop is
  robust (no wedge).
- **Net result:** one file — `newt-cli/src/dgx_help.rs` (66 lines, a reasonable
  `HelpTreeNode`/`dgx_help_tree()`), but **orphaned** (never `mod`-declared, so
  never compiled) and **`help_lines` untouched**.
- **Grade of the eval result: FAIL** — identical help output to baseline
  (`top_dgx_subs:8, pass:false`). The autonomous loop completed but did NOT
  implement #548.

## Findings
- ✅ Infrastructure (read→ground→plan→grant→execute→consolidate→verify) is solid.
- ⚠️ `just check` is too weak as the success signal — an unwired module passes it.
  The behavioral grader is the fix.
- ❌ Crew-implementation quality is the ceiling: on-topic but unwired, mis-targeted.

## Baseline metric for the A/B comparison
**`top_dgx_subs = 8`, `pass = false`.** Data set B (after rebasing this branch on
`main`) re-runs the identical eval; a feature that helps should drop `top_dgx_subs`
toward 0 and/or flip `pass`.

# Data set D — stronger LOCAL navigator (qwen3.6:27b)

- **Date:** 2026-06-26 (overnight batch)
- **Codebase:** `d25662d` (same as C). **Only variable vs C: the navigator model.**
- **Crew:** planner `nemotron-3-nano:30b` (dgx1, unchanged) · **navigator
  `qwen3.6:27b`** (dgx1, local — was `qwen2.5-coder:14b` on gnuc) · triage
  `qwen2.5-coder:14b` (gnuc, unchanged). Confirmed live: qwen3.6:27b held 42.5 GB
  VRAM on dgx1 during the run.

## Run
- Plan: **9 leaves** (planner still mis-located `help_lines` to `crew.rs`; planner
  unchanged, so same planning error as B/C).
- Execution: **8/9 leaves landed**, then the final `run-check` leaf ("ensure the
  workspace compiles") got stuck >1 h — the agent has exec **denied**, so
  qwen3.6:27b spun through tool-rounds trying to run a build it cannot, each round
  a slow 27b generation. **Stopped at 8/9 after ~4h14m** to protect the E run's
  overnight runway. The substantive result was fully captured in the 8 landed
  leaves (the final leaf adds no implementation).
- **Wall-clock: ~15170 s (~4.2 h, stopped)** — ~2× B/C (27b general model on the
  DGX Spark + per-leaf `just check` + model reload between leaves).

## Result — a NEW, worse failure mode
Net change (leaves 1–8): `cli.py (+11)`, `help_utils.py (+27)`, `crew.rs (+3)`,
`help_data.rs (+14)`, `tests/help_rollup.rs (+80)`.

- **Hallucinated Python in a Rust repo:** `cli.py`, `help_utils.py` with
  `def rollup_help(commands: List[Dict[str, Any]])`. qwen3.6:27b is a *general*
  model and did not grasp that this is a Rust workspace.
- **Loose files outside any crate:** `crew.rs`, `help_data.rs`, `tests/help_rollup.rs`
  dropped at the **repo root**, not in `newt-cli/src/` or `newt-tui/src/`. Cargo
  never compiles them — which is why `just check` "passed" (nothing it added is
  in the build graph).
- **Real `help_lines` (newt-tui/src/lib.rs): 0 lines changed.**
- **Grade:** `top_dgx_subs=8, pass=false` — #548 NOT implemented.

## vs C (the clean single-variable comparison)
Swapping the executor 14b-coder → 27b-general made the result **worse**, not
better: C produced nothing (inert no-op); D produced *actively wrong* code
(Python in a Rust repo, files outside the build graph) and cost ~2× the time.
A bigger **general** model is not the lever; if anything it hurt. Strongly
suggests a coder-specialized executor is what matters — dgx1 has local
`Qwen3-Coder-Next` and `devstral-small-2:24b` for a cleaner future test (F).

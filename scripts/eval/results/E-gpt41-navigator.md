# Data set E — capable external navigator (gpt-4.1)

- **Date:** 2026-06-26→27 (overnight batch)
- **Codebase:** `d25662d` (same as C & D). **Only variable vs C/D: the navigator model.**
- **Crew:** planner `nemotron-3-nano:30b` (unchanged) · **navigator `gpt-4.1`**
  (OpenAI; user-authorized external run) · triage `qwen2.5-coder:14b` (unchanged).

## Run
- Plan: **9 leaves**. Planner (unchanged) again mis-located `help_lines` to
  `newt-cli/src/crew.rs` (it is actually in `newt-tui/src/lib.rs`).
- Execution: **all 9 leaves [Done]**, chained, **"✓ plan complete."**
- **Wall-clock: 13558 s (~3.8 h)** — dominated by per-leaf `just check`
  (full `cargo test --workspace`), not gpt-4.1 inference. Zero API errors.

## Result — the most damning: a 5-language polyglot hallucination
Net change: `DgxHelp.cs (+29, C#)`, `cli.py (+13, Python)`, `help.py (+10, Python)`,
`integration/dgx_help_test.go (+28, Go)`, `src/cli/ModelsHelp.hpp (+27, C++)`,
`src/dgx_help.rs (+4)`, `src/help_section.rs (+4)`.

- **Five languages — C#, Python, Go, C++, Rust — in a Rust workspace.** A
  frontier-class model produced the *worst* mess of any run.
- **Loose files outside the build graph:** the `.rs` files are at a top-level
  `src/` that is not a crate; the others aren't Rust at all. Cargo compiles none
  of them → `just check` passes vacuously.
- **Real `help_lines` (newt-tui/src/lib.rs): 0 lines changed** — even the
  `refactor-help-lines-to-use-new-helpers` leaf never touched it (the planner
  pointed it at the wrong file).
- **Grade:** `top_dgx_subs=8, pass=false` — #548 NOT implemented.

## The decisive conclusion (E vs A–D)
E is the headline run: a genuinely capable model, same fixed grader. It **failed
too — worse than the weak local models.** Model capability does **not** correlate
with success here:

| run | executor | result |
|---|---|---|
| A | default crew | orphan Rust module |
| B | default crew | gutted README |
| C | qwen2.5-coder:14b | nothing (no-op) |
| D | qwen3.6:27b (local, general) | Python-in-Rust |
| E | **gpt-4.1 (frontier)** | **C#/Python/Go/C++ polyglot** |

**0 / 5 implement #548. The ceiling is the HARNESS, not the model.** The per-leaf
worker creates new vacuum files from abstract leaf text — wrong language, wrong
location — and never edits the real seam; isolated worktrees never cohere; and
`just check` can't tell inert files from a real implementation. No executor swap
fixes this. The levers are mechanism-level: (1) ground the worker in the real
repo (paths + language), (2) make leaves EDIT real seams, not create files,
(3) gate per-leaf on BEHAVIOR (the grader), not just compilation.

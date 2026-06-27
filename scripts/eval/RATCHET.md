# The Capability Ratchet — a (task × mode × model) matrix over the #548 behavioral grader

This generalizes the **autonomous #548 evaluator** (`METHODOLOGY.md`, `grade-548.sh`,
`grade-run.sh`) from one fixed cell into a **matrix**. #672 held the task fixed (#548) and
varied the codebase/model and found the loop is *mechanically robust but implements the issue
in 0/N runs — the bottleneck is crew-execution capability, not planning or context, and a
bigger general model made it worse.* The ratchet keeps #672's **behavioral-grading
methodology fixed** and sweeps three axes to locate *where* that execution ceiling sits and
*what breaks it*.

> **North-Star, inherited verbatim from `METHODOLOGY.md` §1:** "Pass" is **the feature works
> when you run the binary** — not "the loop finished" and not "`just check` is green". An
> orphan module that compiles + passes unit tests but is never wired in is a **FAIL**.

## The three axes

| Axis | Values (v1 → later) | Selected by |
|---|---|---|
| **Task** | `T0` basic → `T5` decomposition-heavy (a ladder of small, behaviorally-checkable cases) | a `newt-eval/cases/T*` dir |
| **Mode** (the ratchet rung) | `single` → `plan` → `plan+crew` | which real CLI the driver runs |
| **Model × specialization** | gnuc small → dgx **coder** vs **general** (the axis #672 surfaced) → OpenAI frontier | a **named** backend / `[crews.<name>]` in local `~/.newt` |

**Readout — the staircase:** per model, the lowest **mode** that flips behavioral pass at each
task rung; plus **mode-lift**, **coder-vs-general**, and **frontier** tables that test #672's
"execution is the ceiling" thesis directly.

## Modes → real commands (confirmed)

- **`single`** — one agent, one turn. Reuse `newt-eval run --case <T*> --model <m>` (already
  drives the ACP worker single-agent and grades structurally).
- **`plan`** — one model plans then executes autonomously:
  `newt plan --goal "<task prompt>" --one-shot --dir <throwaway> --max-leaves N`. The
  `--one-shot` flag *is* the headless approval (`grant_one_shot_authority` — fs read/write,
  worktree-bounded). The per-leaf crew uses a **single-model** `[crews.<name>]` roster
  (planner == navigator == triage == one model).
- **`plan+crew`** — identical command, but the active `[crews.<name>]` is a **mixed** roster
  (e.g. planner on a resident dgx coder model, navigator/triage on gnuc) — the `[crews.home]`
  shape #672 used. Plan vs crew differ **only** by the roster.

## Grading — two layers, behavioral is the headline

Per cell, after the run lands its `crew/*` branches in the throwaway:

1. **Behavioral (headline)** — does the feature actually work when run? For `T0–T4` (which
   edit a small seed Cargo project) the task's **own tests** are behavioral (they exercise the
   feature), so `cargo test` in the consolidated seed *is* the behavioral grade. For `T5`
   (a real wire-in to newt itself) a bespoke `grade-<task>.sh` drives the built `newt` and
   inspects real output — the `grade-548.sh` shape. `grade-run.sh` is the template:
   find the final `crew/*` branch → build → behaviorally grade → augment with run-shape.
2. **Structural (secondary)** — `newt-eval grade --case <T*> --workspace <final-worktree>`
   runs the `newt-eval` evaluators (`rust_compiles`, `tests_pass`, `pattern_match`, `diff_*`)
   on the post-run worktree. #672's core lesson: these are **necessary but not sufficient**
   (an orphan module passes them), so behavioral pass is the headline and structural is the
   diagnostic.

A cell's value = `behavioral-pass-rate · structural mean_score · leaves · files · tokens ·
wall-clock`, over **n ≥ 3–5 trials** (past #672's n=1 caveat), plus the noise-free
**deterministic regression row** (grade each task's seed binary, no LLM — calibration).

## Security invariant (inherited from #672)

**No home-network specifics in any committed file.** `ratchet.sh`, `RATCHET.md`, and every
`results/*.md` name **model identities** (`qwen2.5-coder:7b`, `gpt-4o`, `devstral-small-2:24b`),
**never hosts/IPs/GPUs**. Endpoints live only in the operator's **local** `~/.newt` config:

- `~/.newt/backends/<name>.toml` — one `[[backends]]` per cell (endpoint + model + `kind`;
  OpenAI via `api_key_file`). *Local, git-ignored, never committed.*
- `[crews.<name>]` — the per-role roster (model **names** only; hosts resolve via the named
  backends).

`ratchet.sh` takes **names** (`--backend gnuc-7b`, `--crew mixed`), never endpoints, and may
generate an **ephemeral** `$NEWT_CONFIG` at runtime (a temp file, not committed) by composing
the operator's local pieces. Placeholder templates (no real hosts) live in
`RATCHET.local.example`.

## The task ladder

| ID | Difficulty | Shape | Why this rung |
|---|---|---|---|
| `T0` | L1 | fix a bug so its failing test passes (1 file) | sanity floor — every model×mode passes — **authored** |
| `T1` | L2 | add error handling + a behavioral regression test (1 file) | multi-step single-file |
| `T2` | L3 | extract a module across 2–3 files, keep behavioral tests green | small-model single-agent starts failing |
| `T3` | L3 | implement a small feature from scratch (subcommand / state machine) + tests that run it | needs a plan |
| `T4` | L3 | a goal with 3–4 **dependent** subtasks (build X → wire Y → add tests) | where decomposition earns its keep |
| `T5` | — | a real wire-in to newt itself (#548-style) with a `grade-<task>.sh` oracle | catches "compiles but inert/orphan" |

`T0–T4` lean on behavioral **workspace tests** so the generic `tests_pass` carries the "it
works" signal; `T5` reuses #672's exact instrument.

## Running (spec for `ratchet.sh`)

```bash
# one cell
scripts/eval/ratchet.sh --task T0 --mode single --backend gnuc-3b --trials 3
# a mode sweep on one model (the ratchet, live)
scripts/eval/ratchet.sh --task T4 --modes single,plan,plan+crew --backend gnuc-7b --crew mixed
# the v1 gnuc matrix
scripts/eval/ratchet.sh --all --tier gnuc            # → results/ratchet-<date>.md + charts
```

## Status & build order

- **v1 (gnuc + dgx1-crew):** `single`/`plan` on gnuc small models; `plan+crew` on the mixed
  roster (planner on a resident dgx coder model). Locate the ceiling on the ladder; quantify
  mode-lift and coder-vs-general (replicating/refuting #672 run D at smaller scale).
- **B:** add dgx `single`/`plan` tiers (more named backends). **C:** add OpenAI
  (`gpt-4o`/`o4-mini`, `gpt-5-codex`) — the generalized #672 Run E across the whole ladder.
- **Coordination:** stacked on `eval/548-grader` (additive files only — `ratchet.sh`,
  `RATCHET.md`, `T*` tasks, the `newt-eval grade` subcommand); rebases onto main when #672
  merges. Heavy steps (cargo builds, dgx1/OpenAI live runs) wait until #672 merges so they
  don't contend with its in-flight runs.

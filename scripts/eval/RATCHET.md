# The Capability Ratchet — a (task × mode × model) matrix over the #548 behavioral grader

This generalizes the **autonomous #548 evaluator** (`METHODOLOGY.md`, `grade-548.sh`,
`grade-run.sh`) from one fixed cell into a **matrix**. #672's complete A–E experiment is the
premise: holding the task fixed (#548) and sweeping codebase + executor model from a 14B
coder to a 27B general local model to **frontier gpt-4.1**, the loop is *mechanically robust
but implements #548 in **0 of 5** — and **model capability does NOT correlate with success**:
the frontier model produced the **worst** result (a five-language polyglot hallucination).*
**The ceiling is the harness mechanism, not the model.** Root cause (precisely diagnosed in
`results/EXPERIMENT.md`): the planner mis-grounds the target file → the per-leaf worker
creates **new vacuum files** from abstract leaf text (wrong language, wrong location) instead
of **editing the real seam** → isolated worktrees never cohere → `just check` passes because
nothing it wrote is in the build graph.

So the ratchet is **not** a model-ranking benchmark. It keeps #672's **behavioral-grading
methodology fixed** and sweeps a matrix to do three things E makes urgent:

1. **Locate the mechanism's competence boundary** on a task-difficulty ladder (T0→T5): #548
   is one hard wire-in; where on the ladder does the autonomous loop start failing?
2. **Isolate whether decomposition helps or HURTS** — `single` (worker edits the given
   workspace directly) vs `plan+crew` (decompose → per-leaf vacuum files). E's diagnosis
   predicts **crew underperforms single on mid/hard tasks**, because decomposition is what
   introduces the mis-grounding + orphan-file failure mode.
3. **Measure the lift of the three mechanism levers** (the eventual payoff, per EXPERIMENT.md's
   bottom line): (a) a **behavioral per-leaf gate** (promote the grader into the crew verify
   so inert-vacuum leaves can't report success), (b) **ground the worker** in the real repo
   (target path + language → EDIT the seam, don't invent files), (c) **fix the planner's file
   grounding**. The ratchet is the instrument that makes "did the lever move `pass` off the
   floor?" measurable across the whole difficulty ladder, not just #548.

**Model is a CONTROL, not the lever** (E settled this): run the cheap local models for most
cells, reserve a frontier spot-check to re-confirm capability isn't the boundary. This also
saves OpenAI spend.

> **North-Star, inherited verbatim from `METHODOLOGY.md` §1:** "Pass" is **the feature works
> when you run the binary** — not "the loop finished" and not "`just check` is green". An
> orphan module that compiles + passes unit tests but is never wired in is a **FAIL**.

## The three axes

| Axis | Values (v1 → later) | Selected by |
|---|---|---|
| **Task** | `T0` basic → `T5` decomposition-heavy (a ladder of small, behaviorally-checkable cases) | a `newt-eval/cases/T*` dir |
| **Mode** (the ratchet rung) | `single` → `plan` → `plan+crew` | which real CLI the driver runs |
| **Mechanism variant** (the eventual primary axis) | `baseline` → `+worker-grounding` → `+behavioral-leaf-gate` → `+planner-grounding` | a newt feature flag / config (downstream; the levers must be built first) |
| **Model** (a CONTROL, per E) | mostly gnuc small; a frontier spot-check | a **named** backend / `[crews.<name>]` in local `~/.newt` |

**Readout — the staircase:** per (mode, mechanism), the highest task rung that still
behaviorally passes — the mechanism's **competence boundary**. The headline tables:
(a) **single-vs-crew** at each rung (does decomposition help or hurt? — E predicts hurt);
(b) **lever-lift** (does `+grounding` / `+behavioral-gate` push the boundary up the ladder, or
move #548-class `top_dgx_subs` off 8?); (c) a **frontier spot-check** confirming model strength
does NOT move the boundary (re-validating E so we can stay on cheap models).

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

- **v1 — the instrument + the baseline boundary (gnuc, cheap).** `single` vs `plan` vs
  `plan+crew` across T0→T5 on cheap gnuc small models (model held as a control), mixed crew
  planner on a resident dgx coder model. **The v1 deliverable is a single-vs-crew competence
  staircase**: where does the autonomous mechanism stop implementing, and does crew underperform
  single (testing E's prediction at smaller scale). No OpenAI needed for v1.
- **v2 — measure the levers (the payoff).** Build the three mechanism fixes (worker grounding,
  behavioral per-leaf gate, planner file-grounding) behind flags and re-run the matrix; watch
  the boundary move up the ladder / `top_dgx_subs` come off 8. This is the actionable
  contribution E points to (mechanism, not model).
- **Frontier spot-check (minimal spend).** One hard-rung cell on `gpt-4o`/`gpt-5-codex` to
  re-confirm E (capability isn't the boundary) — not a main axis.
- **Coordination:** stacked on `eval/548-grader` (additive files only — `ratchet.sh`,
  `RATCHET.md`, `T*` tasks, the `newt-eval grade` subcommand); rebases onto main when #672
  merges. Heavy steps (cargo builds, dgx1/OpenAI live runs) wait until #672 merges so they
  don't contend with its in-flight runs.

## The finding

The synthesized write-up of what this apparatus has found so far —
"The Ceiling Is the Harness: Gaming the Gate and Structurally-Enforced TDD" —
lives at [`docs/design/the-ceiling-is-the-harness.md`](../../docs/design/the-ceiling-is-the-harness.md).
It separates what is SHOWN (#672 A–E), SUGGESTED (the T0 observation), and
PROPOSED (structurally-enforced TDD: the ungameable grade + the OCAP-locked
adversarial Referee role).

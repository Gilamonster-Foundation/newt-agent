# Phase 3 (the untested root-cause lever): does a stronger PLANNER ground?

Phase 1 located the failure (the crew games / mis-grounds). Phase 2 built the
anti-gaming lever (the locked behavioral gate) and measured it lifting T2. But the
#674 review flagged that the whole program — like #672 — had only ever varied the
**executor**, while the diagnosed root cause is **planner mis-grounding**. The
strong-*planner* cell was never run. Phase 3 runs it.

## The instrument: T3, a mis-grounding rung

T0–T2 don't isolate grounding (T2's mis-grounding wasn't fatal; the locked gate
alone saved it). **T3-format-temperature** is built so *grounding is the
bottleneck*:

- The real seam — a buggy `format_temperature` (truncates instead of rounding to
  one decimal) — is buried in `src/units/temperature.rs`.
- A **same-vocabulary decoy** — `format_temp` (already correct, whole-degree) —
  sits in the *obvious* file `src/format.rs`.
- The prompt names the symbol and the behavior ("temperature formatting"),
  vocabulary that matches **both** files.

A planner that grounds on the obvious filename + a fuzzy symbol match edits the
decoy, fixes nothing, and — even with the locked gate — never makes the real test
pass. A planner that traces the failing test to the real seam succeeds.

Validated offline: seed FAILS the external spec (`21°C` ≠ `21.1°C`); golden PASSES
all four values; `mock_e2e` + `case_prompt_lint` green.

## The variable: the plan-authoring model

newt hardwires the plan author to `cfg.backends.first().model` — the step that
decides *which file each leaf targets*. So the grounding model is a config swap,
everything else held identical (same task, same locked gate, same `--one-shot`):

- **weak** = `llama3.1:8b`
- **strong** = `qwen3-coder:30b`

> **Correction (host attribution).** Both models ran on **gnuc's local ollama**
> (`127.0.0.1:11434`). An earlier draft labelled the strong model "dgx" — that was
> wrong: the `~/.newt` `[dgx]` node is mis-pointed at `localhost`, so the "30B on
> dgx" actually loaded a **19 GB model onto the 30 GB gnuc**. This does NOT weaken
> the result — it makes the comparison *cleaner* (weak and strong ran on the **same
> box**, so the only variable is the model). It does explain the delivery failures
> below: they were gnuc thrashing on a local 19 GB model, not a remote machine. The
> real dgx1 (`REDACTED-IP`, with 120B / Qwen3-Coder-Next) was never used.

This is the cleanest possible isolation: **grounding is decided at plan-authoring,
upstream of the executor entirely.** The contrast appears in the *authored plan*,
before a single line is edited — no executor confound; and since both models ran on
the same local ollama, host is not a confound either.

## Result (n=1): the grounding contrast

| Plan author | Where the plan says `format_temperature` lives | Decoy (`format.rs`) | Grounding |
|-------------|-----------------------------------------------|---------------------|-----------|
| **weak** `llama3.1:8b` | a leaf targets **`src/format.rs:9`** (the decoy) | **mis-attributed** | ❌ **MIS-GROUNDED** |
| **strong** `qwen3-coder:30b` | **every** leaf targets `src/units/temperature.rs` | **untouched** | ✅ **GROUNDED** |

Verbatim from the authored plans:

- **weak**: *"update-format_temperature-use — Update the implementation of
  `format_temperature` in **`src/format.rs:9`** …"* — it places the function in the
  decoy file.
- **strong**: *"Locate `format_temperature` in `src/units/temperature.rs`"* →
  *"Modify `format_temperature` in `src/units/temperature.rs` … using proper
  rounding"* — it never mentions the decoy.

**This is the #674-predicted lift, at the cleanest level: move capability to the
grounding role and the mis-grounding disappears.** The weak planner put the
function in the wrong file; the strong planner found the real seam.

## A cross-cutting note: over-decomposition is a HARNESS artifact, not a model one

**Both** planners decomposed a one-line fix into 6 leaves (the weak plan even
included *"research how to control decimal precision in Rust"*). That is the
plan-authoring prompt pushing decomposition, and it is **model-independent**:
`qwen3-coder:30b` over-decomposed too. It is also what made the weak arm time out
(6 mis-grounded leaves × per-leaf full `cargo test` × retries against a gate that
never went green). So strong capability fixed the **grounding** but not the
**over-decomposition** — the latter is a *separate* harness lever (a planner that
right-sizes the leaf count to the task), distinct from the grounding lever this
cell isolates.

## Delivery grade (end-to-end): infrastructure-blocked, not experiment-blocked

The grounding contrast above is the root-cause result and comes from plan
*authoring*, which succeeded on both backends — so it is independent of everything
below. The confirmatory end-to-end grade (does the strong plan *execute* to a
locked-gate PASS?) could **not** be obtained, across four runs, and the reason is
infrastructure degradation, not the experiment:

| Run | Cap | Authored plan | Why no delivery |
|-----|-----|---------------|-----------------|
| weak ML6 | 6 | mis-grounds (decoy `format.rs:9`) + 6 leaves | over-decompose → timeout |
| strong ML8 | 8 | grounds (all 8 → real seam) + 8 leaves | over-decompose → timeout |
| **strong ML2** | 2 | **grounds — tight 2-leaf, names the `{:.1}` fix** | `qwen3-coder:30b` **hung at execution** — it ran on gnuc's LOCAL ollama (the `[dgx]` node points at `localhost`), so loading the **19 GB model on the 30 GB gnuc** thrashed the box (an earlier 1-token probe took 213 s under that pressure) |
| **weak ML2** | 2 | (authoring) | gnuc ollama **`500: timed out waiting for llama runner to start - progress 0.90`** — couldn't load the model runner under memory pressure (252 MiB free) |

So the leaf count tracks the cap exactly (6→6, 8→8, 2→2): the planner fills the
budget, confirming over-decomposition is a *harness* lever, model-independent.
And at a tight cap the strong planner authored a **flawless** plan — correct seam,
correct fix named — yet **no fix landed**. The cause was a single one:
**`~/.newt`'s `[dgx]` node is mis-pointed at `localhost`**, so *both* arms ran on
the same gnuc-local ollama — the weak model OOM'd the runner, and the strong 19 GB
model thrashed a box that can't hold it. The seed file came out unchanged; no
`crew/*` branch was produced. (The fix: point `[dgx]` at the real dgx1
`REDACTED-IP`, which has the headroom for a 30B; that both unblocks this delivery
grade *and* stops the recurring gnuc memory pressure.)

**Honest status:** the *grounding lift* is established (authoring-level, n≥2 for the
strong arm including a plan that names the exact fix). The *delivery* lift —
strong-grounded plan → locked-gate PASS — is **pending a re-run on the real dgx1**
(point `[dgx]` at `REDACTED-IP` so the 30B has room to execute); it is a re-run, not a redesign. The
strong ML2 plan is strong evidence it would pass (it targets the file that, when
fixed with `{:.1}`, passes `grade_spec` — verified offline), but "would pass" is
not "did pass," and we mark it pending rather than claim it.

## Caveats

n=1 per arm. Both arms over-decompose (harness artifact). The strong arm here
swaps the *whole* pipeline to `qwen3-coder:30b` (author + executor); because the
fix is a trivial one-liner and the grounding difference shows up in the *plan*, the
discriminating variable is demonstrably grounding, not execution — but a
planner-only isolation (strong author + pinned cheap executor) is the tighter
follow-up. The full strong-**orchestrator** architecture (#674: strong model reads
the repo, grounds, decomposes, runs the gate; cheap model executes each leaf) is
the larger build this cell motivates.

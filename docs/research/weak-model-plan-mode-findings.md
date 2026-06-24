# Weak-model tool-loop findings + refined plan-mode design

Live A/B investigation (dgx1, latest main + an env-gated experimental re-seat
arm) of the question: **would a better plan mode help weaker models?** — rooted
in the practical observation that *small models make a decent plan, then lose
track of it over time.* Date: 2026-06-24. Runner/rig: `docs/testing/uat-tool-loop.md`
+ `/tmp/plan-reseat-ab/` (experimental; not committed).

## Headline findings

1. **The #1 weak-model failure is tool-call FORMAT, not plan retention.** The
   smallest coders never reach the plan-retention question because they cannot
   emit a native tool call at all: `qwen2.5-coder:3b/14b` and `codestral:22b`
   text-dump the call as fenced-JSON or do nothing → `tool_calls=0` → **nothing
   executes**. Even `qwen3-coder:30b` nondeterministically emitted a
   `<function=plan_set>…</function>` XML form that the harness didn't parse →
   **0 steps, 2 of 6 runs**. Format failure killed more runs than anything else.
   → **P0: a tool-call-recovery handler** (parse fenced-JSON and `<function=>`
   forms out of content and re-dispatch them) is the highest-leverage weak-model
   fix — above any plan work.

2. **Re-seating the live plan FIXES drift where drift exists — and only adds
   mild overhead where it doesn't.** One env-gated arm, two task sizes
   (`devstral-small-2:24b`, N=3):
   - *8-step (no drift):* baseline **8/8 every run (13–21 rounds)**; re-seat
     **7/8 and 24-round cap every run** — mild overhead; the per-round reminder
     makes it churn with no deficit to close.
   - *12-step (induces drift):* baseline **8.00/12 (runs 6, 6, 12)** — drops half
     the later steps in 2 of 3 runs (the user's "loses track"); re-seat
     **12.00/12 (12, 12, 12)** — every step, every run, **zero variance**.
   → **P1: re-seat is VALIDATED for the drift regime (the user's actual
   problem); make it CONDITIONAL** (gate on a drift signal / re-seat on
   plan-mutation) so the big win on hard tasks doesn't cost overhead on easy ones.

3. **A capable model adapts; the drift only emerges at higher task complexity.**
   The 8-step task was too easy to reproduce the user's "loses track" (baseline
   aced it). The harder **12-step** fair-test induces baseline drift (mean
   **8.00/12**, runs 6/6/12) and **re-seat closes it completely (12.00/12, all
   runs)** — re-seat's value is real but *conditional on drift existing*.
   Concluding "re-seat doesn't
   help" from a task the baseline already aces would be wrong; the fair-test is
   built to give re-seat a deficit to close.

4. **Tool calls ran against the dead STUB shell.** On the default/release build
   `run_command` returns *"temporarily unavailable in this build"* (the
   crates.io-safe stub) yet is still advertised — so every shell-dependent result
   here reflects the stub, not a real shell. **Real-shell testing requires
   `just install-real`** (swaps in the real brush OCAP shell) until the upstream
   brush `CommandInterceptor` (reubeno/brush#1184) is accepted. The test suites
   should `just install-real` first; results above are stub-shell results.

## The weak-model failure taxonomy (two tiers)

| Tier | Models (observed) | Failure | Fix |
|---|---|---|---|
| **1 — pre-execution** | `qwen2.5-coder:3b/14b`, `codestral:22b` | can't emit a native tool call → never execute | **tool-call-recovery parser** (plan work is moot for them) |
| **2 — execution drift** | `devstral-2:24b`, `qwen3:30b`, `granite4.1:30b`, `qwen3-coder:30b` | execute fine; "loses track" only on complex/long tasks | **conditional re-seat** + the **overseer/crew split** |

## The A/B (method + data)

Same binary, env-gated arm (`NEWT_RESEAT_PLAN=1` re-injects the live `<plan>`/
`<state>` every round, Ollama loop). Metric: steps-completed (the loses-track
proxy — later steps drop when the model forgets).

**8-step task (mean steps / 8, N=3):**

| model | baseline | re-seat |
|---|---|---|
| devstral-small-2:24b | **8.00** (13–21 rounds) | 7.00 (24-round cap every run) |
| qwen3-coder:30b | 6.67 (one 4/8) | (one valid 8/8; 2 runs lost to `<function=>` format failure at round 0) |

**12-step fair-test (devstral, the drift regime), mean steps / 12, N=3:**

| arm | runs | mean |
|---|---|---|
| baseline | 6, 6, 12 | **8.00** (drifts 2/3) |
| re-seat | 12, 12, 12 | **12.00** (perfect, zero variance) |

Re-seat completed every step in every run where the baseline dropped half the
later steps — the keystone fixes the user's "loses track" exactly when it occurs.

## Rig methodology (hardened — three confounds defeated)

The experiment was wrong three times before it was right; the fixes are the rig:

1. **`pkill -f <proxy/newt>` self-kills the shell** (the pattern matches the
   runner's own command line → exit 144, empty output). Kill by **PID**; clear
   ports with **`fuser -k <port>/tcp`**.
2. **Tool-call format** — non-tool-calling models score 0 for an unrelated
   reason. **Gate the model set** to confirmed tool-callers (or add the recovery
   parser) before measuring anything downstream.
3. **`newt code` reads stdin line-by-line** — a multi-line task is drip-fed as
   separate turns. **Tasks must be a single line.**
4. **Stub vs real shell** — `just install-real` for any shell-dependent test.

## Refined plan-mode design (priority-ordered by evidence)

- **P0 — tool-call-format recovery handler + hallucination tracker.** Recover
  fenced-JSON and `<function=>` tool calls from content; count/track
  non-recoverable emissions as hallucinations. This is the evidence-#1 fix and
  the next code task. Re-enters the existing `resolve_tool_alias`/`is_hallucination`
  path (`tools.rs:274/304`).
- **P1 — conditional re-seat** (on plan mutation / active-step delta, *not* every
  round). **VALIDATED under drift** — 12-step fair-test: re-seat **12/12** vs
  baseline **8/12** (drifts 2/3). Keep it, **gated on a drift signal** (or
  re-seat on plan-mutation) so easy tasks skip the mild overhead it costs there.
- **P2 — the overseer/crew split is the safe weak-model story** (a stronger seat
  authors the DAG; the weak model executes one bounded leaf, with templated tool
  calls that sidestep the format risk). Untouched by this data; still the most
  defensible path.
- **Unchanged & sound:** the Plan-DAG + cursor + *harness-tracks-progress*
  structure; `subtask == CrewTask` (runner-agnostic for crew/distributed).

## Next

- Implement the **tool-call handler + hallucination tracker** (P0).
- Wire **`just install-real`** into the test suites (real-shell tests) until
  reubeno/brush#1184 lands.
- Append the 12-step fair-test re-seat verdict here when it completes.

# Tool-loop UAT — latest harness @ `baba2e7` (Phase 27 in), dgx1

First run of the [live behavioral tool-loop UAT](../uat-tool-loop.md) against the
latest harness (all of Phase 27 landed: 27.1–27.6). **Verdict: the harness is
solid on real coding tasks and the Phase-27 machinery is active.** The two
notable behaviors were *model-workflow/judgment* gaps that **vanished with a
sharper prompt** — not harness regressions; the two loop *guards* (27.3/27.5)
could not be triggered live by a capable model (by design — they catch *weak*
models that loop, which the mocked `uat_thrash` test covers).

- **Harness:** `main` @ `baba2e7`, release build (`CARGO_TARGET_DIR=/tmp/.cargo-target`).
- **Inference:** dgx1 (`dgx1.home.lab:11434`) / `qwen3-coder:30b`. (gnuc-ollama was
  *not* used — it flakes under multi-turn load.)
- **Runner:** `docs/testing/scripts/uat_tool_loop.sh`. Date: 2026-06-24.

## Capability / correctness (S1–S5)

| # | Scenario | Outcome | Notes |
|---|---|---|---|
| S1 | rename function across file | ✅ **pass** | 0 old refs, 3 new, runs correctly (10 rounds) |
| S2 | branch + add constant | ⚠ **partial** | branch *created* (27.2 works) but committed on `master` — no `checkout` call (5 rounds) |
| S3 | fix bug + add test | ✅ **pass** | `apply_discount(100,10)=90.0`, `test_discount.py` created, **"Test passed!"** (20 rounds) |
| S4 | refactor god function → 3 helpers | ✅ **pass** | 4 defs, output **byte-identical** to golden (16 rounds) |
| S5 | "make app.py run" (missing module) | ⚠ **fabricated** | invented `payments_gateway.py` stub (transparent, but fabricate-to-satisfy) |

## Adversarial + sharpened (S6, S7, S2′, S5′)

| # | Scenario | Result |
|---|---|---|
| S6 | force repeated dead `run_command` | **No runaway** — `run_command` called **2×** across 11 rounds; the model read the "unavailable in this build" stub message and **adapted both times** ("I need to find another way"). 27.3's short-circuit had nothing to fire on — a capable model never re-issues the identical call. |
| S7 | dead-shell churn, `max_tool_rounds = 6` | **Cap not reached** — the model abandoned the hopeless shell task early rather than churning. 27.5's honest cap-exit not triggered live (it needs a model that thrashes to the cap). |
| S2′ | *sharper*: "create branch, **switch to it**, … commit **ON the branch**" | ✅ **fixed** — current branch = `feature/timeout-config`; commit *"Add DEFAULT_TIMEOUT configuration"* on the branch. The S2 workflow gap was **promptable**. |
| S5′ | *sharper*: "diagnose in words, **do NOT create files**" | ✅ **fixed** — honest diagnosis ("the module is not available in the Python path … module resolution error"), **no file fabricated**. The S5 fabrication was **promptable**. |

## Phase-27 machinery observed

- **27.1 corrective feedback — ACTIVE.** S1 and S4 each showed **"⚠ 3 hallucination(s) corrected"**; the model recovered and finished. (Tool-name *alias* resolution wasn't needed — qwen used valid names.)
- **27.2 git checkout/branch — WORKS end-to-end** (S2′: branch + switch + commit-on-branch).
- **27.4 plan ledger (default-on local) — ACTIVE & used** (`plan_set`/`plan_advance` fired).
- **27.3 dup-guard / 27.5 honest cap-exit — not exercised live.** A capable model adapts (S6) or gives up (S7) before triggering them; they are safety nets for *weak* models, and the deterministic guarantee is the mocked `uat_thrash_run_gets_honest_cap_exit_not_raise_the_limit` test.

## Honest caveats

- Single live runs are **noisy**; these are behavioral observations, not statistics.
- The **correction to an interim note**: an earlier "dup-guard fired 4×" was a
  false grep — model prose *"identical functionality"*, not the guard. The guard
  was **not** observed firing live (see above).
- `run_command` is the **dead stub** on this build yet still advertised — S6/S7
  are really testing dead-tool handling, the exact original failure class.
- **Logic vs behavior:** the feature/loop *logic* is pinned by unit tests + the
  mocked `tool_round_cap_tests`; this suite validates *live behavior*. Cite both.

# Held-out Terminal-Bench measurement set (#2318)

The #2318 closure report compares baseline and treatment on a **held-out** task set: tasks no treatment was tuned or debugged on. This file declares that set and the rule that produces it. It also declares the development sets, which may be used for tuning and debugging.

## Development sets (tuning-exposed)

| set | file | why it is exposed |
|---|---|---|
| tb-30 | [`tb-30.json`](tb-30.json) | the release-floor ratchet newt has been tuned against since July 2026 |
| smart-ab-12 / smart-ab-8 | [`smart-ab-12.json`](smart-ab-12.json), [`smart-ab-8.json`](smart-ab-8.json) | smart-ab-12 was drawn for the smart-harness experiment (#2260/#2263). Its 8 oracle-passing tasks (smart-ab-8) ran in the pilots and in the 2026-09-14 three-harness baseline. The other 4 saw only the oracle, apart from `build-cython-ext`, which is also in tb-30. All 12 are excluded as a precaution. |

Treatments may be developed, debugged and compared on these sets. **A result on them is a development result, not uplift evidence.**

## The rule

1. **Pool.** Every task in terminal-bench (89), minus every task in a development set (37), leaves **52 tasks**. The list is in [`tb-heldout-pool.json`](tb-heldout-pool.json), produced by `tb_heldout.py pool <dataset> tb-30.json smart-ab-12.json`. smart-ab-8 is a subset of smart-ab-12.
2. **Oracle validation.** Run the reference solution (`--agent oracle`) on all 52 tasks. Drop every task it does not pass, recording the verifier's reason as `smart-ab-8.EXCLUDED.md` does (the P0a rule: a task the reference cannot pass measures nothing). **Pending:** this runs after the 2026-09-14 baseline finishes, so its docker, disk and CPU load does not confound the baseline's timings.
3. **Draw.** Run `tb_heldout.py draw tb-heldout-pool.json <size> <seed> <oracle job dir>`. It makes a seeded draw from the validated pool, stratified by each task's declared `[metadata] difficulty`, with largest-remainder allocation. The output is `tb-heldout.json`, whose sha256 becomes the campaign's `task_set_sha256`.
4. **Freeze.** The set is never resampled. Backfill happens only by oracle-validating a replacement, and the replacement is recorded here.

**Declared before any model runs on these tasks:** size **24**, seed **2318**.

## Exposure check (measured 2026-09-14)

- **No Harbor or terminal-bench run artifacts exist for any of the 52 pool tasks** under any local bench job root: every `/var/tmp/tbench*` directory except the dataset itself.
- A word-bounded search of the knowledge board, this repo's `scripts/` and `docs/`, and gilamonster-bench found two mentions. `polyglot-rust-c` appears in the 2026-09-08 P0a apparatus note, as an **oracle** (reference-solution) run, which involves no model. `filter-js-from-html` is a substring of the tb-30 task `break-filter-js-from-html`.
- **Limit:** this check covers what is on this machine and in these notes. It cannot see runs made elsewhere.

## Statement

**No treatment is tuned, debugged or selected using any held-out task.** That includes tuning thresholds, prompts, treatment settings, model choice or task choice on results from these tasks. Everything in this file was decided from task metadata and development-set results only. If a held-out task is used for development, it moves to a development set, and the held-out set is redrawn from what remains.

## Difficulty spread (declared metadata, not measured)

| difficulty | terminal-bench | development sets | held-out pool |
|---|---|---|---|
| easy | 4 | 4 | **0** |
| medium | 55 | 22 | 33 |
| hard | 30 | 11 | 19 |

**The pool has no easy tasks**, because all four are in development sets. A 24-task draw from the full pool allocates 15 medium and 9 hard; the oracle step can change the allocation.

## Expected floor (development-set evidence only)

Nothing below uses a result on a held-out task.

- **tb-30, newt on qwen3-coder_30b, July 2026** (one attempt per task; older newt builds; 65536 served window): 3/30 and 4/30 resolved.
  - By declared difficulty: easy 0/4 and 0/4, medium 2/19 and 3/19, hard 1/7 and 1/7.
  - Medium + hard: **3/26 and 4/26 (12–15%)**.
- **smart-ab-8, the 2026-09-14 baseline, newt on qwen3-coder_30b, as of 18:08 EDT:**
  - 16 of 24 trials finished, **0 resolved**: easy 0/4, medium 0/6, hard 0/4 graded, and 2 more hard trials that ended in agent-caused errors (reward 0).
- **Derived.** On a medium/hard held-out set, newt on this model should resolve somewhere between **about 0% and 15%** of trials at baseline. On 72 trials (24 tasks × 3) that is roughly 0–11 resolved. A floor that low leaves little room to detect a modest uplift, and a baseline at 0 cannot show one at all (the paired report flags that case as a floor).

## Decision for the maintainer (before GPU-days are spent)

- **Keep qwen3-coder_30b.** The floor risk is as above. This is the cheapest choice, and it is the model where the uncapped-output confound was observed.
- **Choose a stronger model, using development-set evidence only.** On tb-30 in July/August: ornith-1.0-35b-q8 11/30, nemotron-3-super 11/30 (off lane) and 8/30 (on lane), deepseek-v4-pro 17/30 and 15/30 (not served by the local router). The router currently serves ornith-1.5-35b, which has no tb-30 row. It would need a development-set run first; running it on held-out tasks to choose it would be tuning on them.
- **Change the task choice.** Admitting easier tasks is impossible without breaking held-out status, because all easy tasks are exposed. A different suite would need its own declaration.

# Sweep Analysis: 2026-07-01-pr802-baseline

- **Sweep dir:** `scripts/eval/results/sweeps/2026-07-01-pr802-baseline`
- **Git SHA:** `30b2120`
- **Graded rows:** 100
- **Status:** DONE=true (sweep complete)

## Summary

Both single-mode rungs are cleanly supported: 010-decompose-god-function/single and T2-humanize-duration/single each pooled 25/25 (Wilson 95% [87%, 100%]), with every constituent cell at 5/5. Crew mode is measurably behind at the pooled-rung level: 010-decompose-god-function/crew pooled 14/25 ([37%, 73%]) and T2-humanize-duration/crew pooled 19/25 ([57%, 89%]) — the 010/crew interval does not overlap its single counterpart's [87%, 100%], so the crew deficit on that rung is statistically supported; the T2 crew-vs-single gap is suggestive but the intervals overlap. At the per-cell level, every cell is n=5 with wide Wilson intervals (a 100% cell still spans [57%, 100%]), so individual model rankings within a mode are not statistically supported — only the extremes (0/5 deepseek-r1:70b crew vs. 5/5 cells) clearly separate. No PASS?gameable results occurred anywhere (gameable = 0 in every cell and rung), and no cell carries an UNDERPOWERED or GAMEABLE-RUNG flag. Qualitatively, all five inspected FAIL logs ended with `✓ plan complete` — the crew runner believed it succeeded while the grader failed the artifact — consistent with the known crew failure signature (silent bad output, not crashes).

## Per-cell results

| task | mode | model | pass | gameable | fail | n | pass-rate | Wilson 95% | flags |
|---|---|---|---|---|---|---|---|---|---|
| 010-decompose-god-function | crew | deepseek-r1:70b | 0 | 0 | 5 | 5 | 0% | [0%, 43%] |  |
| 010-decompose-god-function | crew | devstral-small-2:24b | 1 | 0 | 4 | 5 | 20% | [4%, 62%] |  |
| 010-decompose-god-function | crew | qwen2.5-coder:14b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| 010-decompose-god-function | crew | qwen2.5-coder:32b | 3 | 0 | 2 | 5 | 60% | [23%, 88%] |  |
| 010-decompose-god-function | crew | qwen3-coder:30b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| 010-decompose-god-function | single | deepseek-r1:70b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| 010-decompose-god-function | single | devstral-small-2:24b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| 010-decompose-god-function | single | qwen2.5-coder:14b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| 010-decompose-god-function | single | qwen2.5-coder:32b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| 010-decompose-god-function | single | qwen3-coder:30b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| T2-humanize-duration | crew | deepseek-r1:70b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| T2-humanize-duration | crew | devstral-small-2:24b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| T2-humanize-duration | crew | qwen2.5-coder:14b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| T2-humanize-duration | crew | qwen2.5-coder:32b | 1 | 0 | 4 | 5 | 20% | [4%, 62%] |  |
| T2-humanize-duration | crew | qwen3-coder:30b | 3 | 0 | 2 | 5 | 60% | [23%, 88%] |  |
| T2-humanize-duration | single | deepseek-r1:70b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| T2-humanize-duration | single | devstral-small-2:24b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| T2-humanize-duration | single | qwen2.5-coder:14b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| T2-humanize-duration | single | qwen2.5-coder:32b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |
| T2-humanize-duration | single | qwen3-coder:30b | 5 | 0 | 0 | 5 | 100% | [57%, 100%] |  |

Notes on reading the cells:

- The crew failures are concentrated, not diffuse: on 010-decompose-god-function/crew, deepseek-r1:70b (0/5, Wilson [0%, 43%]) and devstral-small-2:24b (1/5, [4%, 62%]) account for most of the rung's deficit, while qwen2.5-coder:14b and qwen3-coder:30b are 5/5 on the same rung. On T2-humanize-duration/crew the weak cells shift to qwen2.5-coder:32b (1/5, [4%, 62%]) and qwen3-coder:30b (3/5, [23%, 88%]) — the crew-fragile model is task-dependent, and at n=5 per cell that flip could be partly noise.
- Every single-mode cell is 5/5 (100%, [57%, 100%]).
- No cell carries the UNDERPOWERED flag, so no per-cell claim here requires that stamp; nonetheless all cells are n=5, so per-cell point estimates should be treated as coarse (see Caveats).

## Per-rung pooled

| rung (task/mode) | pass/n (PASS-prefix pooled) | of which gameable | Wilson 95% |
|---|---|---|---|
| 010-decompose-god-function/crew | 14/25 | 0 | [37%, 73%] |
| 010-decompose-god-function/single | 25/25 | 0 | [87%, 100%] |
| T2-humanize-duration/crew | 19/25 | 0 | [57%, 89%] |
| T2-humanize-duration/single | 25/25 | 0 | [87%, 100%] |

- **Statistically supported:** 010-decompose-god-function/crew (14/25, [37%, 73%]) vs. 010-decompose-god-function/single (25/25, [87%, 100%]) — non-overlapping intervals; crew mode genuinely underperforms single mode on this rung at this sha.
- **Suggestive only:** T2-humanize-duration/crew (19/25, [57%, 89%]) vs. T2-humanize-duration/single (25/25, [87%, 100%]) — the intervals overlap at [87%, 89%], so this gap is directionally consistent with the 010 result but not independently conclusive.
- **Gameable column is 0 on every rung.** No PASS?gameable outcome exists in this sweep, so no pass claimed above rests on a gameable grade. No rung carries a GAMEABLE-RUNG flag; if any future sweep flags one, no claim about that rung is trustworthy until a hidden `grade_spec.rs` exists for it — see `/grade-spec-author`.

### Qualitative color from FAIL run logs

All five FAIL run dirs made available for inspection show the same signature: the crew runner completed its authored plan and reported success, yet the run graded FAIL. Four are devstral-small-2:24b on 010-decompose-god-function (matching that cell's 4 fails); one is qwen3-coder:30b on T2-humanize-duration.

- Every one of the five logs ends: `✓ plan complete` / `run-log → plan.run.toml` — no crashes, no error output. The failures are silent-bad-artifact failures, invisible to the runner's own exit status.
- The 010 plans decompose the trivial extract-three-helpers task into four dispatched leaves, e.g. from `tmp.G7guqtaSCn`: "4 subtask(s); 4 leaf/leaves to dispatch: • extract-count … • extract-sum … • extract-max … • rewrite-summarize — Rewrite the `summarize` function to call the three helper functions … (after extract-count, extract-sum, extract-max)". Each leaf then reports `[Done]`, so the failure is presumably in how independently-authored leaf edits compose — not in any leaf refusing or erroring.
- The T2 fail (`tmp.kFDGwziEa4`, qwen3-coder:30b) planned a leaf "validate-fix — Run the specific failing test to verify that 90 seconds now produces \"1m 30s\"" even though the same log states "exec + net stay denied (verify runs via the runner's trusted command)" — the plan assigned itself a validation step it had no authority to execute, then marked it `[Done]` anyway.

These are pointers for autopsy, not graded statistics; the counts above come only from the tables.

## Failure pointers

FAIL run dirs available for `/crew-autopsy` (base: `/var/tmp/newt-sweeps/2026-07-01-pr802-baseline-3613481782/`):

- `/var/tmp/newt-sweeps/2026-07-01-pr802-baseline-3613481782/tmp.G7guqtaSCn` — FAIL; 010-decompose-god-function / crew / devstral-small-2:24b; log ends `✓ plan complete`
- `/var/tmp/newt-sweeps/2026-07-01-pr802-baseline-3613481782/tmp.nIQGaAGMqf` — FAIL; 010-decompose-god-function / crew / devstral-small-2:24b; log ends `✓ plan complete`
- `/var/tmp/newt-sweeps/2026-07-01-pr802-baseline-3613481782/tmp.sjHFd5AvP4` — FAIL; 010-decompose-god-function / crew / devstral-small-2:24b; log ends `✓ plan complete`
- `/var/tmp/newt-sweeps/2026-07-01-pr802-baseline-3613481782/tmp.FszgDeoinB` — FAIL; 010-decompose-god-function / crew / devstral-small-2:24b; log ends `✓ plan complete`
- `/var/tmp/newt-sweeps/2026-07-01-pr802-baseline-3613481782/tmp.kFDGwziEa4` — FAIL; T2-humanize-duration / crew / qwen3-coder:30b; log ends `✓ plan complete`

Gameable run dirs: **none** (gameable = 0 in every cell and every pooled rung).

Note: the per-cell table records more FAILs than the five dirs listed above (e.g., 5 fails for 010/crew/deepseek-r1:70b, 2 for 010/crew/qwen2.5-coder:32b, 4 for T2/crew/qwen2.5-coder:32b, 2 for T2/crew/qwen3-coder:30b); only these five run dirs were provided for inspection in this report. The remaining FAIL run dirs live under the same sweep tmp base.

## Caveats

- **Small n per cell.** Every cell is n=5. No cell carries the UNDERPOWERED flag in this sweep, and no cell has n<5, but n=5 Wilson intervals are wide — a 5/5 cell still only supports [57%, 100%], and a 0/5 cell supports [0%, 43%]. Do not rank models on per-cell point estimates; only pooled-rung comparisons (n=25) carry weight here, and even then only the 010 crew-vs-single split has non-overlapping intervals.
- **Which-model-is-crew-fragile is not settled.** The weak crew cells differ by task (deepseek-r1:70b + devstral-small-2:24b on 010; qwen2.5-coder:32b + qwen3-coder:30b on T2). At n=5 per cell this pattern flip is within noise; a larger re-sweep on the specific weak cells is needed before attributing fragility to particular models.
- **Gameable / GAMEABLE-RUNG.** No PASS?gameable outcomes occurred (gameable = 0 everywhere), and no cell or rung carries the GAMEABLE-RUNG flag, so no claim in this report rests on a gameable grade. Standing rule regardless: a PASS?gameable is never a pass, and any GAMEABLE-RUNG cell requires a hidden `grade_spec.rs` (via `/grade-spec-author`) before any claim about it is trustworthy.
- **Sweep status.** DONE=true with 100 graded rows — this is a complete sweep, not a partial one. Results are pinned to git sha `30b2120`; conclusions apply to that tree only.
- **Runner self-report is unreliable for crew FAILs.** All five inspected FAIL logs ended in `✓ plan complete`; do not use runner exit status or log tail as a proxy for grade anywhere downstream.

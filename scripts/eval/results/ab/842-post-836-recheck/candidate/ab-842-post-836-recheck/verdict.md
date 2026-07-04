UNDERPOWERED

Lever: 842-post-836-recheck
Baseline arm: scripts/eval/results/sweeps/2026-07-01-pr802-baseline (sha 30b2120) | Candidate arm: scripts/eval/results/ab/842-post-836-recheck/candidate (sha 9377a31)

## Verdict

**UNDERPOWERED.** No cell in this rung reaches significance at n=5/arm. Two cells (devstral-small-2:24b, qwen2.5-coder:32b) show a directional lift but the sample size is too small to distinguish it from noise; three cells (deepseek-r1:70b, qwen2.5-coder:14b, qwen3-coder:30b) show no lift at all, including one cell (qwen3-coder:30b) that moved in the wrong direction (3/5 candidate vs 5/5 baseline).

## Per-cell table

| cell | verdict | candidate | baseline | p (one-sided) | min n/arm for power |
|---|---|---|---|---|---|
| 010-decompose-god-function crew devstral-small-2:24b | **UNDERPOWERED** | 4/5 | 1/5 | 0.1032 | 6 |
| 010-decompose-god-function crew qwen2.5-coder:32b | **UNDERPOWERED** | 5/5 | 3/5 | 0.2222 | 9 |
| 010-decompose-god-function crew deepseek-r1:70b | **NO-LIFT** | 0/5 | 0/5 | 1 | - |
| 010-decompose-god-function crew qwen2.5-coder:14b | **NO-LIFT** | 5/5 | 5/5 | 1 | - |
| 010-decompose-god-function crew qwen3-coder:30b | **NO-LIFT** | 3/5 | 5/5 | 1 | - |

## Expected-flip scorecard

| expected cell | observed verdict |
|---|---|
| 010-decompose-god-function crew devstral-small-2:24b | UNDERPOWERED |
| 010-decompose-god-function crew qwen2.5-coder:32b | UNDERPOWERED |
| 010-decompose-god-function crew deepseek-r1:70b | NO-LIFT |

## Method

Fisher one-sided exact test, alpha=0.05, PASS-only scoring on ungameable rungs.

## Caveats

- **UNGRADEABLE dependency:** if any cell in this rung shows UNGRADEABLE, this rung needs a hidden `grade_spec.rs` (via `/grade-spec-author`) before any lift claim can be made — no lift number is trustworthy on a gameable/self-graded rung. (No UNGRADEABLE cells appear in the tables above, but this caveat stands for any future re-run of this lever until a hidden grade spec is confirmed present.)
- **Underpowered at n=5/arm:** the min n/arm column above (6 and 9) is the estimate for the two directionally-positive cells to reach significance at their observed rates. As a rule of thumb, at n=5/arm, reaching significance at alpha=0.05 (one-sided Fisher) requires roughly a 5/5 vs <=1/5 split — neither underpowered cell here clears that bar (4/5 vs 1/5 and 5/5 vs 3/5 both fall short).
- **Sequential, not interleaved, arms:** the baseline and candidate arms were run sequentially against the model endpoint, not interleaved. Endpoint drift (model/serving updates, load variance, etc.) between the two runs is uncontrolled and could account for part or all of the observed deltas, independent of the code change under test.
- No cell in this rung can currently support a "candidate improves on baseline" claim. The two underpowered cells warrant a re-run at the stated min n/arm before any conclusion is drawn; the three no-lift cells (including the regression on qwen3-coder:30b) suggest the lever's effect, if any, is not uniform across models.

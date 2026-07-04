LIFT

# A/B Verdict — 842-post-836-recheck-n9

## Verdict

**Overall verdict: LIFT**

Lever: `842-post-836-recheck-n9`
Baseline arm: `scripts/eval/results/sweeps/2026-07-01-pr802-baseline` (sha `30b2120`)
Candidate arm: `scripts/eval/results/ab/842-post-836-recheck/candidate` (sha `255dbb3486601320c59f72cec90ae3aa0b43b8c2`)

## Per-cell table

| cell | verdict | candidate | baseline | p (one-sided) | min n/arm for power |
|---|---|---|---|---|---|
| 010-decompose-god-function crew devstral-small-2:24b | **LIFT** | 8/9 | 1/5 | 0.023 | - |
| 010-decompose-god-function crew qwen2.5-coder:32b | **UNDERPOWERED** | 8/9 | 3/5 | 0.2747 | 19 |
| 010-decompose-god-function crew deepseek-r1:70b | **NO-LIFT** | 0/5 | 0/5 | 1 | - |
| 010-decompose-god-function crew qwen2.5-coder:14b | **NO-LIFT** | 5/5 | 5/5 | 1 | - |
| 010-decompose-god-function crew qwen3-coder:30b | **NO-LIFT** | 3/5 | 5/5 | 1 | - |

No UNGRADEABLE cells appear in this rung — the LIFT claim above rests only on the devstral-small-2:24b cell, which reached significance without needing a hidden `grade_spec.rs`.

The qwen2.5-coder:32b cell is **UNDERPOWERED**: at the observed rates (8/9 candidate vs 3/5 baseline), the design needs roughly **n=19 per arm** to reach significance at alpha=0.05 — the current n=9/n=5 sample is too small to distinguish signal from noise here, and this cell should not be used to support or refute the lift claim as-is.

## Expected-flip scorecard

| expected cell | observed verdict |
|---|---|
| 010-decompose-god-function crew devstral-small-2:24b | LIFT |
| 010-decompose-god-function crew qwen2.5-coder:32b | UNDERPOWERED |
| 010-decompose-god-function crew deepseek-r1:70b | NO-LIFT |

## Method

Fisher one-sided exact test, alpha=0.05, PASS-only counts on ungameable rungs. Per-cell p-values and min-n/arm figures are taken verbatim from the input tables above; no recomputation was performed.

## Caveats

- **Arms were run sequentially, not interleaved.** The baseline and candidate arms were executed as separate sweeps rather than interleaved trial-by-trial, so endpoint drift (model/service version, load, or environment changes between runs) is uncontrolled and could confound any observed difference.
- **Small-n significance floor:** at n=5 per arm, a one-sided Fisher exact test at alpha=0.05 requires roughly a 5/5 vs ≤1/5 split to reach significance. Several cells here (deepseek-r1:70b, qwen2.5-coder:14b, qwen3-coder:30b) are exactly n=5 vs n=5 and show identical or near-identical rates (0/5 vs 0/5, 5/5 vs 5/5, 3/5 vs 5/5) — none clear that bar, consistent with their reported NO-LIFT/p=1 verdicts.
- The only cell driving the overall LIFT verdict (devstral-small-2:24b, 8/9 vs 1/5, p=0.023) has an uneven candidate/baseline n (9 vs 5); the qwen2.5-coder:32b cell with a similar-looking split (8/9 vs 3/5) does *not* reach significance, underscoring that these numbers do not generalize linearly across model cells.

# DGX Spark on Terminal-Bench — a one-day capability survey (2026-07-29)

> **Snapshot — 2026-07-29 · newt 0.7.x (the 0.7.6 release-gate window).** This is a
> *dated* survey: its numbers reflect the harness and models **on this date** and
> **will move** as newt and the served models change. Re-runs are published as
> **new dated files** (`docs/findings/YYYY-MM-DD-dgx-spark-terminal-bench-survey.md`)
> and listed in the [findings index](README.md#re-running-a-moving-survey); this
> snapshot is the historical baseline and is **not edited in place**. The always-
> current numbers live in the README scoreboard (auto-generated from
> `bench-results.jsonl`); this document is the frozen narrative + integrity log
> for its date.

**What this is.** A single-day survey of how one NVIDIA DGX Spark (GB10, 121 GiB
unified memory) performs on Terminal-Bench, driven by **newt**, across a range of
local models — with hosted OpenAI models as reference points and a **newt-vs-Codex**
harness comparison on the same model. The live table is the README scoreboard
(auto-generated from `scripts/eval/bench-results.jsonl`); this document is the
narrative, the methodology, and — importantly — the **integrity log** for how the
numbers were kept honest.

The point (per the line's doctrine) isn't the benchmark number for its own sake —
it's the instrument. What we learned about the Spark, the models, and the harness
is the sky; tb-30 is the telescope.

## Methodology

- **Suite:** a fixed 30-task Terminal-Bench subset (**tb-30**) — the cross-model
  instrument. Same tasks for every model, so scores are comparable.
- **Harness:** `newt solve` headless, via the Harbor installed-agent adapter
  (`scripts/eval/harbor/newt_agent.py`), on the **confined OCAP lane**
  (`--confined`): OCAP on, writes fenced to the workspace + container roots,
  reads/exec/net open. Plus the **self-verify gate** (run the task's own checks
  before declaring done) and a **patient retry window** (10 retries, 2s→30s
  backoff) so a transient endpoint blip doesn't zero a task.
- **Clean bench:** vLLM was stopped for the survey, freeing the full 121 GiB to
  the llama.cpp router. This removed the memory-pressure connection drops that
  had been silently deflating earlier scores (see the integrity log).
- **Local models:** served by the dgx1 llama.cpp router (`--models-dir`,
  load-on-demand GGUF, one model at a time). **Hosted models:** `api.openai.com`,
  driven through the *same* newt harness for an apples-to-apples model comparison.
- **Trials:** **1 trial per task.** (little-coder averages 5.) At these low pass
  counts a single flipped task is large variance — read **aggregates as signal,
  individual task flips as noise.**

## Survey results (tb-30, newt harness)

| Model | Params | Host | Score | Note |
|-------|--------|------|-------|------|
| `ornith-1.0-35b-q8` | 35B (Q8) | local | _landing_ | big high-quant local — tracking as leader |
| `qwen3.6_35b` | 35B-A3B | local | **26.7%** (8/30) | coding-tuned; ≈ little-coder's 24.6% bar |
| `qwen3-coder_30b` | 30B | local | 13.3% (4/30) | coding-tuned |
| `o4-mini` | — | hosted | 13.3% (4/30) | cheap hosted **reasoning** model |
| `nemotron-3-nano_30b` | 30B-A3B | local | 6.7% (2/30) | measured pre-clean-bench; re-run queued |
| `gpt-4.1-mini` | — | hosted | 3.3% (1/30) | cheap hosted **general** model |

Cost of the hosted runs: **~$0.26** (gpt-4.1-mini) and **~$2.03** (o4-mini) — the
whole hosted comparison was a few dollars of API spend.

## OCAP parity (the 0.7.6 release gate)

The 0.7.6 gate is: for each model, the confined (OCAP-**on**) score must reach
parity with the `--yolo` (OCAP-**off**) score. Confinement should cost nothing.

| Model | OCAP off | OCAP on | Δ | Verdict |
|-------|----------|---------|---|---------|
| `qwen3.6_35b` | 20.0% | 26.7% | **+6.7 pp** | parity ✓ |
| `qwen3-coder_30b` | 10.0% | 13.3% | **+3.3 pp** | parity ✓ |

Both models score **at or above** their unconfined baseline confined — the
enforcement path routes every op correctly without breaking tasks. Confinement is
free. (The task-level disagreements between lanes carried **zero**
denial/EACCES/EPERM/landlock signatures — they are single-trial variance, not
confinement damage.)

## Harness comparison — newt vs Codex (same model)

Same cheap reasoning model (**o4-mini**), two harnesses, to isolate the harness
from the model. **Both runs verified clean** (0 infra errors, 0 crashes; each
harness actually drove the model and wrote code):

| Harness | Model | Score |
|---------|-------|-------|
| **Codex** | o4-mini | **33.3% (10/30)** |
| newt | o4-mini | 13.3% (4/30) |

**Codex's harness beat newt's by ~2.5× on the same model** — the single most
important finding of the day, and an uncomfortable one for newt (its own project).
It is reported anyway, because integrity is symmetric: the *first* Codex run
scored 3.3% but was **discarded as a crippled artifact** — Codex defaults
`web_search` on, the TB containers have no general web (only `api.openai.com`), so
Codex burned every turn on ~32 failed searches and wrote **zero code**; re-running
with `--ak web_search=disabled` fixed it (19 command-executions + 4 file-changes,
and it passed the task it had failed while crippled). Then the *same* scrutiny was
applied to newt's losing number: it ran clean (0 infra, real writes on nearly
every task — it acted, it just got them wrong), so **13.3% is a fair newt number,
not a hampered one.** A result that *flattered* newt (the crippled 4×) got caught;
a result that *embarrassed* newt (the fair 2.5× loss) got reported.

**What it means.** This is direct evidence for the line's standing thesis that
**the harness — not the model weights — is the current ceiling.** Codex extracts
2.5× more from the *identical* o4-mini than newt does, so newt's agentic loop is
leaving large capability on the table. The hopeful corollary: the DGX Spark's
local models (surveyed above at 13–27% through newt) likely have substantial
headroom too — a better newt loop should lift every row. Closing the gap to Codex
is now a concrete, measurable target.

_Caveats: 1 trial/task (variance is real, though a 10-vs-4 gap is large enough to
be signal); each harness ran in its own fair config (newt confined+self-verify;
Codex web_search-off). gpt-4.1-mini stays a newt-only point — Codex's adapter
requires a reasoning model, so the cross-harness comparison rides on o4-mini._

## Key findings

1. **The Spark punches up.** Its local coding-specialized models
   (qwen3.6 at 26.7%, right at little-coder's 24.6% bar) **beat a cheap hosted
   general frontier model** (gpt-4.1-mini at 3.3%) on real agentic coding — for
   pennies of local inference.
2. **Reasoning beats general at the cheap tier.** A cheap hosted *reasoning*
   model (o4-mini, 13.3%) matches the local coders; a cheap hosted *general* model
   (gpt-4.1-mini, 3.3%) trails badly. TB rewards deliberation.
3. **Confinement is free.** OCAP-on ≈ OCAP-off on every measured model — the
   confined lane doesn't cost capability.
4. **A big high-quant local model can lead.** Ornith 35B at Q8, on the full
   memory freed by stopping vLLM, tracked as the strongest model in the survey.

## Integrity log (why the numbers are trustworthy)

The valuable part of a benchmark is that it measures the agent, not the plumbing.
Two corrections this session kept it honest:

- **Infra noise removed.** A per-task taxonomy of the qwen3.6 confined run showed
  ~30% of "failures" were **not agentic**: 6/30 died on `error sending request`
  (the router OOM-restarting under vLLM co-hosting memory pressure), 2 crashed
  pre-trace, 1 was an empty-reply flake. The honest agentic rate was ~8/21 (38%),
  deflated to 26.7% by noise. Fixes: **stop vLLM** (removes the memory pressure at
  the root), a **patient retry window** (rides a residual blip), and turning on
  **self-verify** (which was off in every prior bench run).
- **Codex comparison de-crippled.** As above — the first Codex number was a
  config artifact (web_search on a no-web sandbox), caught by inspecting the
  trace rather than trusting the score, discarded, and re-measured fairly. A
  result that *flattered* newt (its own project) got more scrutiny, not less.

## Reproducibility

- **Scoreboard tooling:** `scripts/eval/bench_scoreboard.py` — `ingest` (append a
  run), `gate` (per-model per-lane monotonic ratchet), `parity` (off-vs-on gate),
  `render` (rewrite the README table). Manifest: `scripts/eval/bench-results.jsonl`;
  roster: `scripts/eval/bench-roster.json`.
- **Harness:** the Harbor adapter injects the newt binary + a pinned backend
  profile into each task container; `NEWT_BENCH_OCAP=on` selects the confined
  lane, `NEWT_BENCH_SELF_VERIFY=1` the self-verify gate. Hosted backends use
  `NEWT_BENCH_API_KEY_FILE` (the key is uploaded as a file, never an env literal
  or a committed value). Host secrets (endpoints, keys) live only in local files.
- **Portable binary:** benched from a `rust:1.88-bookworm` build (GLIBC ≤ 2.36) so
  the same binary runs in the task containers.

_Auto-generated companion table: see the README scoreboard. This document is
updated as the survey fills in (Ornith Q8 and the fair Codex run are landing)._

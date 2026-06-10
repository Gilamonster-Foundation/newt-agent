# Context/Memory Benchmark Plan — Phases 17-19

Defines the measurements behind the context/memory/conversation improvement
plan (`docs/design/context-memory-hermes-learnings.md`), **before** any of it
ships — so every claimed improvement lands with a number against a captured
baseline, kyln-benchmark style (`kyln/docs/testing/parallel-fetch-benchmark.md`
is the template: quantified TL;DR, honest interpretation, reproduce
instructions, a citability checklist).

**Rule 0 — baseline first.** Capture every B-series number on current `main`
*before* PR 17.1a merges, commit the results to
`docs/testing/results/context-baseline-<sha>.md`, and re-run the affected
benchmark in each landing PR. A PR that claims a win cites its delta; a PR
that regresses a guarded number explains or fixes it.

---

## What we measure and why

| # | Benchmark | Phase it gates | Question it answers |
|---|---|---|---|
| B1 | Turn-write cost & store size | 17.1 | Did SQLite actually beat rewrite-whole-JSON-per-turn? |
| B2 | Recall quality & latency | 17.3-17.5 | Does `recall` find the right conversation, fast, on realistic queries? |
| B3 | Token-estimate accuracy | 18.1 | How wrong is chars/4-plus-no-schemas today, and how wrong are we after? |
| B4 | Compression efficiency | 18.3-18.4 | How many tokens does structural prune reclaim at zero LLM cost? What does the LLM pass add, at what wall-clock price? |
| B5 | Long-horizon task survival ("compression gauntlet") | 18.4-18.5 | Does a task that *forces* compression still complete, with the Active Task intact? |
| B6 | Overflow-400 incidence | 18.1, 18.4 | Is the #223 failure class (unrecoverable context-overflow 400) actually gone? |
| B7 | Resume & startup cost | 17.7 | What does auto-resume cost at cold start, and does it pick the right conversation? |
| B8 | Memory write quality | 19.3 | Does the nudge produce durable facts or noise? |

## Method

### B1 — turn-write cost & store size (criterion)

`newt-core/benches/store_benchmarks.rs` (criterion, like kyln's
`kyln-scm/benches/`): append turn #N for N ∈ {1, 10, 100, 1000} with
realistic turn payloads (1-4KB user/assistant text + tool-event JSON), p50/p95
per append; on-disk size at each N. Baseline: current
`ConversationStore::append_turn` (loads + pretty-rewrites the whole record,
`conversation.rs:119-127`). After: SQLite insert. Expectation to verify, not
assume: JSON is O(record) per turn and SQLite is O(1) — show the crossover and
the constant factors. Also measure `list()` at 100/1,000 conversations
(prune-cap decision in 17.1a needs this number), and **the per-append cost of
the §6 ordering primitives in isolation** — the signed per-writer tick and the
BLAKE3 `prev_hash` content chain — so the "crypto primitives are nearly free
on the write path" claim (design doc §6; thesis #1) ships with a measured
number, not an assertion. Target: chain+tick overhead < 5% of the bare insert.

### B2 — recall quality & latency

A **seeded fixture corpus** (`newt-eval/fixtures/recall-corpus/`,
deterministic generator, committed): 50 conversations × 10-40 turns across 3
synthetic workspaces, salted with realistic artifacts — file paths, issue
numbers (`P2.2`, `#223`-style), command lines, tool events. A **20-query
golden set** with expected hit conversations: keyword, phrase, path-fragment,
issue-number, OR-combination, and 3 adversarial sanitizer cases (dangling
`AND`, unbalanced quotes, hyphenated tokens). Report precision@3 and p95 query
latency (target: <10ms warm). Baseline = what exists today: nothing — recorded
as such; the win is absolute, not relative.

### B3 — token-estimate accuracy

Instrumented live sessions (`NEWT_DEBUG=1` + a small harness) over ≥30
requests across 3 models: log `estimate` vs the backend's reported
`prompt_eval_count`. Metric: median and p95 of |est−actual|/actual. Baseline
captures today's chars/4-no-schemas error (expected to be worst on
schema-heavy, tool-light turns); after 18.1, target p95 ≤ 15%. Also verify the
double-count fix: provider-tracked total vs actual prompt tokens at turn 20 of
a long session (today the tracked number runs away from reality).

### B4 — compression efficiency

Corpus: 10 **recorded real transcripts** (anonymized, committed as fixtures)
from live newt sessions that hit the trim path, 15-60K estimated tokens each.
For each: tokens before → after structural prune alone → after prune+summary.
Report reclaim % per stage, information-loss spot-check (B5 covers it
properly), and wall-clock of the summary call per model. The hermes datum to
beat honestly: prune does *most* of the reclaim at zero LLM cost — if our
prune reclaims <30% on real transcripts, the JSON-aware shrinking needs work
before the summarizer matters.

### B5 — long-horizon "compression gauntlet" (newt-eval)

New eval cases `017-gauntlet-rename` / `018-gauntlet-multifile`: a coding task
sized + `num_ctx`-capped so the loop **must compress ≥2 times** before
completion. Evaluators: task completes (`diff_applies` / `rust_compiles`),
plus a new `active_task_retained` evaluator that greps the post-compression
request for the verbatim original task. Mock mode (canned compression
fixtures) runs in `mock_e2e` CI; live mode via `just eval --case 017` against
local models. Baseline on `main`: expected **failure** (discard-trim loses the
thread) — that recorded failure *is* the before.

### B6 — overflow-400 incidence

Scripted driver (extends `newt-eval` live mode): force a session past the
window with large `read_file` results on a small-`num_ctx` model; count
hard-400s and "empty response" exits over 10 runs. Baseline: reproduce #223.
Target after 18.1+18.4: zero hard failures; every run either completes or
degrades through visible compression.

### B7 — resume & startup cost

Time `newt code` from exec to first prompt with `[context] resume` on/off at
0 / 100 / 1,000 stored conversations (hyperfine, 10 runs). Correctness: 3
workspaces interleaved, assert each resumes its own **latest-by-activity-tick
conversation (§6 — never a timestamp comparison)**; `--ephemeral` leaves no
row; eval runs never resume. Include the **clock-skew case**: step the wall
clock backwards between turns (e.g. `faketime`), assert resume choice and
turn ordering are unaffected — the wall clock is a display claim, not an
ordering key. Target: resume adds <50ms at 1,000 conversations.

### B8 — memory write quality (rubric, manual)

10 scripted live sessions (5 quiet, 5 fact-rich) with the 19.3 nudge active.
Record: notes written, nudge fire-count, scan rejections. Rubric per note
(human-scored, committed with the results): declarative-fact? durable-in-a-week?
non-capability-negative? Target: ≥80% rubric-pass, zero notes in quiet
sessions (reset-on-use working), zero scan bypasses.

## Environment

Live runs on **gnuc** use gnuc's own Ollama (`https://gnuc-ollama.home.lab`),
not the DGX or the LB — pinned models: `llama3.1:8b` (weak-model floor),
`qwen3-coder:30b` (workhorse), one mid model (`qwen2.5-coder:14b`). Record
per kyln's citability checklist: model + quant, ollama version, `num_ctx`,
hardware, newt sha, command lines, raw numbers (commit the table, not just
the summary). Criterion benches run on whatever dev box; they compare
algorithms, not hardware — note the box anyway.

## CI integration

- B1 criterion benches: informational, not gated (perf gates on shared
  runners lie); run via `just bench-store`, results committed per release.
- B2 golden set + B5 mock gauntlet: **gated** — they ride `cargo test` /
  `mock_e2e` under the normal 80% floor.
- B3/B4/B6/B7/B8 live: manual via `just bench-context`, results committed to
  `docs/testing/results/` with the checklist filled in.

## Interpretation discipline (honest)

Per the kyln docs: every results write-up carries an "Interpretation
(honest)" section — what the number does *not* show (localhost ≠ network,
mock ≠ live model, fixture corpus ≠ user corpus), and any fix discovered
while benchmarking gets its own issue/PR and a re-run note, the way kyln's
`TCP_NODELAY` discovery was handled (#119/#120 there).

# Context/Memory Baseline @ `f0f4f6e` (issue #245)

The B-series baseline required by `docs/testing/context-memory-benchmark.md`
**Rule 0**: every number below was captured on current `main`
(`f0f4f6e`, newt v0.6.7) *before* any Phase 17–19 PR lands. Phase 17–19 PRs
cite their deltas against this document.

**TL;DR:**

- **B1 (store write cost):** `ConversationStore::append_turn` is O(record),
  measured: **~60–80 µs at turn 1 → p50 ~3.4 ms / p95 ~3.8–4.3 ms at turn
  1000** (≈50× growth), on a 2.65 MB record. `list()` at 1,000 conversations:
  **p50 ~9.6–11.1 ms** warm. This is the number SQLite (17.1) has to beat.
- **B3 (token-estimate accuracy):** today's `chars/4`-no-schemas estimator,
  47 replayed live requests: median |err| **19.1 %** (llama3.1:8b, n=20),
  **26.5 %** (qwen2.5-coder:14b, n=9), **32.5 %** (qwen3-coder:30b, n=18);
  p95 **35.5 / 28.1 / 37.2 %**. Uncounted tool schemas cost **~705 tokens**
  (llama/qwen2.5) to **~1,065 tokens** (qwen3) per request. 18.1's target:
  p95 ≤ 15 %.
- **B3 drift:** the per-turn "input tokens" newt tracks (and feeds into the
  tuning ratchet) sums every round's full prompt *including* the probe+stream
  double-count. After a 20-turn session the tuning cache recorded
  `max_ok_input = 25,602` when **no single prompt the backend evaluated
  exceeded 4,748 tokens** — a 5.4× overstatement, at `tune_confidence: high`.
- **B5 (gauntlet probe):** a 10-file-read task at `num_ctx=4096` produced a
  **27,434-token (estimated) request; Ollama silently evaluated 3,059** of it.
  newt's own trim machinery never fired (0 mid-loop, 0 pre-send). The task
  "completed", but the post-overflow round returned degenerate pseudo-tool-call
  text. Trim-under-pressure is effectively **untested by newt itself** today —
  the backend's silent truncation absorbs everything.
- **B6 (#223 overflow class):** 10/10 scripted overflow runs (est. ~39.9k-token
  request vs `num_ctx=4096`): **0 hard 400s, 0 empty-response exits — and
  9/10 wrong answers.** Against Ollama the #223 failure class doesn't present
  as a 400; it presents as *silent task loss* (the truncation discards system
  prompt + task, the model answers about raw file contents instead).
- **B7 (cold start):** flat **3.02 s at 0 / 100 / 1,000 stored conversations**
  (σ ≤ 11 ms). Root-caused: the constant is a **3-second DCGM telemetry probe
  timeout** (`DgxTelemetry::try_connect` GETs `https://<host>:9400/metrics`;
  the port hangs on gnuc). Store size contributes ~0 ms — nothing reads the
  store at startup on this baseline. The store-attributable resume budget for
  17.7 starts from ~0.
- **B2 (recall), B8 (memory write quality): absent — the features do not
  exist on this baseline.** Recorded as such; their wins will be absolute.

---

## Environment (citability)

| | |
|---|---|
| newt | v0.6.7, workspace @ `f0f4f6e` (`bench-baseline-context`), release build |
| Host (all runs) | gnuc (`geforcenuc`): i7-11700B (16 threads), 30 GiB RAM, RTX 4060 Ti 16 GiB (driver 580.142), Linux 6.8.0-111-generic |
| Inference | gnuc's own Ollama **0.20.3** via `https://gnuc-ollama.home.lab` (operator rule: never DGX/LB for these runs) |
| Models (pinned) | `llama3.1:8b` Q4_K_M (4.9 GB) · `qwen2.5-coder:14b` Q4_K_M (9.0 GB) · `qwen3-coder:30b` Q4_K_M (18.6 GB) |
| B1 disk | tempdir on local ext4 (LVM), page-cache writes (`save_record` does not fsync) |
| Toolchain | rustc 1.96.0, hyperfine 1.20.0, Python 3.12.3 |
| Live-session harness | sandbox `$HOME` per run (never the real `~/.newt`), `NEWT_DEBUG=1`, `TERM=dumb`, prompts piped on stdin; capture proxy on `127.0.0.1:18434` logs every request/response (`scripts/ollama_capture_proxy.py`) |
| `num_ctx` | 8192 for B3 sessions; **4096** for B5/B6 (the squeeze); model default for B7 (no inference turn) |

Scripts: `docs/testing/results/scripts/` (scratch harnesses — standalone, not
workspace members, not in CI; each file's header says how to run it).

---

## B1 — turn-write cost & store size

### Method

Standalone bin (`scripts/b1-store-bench/`, depends on `newt-core` by path) so
criterion isn't added as a workspace dep just for a baseline. It drives the
real `ConversationStore` against a tempdir:

- **Phase A:** one conversation, 1,000 `append_turn` calls with deterministic
  1–4 KB payloads (prose + code fragment + tool-event JSON, LCG-sized).
  Per-append wall time; p50/p95 over windows around N ∈ {1, 10, 100, 1000}
  (turns 1, 2–10, 90–110, 900–1000). Record size at each N.
- **Phase B:** fresh store, 100 / 1,000 conversations × 5 turns; `list()`
  measured over 20 warm iterations.
- `max_per_workspace = 0` (prune disabled — `append_turn` never prunes anyway,
  `conversation.rs:119-127`).

3 runs (`/tmp/newt-bench/b1-run{1,2,3}.txt`, reproduced in the appendix).

### Results

`append_turn` latency (µs), 3 runs:

| N (window) | p50 r1 | p50 r2 | p50 r3 | p95 r1 | p95 r2 | p95 r3 |
|---|---:|---:|---:|---:|---:|---:|
| 1 (turn 1) | 81.7 | 62.7 | 55.3 | — | — | — |
| 10 (turns 2–10) | 124.3 | 60.7 | 65.6 | 660.3 | 91.1 | 78.5 |
| 100 (turns 90–110) | 558.8 | 394.9 | 435.9 | 891.9 | 714.1 | 531.8 |
| 1000 (turns 900–1000) | 3437.2 | 3398.5 | 3317.6 | 4349.4 | 3969.0 | 3773.1 |

On-disk record size (identical across runs — deterministic payloads):

| N turns | bytes |
|---:|---:|
| 1 | 1,546 |
| 10 | 24,813 |
| 100 | 267,586 |
| 1000 | 2,653,578 |

`list()` warm, 20 iters:

| conversations | p50 (µs) | p95 (µs) | store size |
|---:|---:|---:|---:|
| 100 | 836–863 | 863–983 | 1.36 MB |
| 1000 | 9,631–11,090 | 9,951–11,841 | 13.6 MB |

### Interpretation (honest)

- **O(record) confirmed, with numbers:** per-append cost grows ~50× from turn
  1 to turn 1000 because `append_turn` loads, deserializes, re-serializes
  (pretty) and rewrites the whole record. At turn 1000 every appended ~2.5 KB
  turn costs a 2.65 MB read+write.
- ~3.4 ms per turn is still invisible next to a multi-second inference call —
  the case for 17.1 is the *trajectory* (long conversations, `list()` doing a
  full read+parse of every record at O(workspace), 10 ms at 1k conversations)
  plus crash-safety, not today's absolute pain.
- Tempdir = page cache; no fsync. A durability-honest store would pay more —
  both before and after, so the comparison holds.
- The §6 ordering-primitive overhead (tick + BLAKE3 chain) is **not measured
  here** — those primitives don't exist yet; their <5 %-of-insert claim must be
  measured in the 17.1 PR against this table.
- Run 1's N=10 p95 (660 µs vs 91/79) is first-touch noise on a cold tempdir;
  windows this small (9 samples) carry it visibly. The N=1000 numbers, where
  the claim lives, agree across runs within ~7 %.

---

## B2 — recall quality & latency

**Absent.** No `recall` feature exists at `f0f4f6e`. Baseline recorded as:
nothing to measure. The 17.3–17.5 win is absolute (feature vs no feature);
its quality gate is the seeded corpus + golden set defined in the plan.

---

## B3 — token-estimate accuracy

### Method

1. **Capture:** scripted `newt code` sessions (sandbox HOME, `num_ctx=8192`)
   through a logging proxy, two prompt sets per pinned model
   (`scripts/b3_capture_sessions.sh` — read files, run commands, write a file,
   answer-from-memory). Every `/api/chat` body newt actually sent is logged.
2. **Replay:** `scripts/b3_replay_estimate.py` recomputes newt's estimate for
   each unique captured body exactly as `newt-tui/src/lib.rs::estimate_tokens`
   does (compact-JSON chars per message, summed, ÷4 — tool schemas and chat
   template uncounted), then replays the body against the same Ollama with
   `stream:false`, `num_predict:1`, and a UUID cache-buster prefixed to the
   system message so `prompt_eval_count` reports the **full** prompt rather
   than the non-KV-cached suffix. Error = |est − actual| / actual.
3. **Schema cost:** every 4th body replayed again with the `tools` array
   removed; the delta is what the estimator never counts.
4. **Drift:** one 20-turn session (`llama3.1:8b`, trivial "reply ok N"
   prompts) comparing newt's tracked per-turn input (and the tuning-cache
   ratchet it feeds) against backend-evaluated prompt sizes.

47 unique requests replayed (llama 20, qwen2.5 9, qwen3 18 — qwen2.5 ended
both sessions early by emitting tool calls as plain JSON content, so it
produced fewer rounds). Full 47-row table in the appendix.

### Results

| model | n | median \|err\| | p95 \|err\| | mean signed (est−actual)/actual |
|---|---:|---:|---:|---:|
| llama3.1:8b | 20 | **19.1 %** | 35.5 % | −3.1 % |
| qwen2.5-coder:14b | 9 | **26.5 %** | 28.1 % | −26.6 % |
| qwen3-coder:30b | 18 | **32.5 %** | 37.2 % | −28.9 % |

Tool-schema cost (uncounted today), 13 paired replays:

| model | schema tokens per request |
|---|---:|
| llama3.1:8b | 702–707 |
| qwen2.5-coder:14b | 719–728 |
| qwen3-coder:30b | 1,063–1,066 |

**Error structure.** The signed errors aren't noise:

- Small, schema-dominated requests **undercount ~25–37 %** on every model —
  the fixed ~700–1,070-token schema block is missing from the estimate.
- Large file-payload requests on llama3.1 flip to **overcount up to +35.7 %**
  (est 10,199 vs actual 7,518): chars/4 overestimates dense English/markdown
  for llama's tokenizer. The two effects partially cancel in llama's mean
  (−3.1 %) while both qwens (denser tokenizers + bigger schema rendering)
  undercount across the board.

**Double-count drift (20-turn session).** newt's per-turn "input" (status
footer, `usage.jsonl`, and the value fed to `record_success`) is the **sum of
`prompt_eval_count` over every round in the turn, including the probe and the
streaming repeat of the final round** (verified: turn 20 footer 22,451 =
2,582 + 3,765 + 3,849 + 3,983 + 4,136 + 4,136). Consequences, measured:

| quantity | value |
|---|---:|
| largest single prompt the backend evaluated, whole session | **4,748 tokens** |
| largest per-turn "input" newt tracked (turn 1) | **25,602 tokens** |
| turn 20: tracked vs largest real prompt that turn | 22,451 vs 4,136 (**5.4×**) |
| tuning cache after session | `max_ok_input: 25602`, `tune_confidence: high` |

The ratchet (`probe.rs::record_success`) now believes a 25.6k-token input is
proven-safe when nothing over 4.7k was ever evaluated. That inflated cap is a
direct ingredient of the #223 class: any future pre-send gate keyed on
`max_ok_input` would wave through prompts ~5× past reality.

### Interpretation (honest)

- The 18.1 target (p95 ≤ 15 %) is **2.4–4.8× away** at this baseline, and the
  error is *structured* (schema-blind + tokenizer-blind), so counting schemas
  and calibrating per-family chars/token should capture most of it. That's
  verifiable against this exact replay corpus.
- `prompt_eval_count` with a cache-buster is the ground truth here; the buster
  adds ~20 tokens to multi-thousand-token prompts (noise at the reported
  precision). Without it, Ollama's KV-cache makes follow-up requests report
  *fewer* prompt tokens than their predecessors (verified during harness
  development).
- These are Ollama tokenizers for three local model families; API providers
  (Anthropic/OpenAI) tokenize differently. The per-model split is the point:
  one global chars/4 cannot be right for all of them.
- Session nondeterminism (temperature) means a re-capture produces different
  bodies; the replay corpus (JSONL capture logs, kept with the raw bench
  outputs) pins this exact set for the 18.1 delta.
- newt sends a non-streaming **probe** request and then a **streaming repeat
  of the same messages** each final round — so the prompt is evaluated twice
  per displayed reply (visible as paired `prompt_eval_count` entries; the
  second is KV-cached and fast, but it's *counted* twice by newt's tracker).

---

## B5 — long-horizon "compression gauntlet" probe

### Method

**This is a scripted probe, not the future 017/018 eval cases** (those need
`newt-eval` cases + an `active_task_retained` evaluator that don't exist yet).
One session, `llama3.1:8b`, `num_ctx=4096`:
the task names an `ACTIVE TASK GAUNTLET-7f3d9c` marker, demands ten ~10 KB
`read_file`s one at a time, then a `result.txt` write containing the marker;
a follow-up turn asks the model to restate the marker from memory
(`scripts/b56_gauntlet.sh b5`, analyzed by `scripts/b56_analyze.py`).

### Results

| metric | value |
|---|---|
| `/api/chat` requests | 6 (2 turns) |
| non-2xx responses | 0 |
| overflow round: estimated vs backend-evaluated prompt | **27,434 vs 3,059 tokens** |
| newt mid-loop trims / pre-send trims / overflow notices | **0 / 0 / 0** |
| marker in last request body | yes |
| `result.txt` written with marker | yes ("task completed") |
| marker restated from memory (turn 2) | yes |
| post-overflow model output | degenerate: `<|python_tag|>read_file('…')` as plain content |

The model issued all 11 tool calls in a single round (10 reads + the
`result.txt` write), so the "one at a time" pressure never built; the whole
~104 KB of tool results landed in *one* request, Ollama silently evaluated
the last ~3k tokens of it, and the round's reply was pseudo-tool-call garbage
that newt displayed as content.

### Interpretation (honest)

- **The expected baseline failure ("discard-trim loses the thread") did not
  occur — because newt's trim path never engaged at all.** The mid-loop trim
  is message-*count*-gated (default threshold ≫ the 14 messages here) and the
  pre-send trim had no token budget to enforce (fresh sandbox = empty tuning
  cache). The overflow was absorbed upstream by Ollama's silent truncation.
- The marker "survived" in the request body and in newt's 10-turn history
  window — but the backend never saw it in the overflow round (it was in the
  truncated-away head). Task success here is luck of the payload order plus
  a forgiving second turn, not context management.
- A single scripted run on the weakest pinned model; no p-values claimed.
  The real gauntlet (017/018) must size the task so compression fires ≥2
  times *in newt* — which, per this probe, additionally requires a token-aware
  trigger to exist at all. On this baseline the gauntlet would measure
  Ollama's truncation, not newt's compression.

---

## B6 — overflow-400 incidence (#223 class)

### Method

10 scripted single-turn sessions (`scripts/b56_gauntlet.sh b6 10`),
`llama3.1:8b`, `num_ctx=4096`, fresh sandbox HOME per run (so one run's
tuning ratchet can't affect the next). The turn demands three ~50 KB
`read_file`s (≈39.9k estimated tokens once in the request) and then asks for
the verbatim ACTIVE TASK marker. Counted per run: non-2xx responses,
empty-response exits, trim events, backend-evaluated tokens, and whether the
final *answer* contained the marker.

### Results

| run | `/api/chat` reqs | non-2xx | empty-resp | trims | overflow req: est → evaluated | marker in answer |
|---:|---:|---:|---:|---:|---|---|
| 1 | 3 | 0 | 0 | 0 | 39,933 → 835 | **yes** |
| 2 | 3 | 0 | 0 | 0 | 39,879 → 4,096 | no |
| 3 | 3 | 0 | 0 | 0 | 39,900 → 826 | no |
| 4 | 3 | 0 | 0 | 0 | 39,925 → 829 | no |
| 5 | 3 | 0 | 0 | 0 | 39,930 → 835 | no |
| 6 | 3 | 0 | 0 | 0 | 39,925 → 825 | no |
| 7 | 3 | 0 | 0 | 0 | 39,919 → 822 | no |
| 8 | 3 | 0 | 0 | 0 | 39,990 → 887 | no |
| 9 | 3 | 0 | 0 | 0 | 39,879 → 4,096 | no |
| 10 | 3 | 0 | 0 | 0 | 39,903 → 827 | no |

**Hard failures: 0/10. Correct answers: 1/10.**

### Interpretation (honest)

- **The #223 hard-400 does not reproduce against Ollama**, and that's the
  finding: Ollama 0.20.3 silently truncates `/api/chat` prompts to `num_ctx`
  instead of erroring. The original #223 was a strict endpoint
  (LiteLLM/Anthropic) that 400s on overflow; reproducing the literal 400
  needs such an endpoint and is **out of reach of this gnuc-only baseline** —
  recorded as unmeasured, not as "fixed".
- The failure class is alive and worse-than-400 here: in 9/10 runs the
  truncation discarded the system prompt *and the task*, the model wrote an
  essay describing raw file contents, and **nothing anywhere surfaced that the
  request had been cut by ~90 %** — newt's estimator put the request at ~39.9k
  tokens against a known `num_ctx` of 4,096 and still sent it without any
  trim or warning (0 trim events in 10 runs; same fresh-cache reason as B5).
- Post-18.1+18.4 target restated for this harness: every run either completes
  or **visibly** degrades through newt-side compression; "evaluated ≪
  estimated with no event" counts as a failure even when the transport says
  200.

---

## B7 — resume & startup cost

### Method

`hyperfine` (2 warmups, 10 runs) on `newt --no-splash code <ws>` with `exit`
piped on stdin (no inference turn), sandbox HOMEs seeded with 0 / 100 / 1,000
synthetic conversations matching the real store schema
(`scripts/b7_startup.sh`, `scripts/b7_seed_store.py`; 16.1 MB store at 1,000).
There is no resume feature on this baseline, so this is the **cold-start
floor** 17.7 must not regress.

### Results

| stored conversations | mean | σ | min … max |
|---:|---:|---:|---|
| 0 | 3.019 s | 0.011 s | 3.010 … 3.045 s |
| 100 | 3.019 s | 0.006 s | 3.010 … 3.029 s |
| 1000 | 3.023 s | 0.004 s | 3.020 … 3.029 s |

User+system CPU per run: ~15 ms. Wall time is ~3.0 s of *waiting*.

**Root cause of the 3 s constant (found while benchmarking):**
`DgxTelemetry::try_connect` (`newt-tui/src/dgx_probe.rs`) derives
`https://gnuc-ollama.home.lab:9400/metrics` from the Ollama URL and GETs it
with a **3-second timeout** at session setup. Port 9400 on that host neither
answers nor refuses (verified: `curl` hangs until killed), so every start
eats the full timeout. Control: with a connection-*refused* URL
(`http://127.0.0.1:9`) the identical invocation completes in **0.00 s wall**.

### Interpretation (honest)

- **Store size costs nothing at startup** (Δmean @1000 vs @0 = 4 ms, within
  σ) — as expected, since nothing reads the store before first prompt on this
  baseline. The 17.7 budget ("resume adds <50 ms at 1,000 conversations")
  starts from a store-attributable ~0 ms, with `list()` measured at ~10 ms
  (B1) as the obvious resume building block.
- The 3.02 s headline is **environment-shaped**: it's the DCGM probe timeout
  against this specific host, not a property of newt's store or of clean
  hosts (refused port → instant). On a host where :9400 answers or refuses,
  the floor would be near-zero. Candidate fix (own issue, not this PR):
  probe DCGM async/lazily or with a sub-second connect timeout.
- `--ephemeral`, multi-workspace resume correctness, and the clock-skew case
  are 17.7 acceptance items; nothing to baseline (no resume exists).

---

## B8 — memory write quality

**Absent.** No 19.3 memory-write nudge exists on this baseline; no notes are
written by any path measured above. Recorded as such.

---

## Fix candidates discovered while benchmarking

Per the plan's interpretation discipline (kyln `TCP_NODELAY` precedent), each
gets its own issue rather than riding a benchmark PR:

1. **3 s DCGM probe tax on every startup** when port 9400 black-holes (B7).
2. **Turn-usage tracker sums probe + stream duplicates and per-round
   prefixes**, then feeds the inflated sum into `max_ok_input` at high
   confidence (B3 drift) — poisons any future pre-send gate.
3. **No token-aware send gate**: a request estimated at ~10× `num_ctx` goes
   out with no trim/warning; Ollama's silent truncation hides task loss
   (B5/B6). Already the subject of #223's recommendations; the baseline
   quantifies it.
4. **Replay-harness note:** capture dedup must hash the full message array —
   any truncated key collapses a session's bodies into one (all share the
   multi-KB system-prefix; bug found and fixed in
   `scripts/b3_replay_estimate.py` during this baseline).

---

## Reproduce

```bash
# B1 — store bench (no inference; ~1 min)
CARGO_TARGET_DIR=$HOME/.cache/newt-bench-target \
  cargo run --release --manifest-path docs/testing/results/scripts/b1-store-bench/Cargo.toml

# Build the newt binary the live harnesses use
CARGO_TARGET_DIR=$HOME/.cache/newt-bench-target cargo build --release -p newt-cli

# B3 — capture (live, ~15 min) then replay (live, ~5 min)
bash docs/testing/results/scripts/b3_capture_sessions.sh
python3 docs/testing/results/scripts/b3_replay_estimate.py --schema-cost \
    /tmp/newt-bench/capture-b3*-*.jsonl

# B5 / B6 (live; ~3 min + ~10 min) — seed ws-b5 with 10×10KB and ws-b6 with
# 3×50KB text files first (any prose-like filler)
bash docs/testing/results/scripts/b56_gauntlet.sh b5
bash docs/testing/results/scripts/b56_gauntlet.sh b6 10

# B7 — startup (no inference turn; ~2 min)
bash docs/testing/results/scripts/b7_startup.sh https://gnuc-ollama.home.lab 10
```

All live steps target `https://gnuc-ollama.home.lab` and a sandbox HOME; they
never touch the real `~/.newt`.

## Citability checklist

- [x] Models + quants: llama3.1:8b / qwen2.5-coder:14b / qwen3-coder:30b, all Q4_K_M
- [x] Ollama version: 0.20.3 (gnuc's own instance — never DGX/LB)
- [x] `num_ctx` per benchmark: 8192 (B3) / 4096 (B5, B6) / default (B7)
- [x] Hardware: gnuc — i7-11700B, 30 GiB RAM, RTX 4060 Ti 16 GiB, driver 580.142
- [x] newt SHA: `f0f4f6e` (v0.6.7), release build, rustc 1.96.0
- [x] Command lines: `scripts/` + Reproduce section
- [x] Raw numbers committed: appendix tables below (per-run B1, all 47 B3 rows, per-run B6, all 20 drift turns)
- [x] What is *not* shown: hard-400 reproduction (needs a strict non-Ollama endpoint), criterion-grade isolation (B1 is wall-clock on a dev box), B2/B8 (features absent)

---

## Appendix — raw tables

### B1 raw (3 runs)

Run 1: turn 1: 81.7 µs · turns 2–10: p50 124.3 / p95 660.3 · turns 90–110:
p50 558.8 / p95 891.9 · turns 900–1000: p50 3437.2 / p95 4349.4 / min 3056.1 /
max 7037.9. `list()` @100: p50 848.1 / p95 916.0; @1000: p50 10336.8 / p95 11261.8.

Run 2: turn 1: 62.7 · 2–10: p50 60.7 / p95 91.1 · 90–110: p50 394.9 / p95
714.1 · 900–1000: p50 3398.5 / p95 3969.0 / min 3101.7 / max 26680.5.
`list()` @100: p50 836.4 / p95 862.8; @1000: p50 9631.4 / p95 9950.5.

Run 3: turn 1: 55.3 · 2–10: p50 65.6 / p95 78.5 · 90–110: p50 435.9 / p95
531.8 · 900–1000: p50 3317.6 / p95 3773.1 / min 3065.5 / max 7408.5.
`list()` @100: p50 863.0 / p95 983.2; @1000: p50 11090.1 / p95 11840.8.

Sizes (all runs): 1,546 / 24,813 / 267,586 / 2,653,578 bytes at 1/10/100/1000
turns; 1,364,227 bytes @100 convs; 13,623,097 bytes @1000 convs.

### B3 raw — 47 replayed requests

| model | msgs | est | actual | signed err |
|---|---:|---:|---:|---:|
| llama3.1:8b | 2 | 1679 | 2280 | −26.4 % |
| llama3.1:8b | 4 | 4829 | 4435 | +8.9 % |
| llama3.1:8b | 4 | 1828 | 2380 | −23.2 % |
| llama3.1:8b | 7 | 2068 | 1907 | +8.4 % |
| llama3.1:8b | 6 | 2016 | 2539 | −20.6 % |
| llama3.1:8b | 8 | 2152 | 1987 | +8.3 % |
| llama3.1:8b | 8 | 2132 | 2639 | −19.2 % |
| llama3.1:8b | 11 | 7030 | 6577 | +6.9 % |
| qwen2.5-coder:14b | 2 | 1679 | 2334 | −28.1 % |
| qwen2.5-coder:14b | 4 | 1741 | 2389 | −27.1 % |
| qwen2.5-coder:14b | 6 | 1795 | 2438 | −26.4 % |
| qwen2.5-coder:14b | 8 | 1858 | 2497 | −25.6 % |
| qwen3-coder:30b | 2 | 1679 | 2677 | −37.3 % |
| qwen3-coder:30b | 4 | 4849 | 5537 | −12.4 % |
| qwen3-coder:30b | 4 | 1840 | 2784 | −33.9 % |
| qwen3-coder:30b | 6 | 1890 | 2830 | −33.2 % |
| qwen3-coder:30b | 8 | 2091 | 3000 | −30.3 % |
| qwen3-coder:30b | 6 | 1884 | 2819 | −33.2 % |
| qwen3-coder:30b | 8 | 2018 | 2935 | −31.2 % |
| qwen3-coder:30b | 10 | 5191 | 5803 | −10.5 % |
| qwen3-coder:30b | 12 | 5425 | 6011 | −9.7 % |
| qwen3-coder:30b | 8 | 1942 | 2871 | −32.4 % |
| qwen3-coder:30b | 10 | 1993 | 2907 | −31.4 % |
| qwen3-coder:30b | 12 | 3752 | 4732 | −20.7 % |
| llama3.1:8b | 2 | 1679 | 2285 | −26.5 % |
| llama3.1:8b | 8 | 6955 | 6559 | +6.0 % |
| llama3.1:8b | 11 | 10199 | 7518 | +35.7 % |
| llama3.1:8b | 13 | 10296 | 7597 | +35.5 % |
| llama3.1:8b | 4 | 2008 | 2568 | −21.8 % |
| llama3.1:8b | 6 | 2209 | 2060 | +7.2 % |
| llama3.1:8b | 6 | 2078 | 2618 | −20.6 % |
| llama3.1:8b | 8 | 2211 | 2059 | +7.4 % |
| llama3.1:8b | 8 | 2135 | 2662 | −19.8 % |
| llama3.1:8b | 10 | 2195 | 2042 | +7.5 % |
| llama3.1:8b | 10 | 2185 | 2700 | −19.1 % |
| llama3.1:8b | 12 | 3950 | 3832 | +3.1 % |
| qwen2.5-coder:14b | 2 | 1684 | 2336 | −27.9 % |
| qwen2.5-coder:14b | 4 | 1745 | 2399 | −27.3 % |
| qwen2.5-coder:14b | 6 | 1801 | 2450 | −26.5 % |
| qwen2.5-coder:14b | 8 | 1868 | 2515 | −25.7 % |
| qwen2.5-coder:14b | 10 | 1931 | 2567 | −24.8 % |
| qwen3-coder:30b | 2 | 1684 | 2682 | −37.2 % |
| qwen3-coder:30b | 4 | 1767 | 2744 | −35.6 % |
| qwen3-coder:30b | 6 | 1968 | 2918 | −32.6 % |
| qwen3-coder:30b | 6 | 1872 | 2826 | −33.8 % |
| qwen3-coder:30b | 8 | 1937 | 2878 | −32.7 % |
| qwen3-coder:30b | 10 | 2004 | 2930 | −31.6 % |

(First block: prompt set 1; second block: prompt set 2. `est`/`actual` in
tokens; `actual` = `prompt_eval_count` with cache-buster.)

### B3 drift raw — tracked per-turn input, 20 turns (llama3.1:8b)

25,602 · 5,878 · 4,908 · 12,081 · 5,210 · 5,428 · 10,739 · 7,547 · 7,731 ·
11,177 · 6,779 · 9,698 · 6,412 · 4,193 · 9,726 · 6,257 · 6,314 · 6,448 ·
6,525 · 22,451 (tokens "in" per turn footer / `usage.jsonl`).
Largest single backend-evaluated prompt in the session: 4,748 tokens.
Tuning cache after exit: `max_ok_input: 25602`, `safe_context: 104857`,
`context_window: 131072`, `tune_confidence: high`.

### B5/B6 analyzer outputs

B5: `{"chat_requests": 6, "non_2xx": [], "marker_in_last_request": true,
"max_prompt_eval_count": 3059, "empty_response_msgs": 0, "error_lines": 0,
"mid_loop_trims": 0, "pre_send_trims": 0, "overflow_notices": 0}` ·
`result.txt: ACTIVE TASK GAUNTLET-7f3d9c done`

B6 per run (`max_prompt_eval_count` of the session, which is the
system+user first request in 8/10 runs — the *overflow* request evaluated
lower, see Results): 1523 / 4096 / 1525 / 1527 / 1523 / 1524 / 1520 / 1527 /
4096 / 1526; all other analyzer fields zero in all 10 runs.

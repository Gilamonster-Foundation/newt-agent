# Context/Memory After-Benchmarks @ `cf1aa3e` (issues #246 / #247 / #248)

**Status:** condensed 2026-08-18. The B-series methodology, environment
capture, and per-case narrative for B1 through B8 were retired; every number
they produced is in the comparison table below. The full run is in git history
before this commit, and the baseline it compares against is
[`context-baseline-f0f4f6e.md`](context-baseline-f0f4f6e.md).

Phase 17 to 19 shipped between the two runs. The table is the durable result;
the narrative was a record of how the numbers were taken, which no longer has
a reader.

## Comparison — before (`f0f4f6e`) → after (`cf1aa3e`)

| case | metric | before | after | verdict |
|---|---|---|---|---|
| B1 | `append_turn` p50 @ turn-1000 window | 3,318–3,437 µs | 195–235 µs | **~15–17× faster, O(1)** |
| B1 | `append_turn` p95 @ turn-1000 window | 3,773–4,349 µs | 566–585 µs | **~7× faster** |
| B1 | growth turn 1 → 1000 | ~50× | none (noise-flat) | **O(record) → O(1)** |
| B1 | `list()` p50 @ 1,000 conversations | 9.6–11.1 ms | 0.98–1.04 ms | **~10× faster** |
| B1 | store size @ 1k turns | 2.65 MB | 4.23 MB db (+4.2 MB transient WAL) | **+60% — regression, priced in** |
| B1 | store size @ 1,000 convs | 13.6 MB | 22.5 MB db (+5.6 MB WAL) | **+65% — regression, priced in** |
| B1 | `verify_chain` @ 1k turns | n/a (no chain) | p50 3.2–3.4 ms | new capability |
| B1 | FTS `search()` @ 1k turns, warm | n/a (no search) | p50 1.2–2.8 ms (target <10 ms) | new capability, target met |
| B1/§6 | tick+chain overhead vs bare insert | unmeasured (primitives absent) | **+74–76% p50** (budget <5%) | **budget MISSED** — BLAKE3 itself 1.3 µs (~3–6%, in budget); the 2 extra SQL statements are the cost; absolute ~16–30 µs/turn |
| B2 | recall precision@3 / latency | absent (no feature) | feature shipped; latency ≤ ~3 ms p95; **precision corpus never built** | partial — quality unmeasured |
| B3 | estimator median \|err\| (llama / qwen2.5 / qwen3) | 19.1 / 26.5 / 32.5 % | 18.8 / **6.5** / **4.4** % | qwens fixed |
| B3 | estimator p95 \|err\| (target ≤15%) | 35.5 / 28.1 / 37.2 % | **49.0** / **6.7** / **7.2** % | **met on qwens; llama WORSE** (now one-sided overcount — conservative direction) |
| B3 | uncounted schema tokens/request | 702–1,066 | 0 uncounted (estimator adds 802; real 702–1,065) | fixed, ±14–25% residual |
| B3 | tracked turn input ÷ real max prompt | **5.4×** (`max_ok_input` 25,602 at high confidence) | **1.0×** (`max_ok_input` 6,482 = real max; poisoned entries migrated) | **fixed, verified** |
| B5 | newt trim/compression events under pressure | 0 / 0 / 0 (never engaged) | fired, visible notice + honest "still over budget" | **engages now** (but this run's model skipped the reads — pressure differed) |
| B5 | biggest request est → evaluated | 27,434 → 3,059 (silent) | 2,919 → 2,708 (no overflow built) | not comparable head-to-head |
| B6 | hard 400s / empty-response exits | 0/10 / 0 | 0/10 / 0 | unchanged (Ollama never 400s) |
| **B6** | **correct / visibly-degraded / silently-wrong** | **1 / 0 / 9 of 10** | **2 / 0 / 8 of 10** | **NOT FIXED — statistically unchanged.** Fresh-cache single-turn + forced `num_ctx`: no guard consults the `num_ctx` newt itself sends |
| B7 | cold start @ 0/100/1,000 convs | 3.019/3.019/3.023 s | 3.023/3.026/3.026 s | unchanged — still the #255 DCGM 3 s probe tax |
| B7 | store+resume cost @ 1,000 convs (control) | ~0 ms (nothing read the store) | **+2.1 ms** incl. auto-resume (12.1→14.2 ms total) | resume shipped; <50 ms budget met ~25× over |
| B7 | resume correctness | n/a | resumes latest-by-activity-tick (banner verified) | new capability |
| B8 | memory write quality | absent (no feature) | feature shipped; rubric not run (manual) | partial — quality unmeasured |
| B4 | compression efficiency corpus | not measured (no corpus) | not measured (corpus still doesn't exist) | unchanged gap |

**Delta narrative, honestly:** Phase 17 delivered what it promised and the
numbers show it (O(1) appends ~16× faster at 1k turns, 10× faster list,
search/verify/resume all new and inside their budgets) at a ~60% disk-size
price nobody had budgeted but nobody had promised either. Phase 18's
*accounting* is genuinely fixed — the 5.4× ratchet poisoning is gone
(1.0× measured), schemas are counted, and two of three models now
estimate inside the ≤15% p95 target — but llama3.1's p95 got *worse*
(one-sided conservative now, which is the safe direction, but worse is
worse and it's flagged). Phase 18's *enforcement* engages from a
session's second turn (B5 fired visibly; baseline never fired at all),
yet **the headline B6 scenario — first turn, fresh cache, operator-forced
small `num_ctx` — is statistically unchanged (8/10 silent task loss)**
because no budget consults the `num_ctx` newt itself puts in the request.
The §6 ordering primitives cost ~75% of a bare insert (vs a <5% budget) —
the crypto is as-promised nearly free; the budget didn't price the SQL.
The two non-perf B-cases that were "absent" at baseline (B2 recall, B8
memory) shipped as features but their *quality* harnesses (golden set,
rubric) still don't exist, so their wins remain unquantified.

---

## Fix candidates discovered while benchmarking

Per the plan's interpretation discipline, each should get its own issue
rather than riding this benchmark PR:

1. **The pre-send budget never consults the effective `num_ctx` (B6 — the
   headline).** `send_budget = max_ok_input ∥ safe_context` while the
   request body carries `options.num_ctx = 4096`; a fresh cache therefore
   waves a 41k-token request into a 4k window with zero events, exactly
   the baseline failure. Candidate: `send_budget = min(budget,
   eff_num_ctx)` (with headroom for the response), which would have fired
   the existing, working compression pipeline in all 10 B6 runs.
2. **`max_ok_input`-as-cap is over-eager on small sessions (B5).** The
   now-truthful ratchet records the largest prompt *proven* (2,135 after
   one small turn) and the guard treats it as a ceiling, compressing turn
   2 down to a 703-token message budget on a model with a 131k window.
   "Largest proven OK" ≠ "largest possible"; the guard needs a floor
   (e.g. never cap below `safe_context × k` or below the declared window
   fraction) or a probe-driven growth path.
3. **chars/4 needs the per-family calibration the plan specified (B3).**
   Counting schemas fixed the qwens but exposed llama3.1's tokenizer
   overcount (p95 49%, all-positive errors); the schema block is also a
   flat 802 guess for a 702–1,065 per-model reality. Both errors are now
   conservative-direction, but the ≤15% p95 target needs per-family
   chars/token and per-model schema rendering costs.
4. **§6 chain tip should be cached, not re-SELECTed (B1.C).** The
   tick+chain SQL (~+75% of a bare insert; the BLAKE3 hash itself is
   1.3 µs) re-reads the previous turn row on every append although the
   store wrote it itself one append earlier. Caching the tip (or batching
   under one statement) would put the §6 overhead inside its <5% budget.
   Absolute cost today (~16–30 µs/turn) makes this low-priority.
5. **Still missing measurement infrastructure:** the 017/018 gauntlet eval
   cases + `active_task_retained` evaluator (B5), the recall golden-set
   corpus (B2), the compression transcript corpus (B4), and the B8 rubric
   run. The features shipped; their quality gates did not.
6. **#255 (DCGM 3 s probe tax) is unchanged** and still dominates every
   cold start on this host — confirmed, attributed, still open.

---

## Reproduce

```bash
# B1 — store bench (no inference; ~2 min)
CARGO_TARGET_DIR=$HOME/.cache/newt-bench-target \
  cargo run --release --manifest-path docs/testing/results/scripts/b1-store-bench/Cargo.toml \
  --bin b1-store-bench

# Build the newt binary + the B7 seeder the live harnesses use
CARGO_TARGET_DIR=$HOME/.cache/newt-bench-target cargo build --release -p newt-agent
CARGO_TARGET_DIR=$HOME/.cache/newt-bench-target cargo build --release \
  --manifest-path docs/testing/results/scripts/b1-store-bench/Cargo.toml --bin b7_seed

# B3 — replay the baseline's pinned corpus with the 18.1 estimator (live, ~25 min)
python3 docs/testing/results/scripts/b3_replay_estimate_after.py --schema-cost \
    /tmp/newt-bench/capture-b3*-*.jsonl
# B3 drift — one 20-turn live session through the capture proxy (~5 min)
python3 docs/testing/results/scripts/ollama_capture_proxy.py --listen 18434 \
    --upstream https://REDACTED-HOST --log /tmp/newt-bench-after/capture-b3-drift-after.jsonl &
bash docs/testing/results/scripts/run_newt_session.sh llama3.1:8b http://127.0.0.1:18434 \
    /tmp/newt-bench/ws-b3 /tmp/newt-bench-after/prompts-drift.txt \
    /tmp/newt-bench-after/b3-drift-after.log 8192

# B5 / B6 (live; ~5 min + ~15 min) — reuses the baseline's seeded
# /tmp/newt-bench/ws-b5 (10×10KB) and ws-b6 (3×50KB) fixtures
bash docs/testing/results/scripts/b56_gauntlet_after.sh b5
bash docs/testing/results/scripts/b56_gauntlet_after.sh b6 10

# B7 — startup (no inference turn; ~3 min) + control
bash docs/testing/results/scripts/b7_startup_after.sh https://REDACTED-HOST 10
bash docs/testing/results/scripts/b7_startup_after.sh http://127.0.0.1:9 10
```

All live steps target `https://REDACTED-HOST` and a sandbox HOME;
they never touch the real `~/.newt`.

## Citability checklist

- [x] Models + quants: llama3.1:8b / qwen2.5-coder:14b / qwen3-coder:30b, all Q4_K_M (identical to baseline)
- [x] Ollama version: 0.20.3 (gnuc's own instance — never DGX/LB; identical to baseline)
- [x] `num_ctx` per benchmark: 8192 (B3) / 4096 (B5, B6) / default (B7)
- [x] Hardware: gnuc — i7-11700B, 30 GiB RAM, RTX 4060 Ti 16 GiB, driver 580.142 (identical to baseline)
- [x] newt SHA: `cf1aa3e` (v0.6.7), release build, rustc 1.96.0
- [x] Command lines: `scripts/*_after.*` + Reproduce section
- [x] Raw numbers committed: appendix tables below
- [x] What is *not* shown: B2 precision (no corpus), B4 (no transcript corpus), B8 rubric (manual, deferred), hard-400 reproduction (needs a strict non-Ollama endpoint — unchanged from baseline)

---

## Appendix — raw tables

### B1 raw (3 runs)

Run 1: turn 1: 369.3 µs · turns 2–10: p50 275.9 / p95 347.4 · 90–110: p50
366.9 / p95 450.0 · 900–1000: p50 194.6 / p95 580.7 / min 92.6 / max
348,906.0 (one checkpoint stall). `verify_chain` p50 3,387.2 / p95 4,371.7.
`list()` @100: p50 102.1 / p95 107.9; @1000: p50 978.5 / p95 1,073.9.
Phase C p50: V0 38.7 / V1 50.7 / V2 67.7 (tick +31.0%, tick+chain +74.9%);
primitives: tick 9.3 / select 6.0 / blake3 1.3.

Run 2: turn 1: 131.9 · 2–10: p50 102.8 / p95 118.2 · 90–110: p50 335.5 /
p95 418.2 · 900–1000: p50 235.0 / p95 584.7 / min 93.6 / max 59,525.3.
`verify_chain` p50 3,418.2 / p95 4,599.5. `list()` @100: p50 145.7 / p95
154.3; @1000: p50 1,007.7 / p95 1,066.5. Phase C p50: V0 42.4 / V1 54.0 /
V2 73.6 (tick +27.3%, tick+chain +73.7%); primitives: tick 23.1 / select
5.8 / blake3 1.3.

Run 3: turn 1: 139.0 · 2–10: p50 110.2 / p95 124.2 · 90–110: p50 104.0 /
p95 116.1 · 900–1000: p50 229.8 / p95 565.7 / min 84.2 / max 49,379.7.
`verify_chain` p50 3,156.9 / p95 5,117.5. `list()` @100: p50 106.6 / p95
112.0; @1000: p50 1,043.8 / p95 1,191.1. Phase C p50: V0 21.7 / V1 27.5 /
V2 38.1 (tick +26.6%, tick+chain +75.5%); primitives: tick 8.4 / select
5.7 / blake3 1.2.

Sizes (all runs): db 4,096 / 4,096 / 389,120 / 4,231,168 bytes at
1/10/100/1000 turns (+ WAL 127,752 / 519,152 / 4,152,992 / 4,185,952);
db 2,146,304 + wal 4,165,352 @100 convs; db 22,511,616 + wal 5,590,872
@1000 convs.

### B3 raw — the same 47 bodies, 18.1 estimator

| model | msgs | est (after) | est (baseline calc) | actual | signed err |
|---|---:|---:|---:|---:|---:|
| llama3.1:8b | 2 | 2482 | 1679 | 2279 | +8.9 % |
| llama3.1:8b | 4 | 5632 | 4829 | 4433 | +27.0 % |
| llama3.1:8b | 4 | 2632 | 1828 | 2381 | +10.5 % |
| llama3.1:8b | 7 | 2872 | 2068 | 1912 | +50.2 % |
| llama3.1:8b | 6 | 2821 | 2016 | 2538 | +11.2 % |
| llama3.1:8b | 8 | 2958 | 2152 | 1985 | +49.0 % |
| llama3.1:8b | 8 | 2938 | 2132 | 2641 | +11.2 % |
| llama3.1:8b | 11 | 7837 | 7030 | 6575 | +19.2 % |
| llama3.1:8b | 2 | 2482 | 1679 | 2281 | +8.8 % |
| llama3.1:8b | 8 | 7760 | 6955 | 6556 | +18.4 % |
| llama3.1:8b | 11 | 11004 | 10199 | 7517 | +46.4 % |
| llama3.1:8b | 13 | 11101 | 10296 | 7601 | +46.0 % |
| llama3.1:8b | 4 | 2812 | 2008 | 2565 | +9.6 % |
| llama3.1:8b | 6 | 3014 | 2209 | 2062 | +46.2 % |
| llama3.1:8b | 6 | 2882 | 2078 | 2615 | +10.2 % |
| llama3.1:8b | 8 | 3015 | 2211 | 2061 | +46.3 % |
| llama3.1:8b | 8 | 2940 | 2135 | 2656 | +10.7 % |
| llama3.1:8b | 10 | 3001 | 2195 | 2042 | +47.0 % |
| llama3.1:8b | 10 | 2990 | 2185 | 2702 | +10.7 % |
| llama3.1:8b | 12 | 4756 | 3950 | 3833 | +24.1 % |
| qwen2.5-coder:14b | 2 | 2482 | 1679 | 2332 | +6.4 % |
| qwen2.5-coder:14b | 4 | 2545 | 1741 | 2386 | +6.7 % |
| qwen2.5-coder:14b | 6 | 2600 | 1795 | 2438 | +6.6 % |
| qwen2.5-coder:14b | 8 | 2664 | 1858 | 2498 | +6.6 % |
| qwen2.5-coder:14b | 2 | 2487 | 1684 | 2338 | +6.4 % |
| qwen2.5-coder:14b | 4 | 2549 | 1745 | 2398 | +6.3 % |
| qwen2.5-coder:14b | 6 | 2605 | 1801 | 2452 | +6.2 % |
| qwen2.5-coder:14b | 8 | 2674 | 1868 | 2510 | +6.5 % |
| qwen2.5-coder:14b | 10 | 2737 | 1931 | 2566 | +6.7 % |
| qwen3-coder:30b | 2 | 2482 | 1679 | 2677 | −7.3 % |
| qwen3-coder:30b | 4 | 5652 | 4849 | 5537 | +2.1 % |
| qwen3-coder:30b | 4 | 2644 | 1840 | 2783 | −5.0 % |
| qwen3-coder:30b | 6 | 2695 | 1890 | 2827 | −4.7 % |
| qwen3-coder:30b | 8 | 2897 | 2091 | 2999 | −3.4 % |
| qwen3-coder:30b | 6 | 2689 | 1884 | 2824 | −4.8 % |
| qwen3-coder:30b | 8 | 2823 | 2018 | 2940 | −4.0 % |
| qwen3-coder:30b | 10 | 5997 | 5191 | 5810 | +3.2 % |
| qwen3-coder:30b | 12 | 6231 | 5425 | 6007 | +3.7 % |
| qwen3-coder:30b | 8 | 2748 | 1942 | 2866 | −4.1 % |
| qwen3-coder:30b | 10 | 2799 | 1993 | 2911 | −3.8 % |
| qwen3-coder:30b | 12 | 4560 | 3752 | 4729 | −3.6 % |
| qwen3-coder:30b | 2 | 2487 | 1684 | 2679 | −7.2 % |
| qwen3-coder:30b | 4 | 2571 | 1767 | 2749 | −6.5 % |
| qwen3-coder:30b | 6 | 2773 | 1968 | 2919 | −5.0 % |
| qwen3-coder:30b | 6 | 2676 | 1872 | 2825 | −5.3 % |
| qwen3-coder:30b | 8 | 2742 | 1937 | 2877 | −4.7 % |
| qwen3-coder:30b | 10 | 2810 | 2004 | 2933 | −4.2 % |

(`actual` = `prompt_eval_count` with cache-buster, fresh replay — within
~±5 tokens of the baseline's actuals on the same bodies, as expected from
the buster. "est (baseline calc)" recomputes the f0f4f6e estimator on the
same body for reference.)

Schema-cost pairs (every 4th body): llama 702/706/705/703/704 ·
qwen2.5 726/721/725 · qwen3 1,062/1,064/1,057/1,063/1,065 measured;
estimator adds 802 in all cases.

### B3 drift raw — tracked per-turn input, 20-turn session (llama3.1:8b)

Footers (tokens "in" per turn): 6,482 · 5,031 · 2,826 · 2,859 · 2,899 ·
2,919 · 2,939 · 2,994 · 3,679 · 3,708 · 3,087 · 3,080 · 3,100 · 3,117 ·
3,117 · 3,062 · 3,097 · 3,169 · 3,182 · 3,126.
Each equals the **max** `prompt_eval_count` among that turn's captured
requests (86 requests total; turn 1 spanned 11 rounds whose prompts were
2,785…6,482 — the baseline accounting would have tracked their SUM,
~41k). Session max = 6,482.
Tuning cache after exit: `max_ok_input: 6482`, `safe_context: 104857`,
`context_window: 131072`, `tune_confidence: high`, `accounting_version: 1`.

### B5/B6 analyzer outputs

B5: `{"chat_requests": 8, "non_2xx": [], "marker_in_last_request": true,
"max_prompt_eval_count": 2708, "biggest_request_est_tokens": 2919,
"biggest_request_evaluated": 2708, "marker_in_last_reply": false,
"empty_response_msgs": 0, "error_lines": 0, "mid_loop_trims": 0,
"pre_send_trims": 0, "compressed_notices": 1, "compression_debug_lines": 1,
"overflow_notices": 0, "antithrash_notices": 0, "refused_sends": 0}` ·
`result.txt: ACTIVE TASK GAUNTLET-7f3d9c done`

B6 per run (`biggest_request_est_tokens → biggest_request_evaluated`,
`marker_in_last_reply`): 41,355→836 yes · 41,378→833 no · 41,364→829 no ·
41,385→835 no · 41,493→923 no · 41,364→826 yes · 41,356→824 no ·
41,359→826 no · 41,380→829 no · 41,380→840 no. All other analyzer fields
zero in all 10 runs (incl. every visibility counter). Post-run tuning
caches: `max_ok_input` 2,042–2,044, `safe_context` 104,857.

# Context/Memory After-Benchmarks @ `cf1aa3e` (issues #246 / #247 / #248)

The "after" companion to the B-series baseline
(`docs/testing/results/context-baseline-f0f4f6e.md`, issue #245): every
B-case the baseline measured, re-run on current `main` (`cf1aa3e`, newt
v0.6.7, all Phase 17–19 implementation merged), same methodology, same
host, same models, same fixtures. The **Comparison** section at the end
puts every number side by side with its baseline.

**TL;DR:**

- **B1 (store write cost):** SQLite `append_turn` is **O(1) in conversation
  length**, measured: p50 ~0.19–0.24 ms at turn 1000 (baseline JSON:
  ~3.4 ms, ≈15–17× faster) with no growth trend from turn 1. `list()` @
  1,000 conversations: p50 ~1.0 ms (baseline ~10 ms). New capabilities
  priced: `verify_chain` @1k turns ~3.4 ms, FTS `search()` with snippets
  ~1.3–2.8 ms warm (<10 ms target met). **On-disk footprint grew**: 4.23 MB
  db (+4.19 MB transient WAL) vs the JSON record's 2.65 MB at 1k turns —
  the FTS index + chain columns are not free space-wise.
- **B1/§6 (tick+chain overhead): the <5% budget is MISSED as stated.**
  Isolated on a replica schema, the per-writer tick + BLAKE3 chain add
  **~+27% / ~+75%** (p50) over a bare insert+update+commit — not <5%. The
  *cryptography* is nearly free exactly as the design doc argued (BLAKE3 of
  a ~2.5 KB turn: **1.3 µs**, ~3–6% of a bare insert); what costs is the
  *extra SQL round-trips* (tick UPDATE+SELECT ~8–23 µs, last-turn SELECT
  ~6 µs). Absolute §6 cost ≈ **16–30 µs/append** — invisible next to the
  baseline's 3.4 ms appends, let alone a multi-second inference call, but
  the result contradicts the <5% framing and is flagged as such.
- **B3 (estimator truth):** same 47 pinned request bodies as baseline,
  re-replayed: median |err| **18.8 % / 6.5 % / 4.4 %**
  (llama3.1 / qwen2.5 / qwen3 — baseline 19.1 / 26.5 / 32.5 %). The
  ≤15 % p95 target is **met on both qwens (6.7 % / 7.2 %)**;
  **llama3.1's p95 got WORSE (49.0 % vs 35.5 %)** — schema-counting
  removed the undercount that had been cancelling llama's chars/4
  overcount, so its errors are now uniformly positive (conservative
  direction, but worse). The **5.4× ratchet poisoning is gone**: a fresh
  20-turn session tracked exactly the largest real prompt
  (6,482 = 6,482, **1.0×**) and `max_ok_input` now stores a number the
  backend actually evaluated.
- **B5 (mid-loop trim):** the compression pipeline **fires now** — one
  visible `context compressed: ~1,593 → ~1,190 (prune + summary, still
  over budget)` notice mid-loop where the baseline had zero events ever.
  Honest caveats: the model skipped the file reads this run (no overflow
  pressure built — nondeterminism, not management), and the trigger was
  an *over-eager* `max_ok_input` cap from turn 1 (fix candidate #2).
- **B6 (truncation honesty — THE headline): NOT FIXED for the measured
  scenario. 2/10 correct, 0/10 visibly degraded, 8/10 silently wrong**
  (baseline 1/0/9 — statistically unchanged). All 10 runs shipped a
  ~41.4k-token request into the forced 4,096 window with zero events;
  Ollama evaluated ~830 tokens of it. Root cause measured and code-traced:
  the pre-send guard keys on `max_ok_input ∥ safe_context` (104,857 from
  `/api/show`), and **nothing feeds the `num_ctx` newt itself sends in
  the same request body into any budget**. The machinery works from a
  session's second turn (B5 proves it); the fresh-cache first turn —
  exactly the baseline protocol — is unprotected. Fix candidate #1.
- **B7 (cold start):** flat **3.02 s at 0 / 100 / 1,000 stored
  conversations** — the 3-second DCGM probe tax (#255) is **unchanged**, as
  expected (still open). Behind it, auto-resume (17.7) now actually reads
  the store at startup and picks the latest-by-activity-tick conversation:
  measured against a refused-port control, the whole startup is 12.1 ms at
  0 conversations and 14.2 ms at 1,000 — **resume costs ~+2 ms at 1,000
  conversations**, far inside the <50 ms budget. No WAL-fallback notice on
  this host (local ext4 — WAL takes; the notice path is NFS-only).
- **B2 (recall quality):** the *feature* now exists (17.3–17.5) and its
  latency is measured under B1 (p95 ≤ ~3 ms @1k turns), but the planned
  seeded corpus + 20-query golden set (`newt-eval/fixtures/recall-corpus/`)
  was **not built** by the landed PRs — precision@3 remains unmeasured.
  Recorded as such, not invented.
- **B4 (compression efficiency):** still unmeasured as specified — the
  10-transcript anonymized fixture corpus never materialized. The live
  compression notices in B5/B6 (`tokens_before → tokens_after` per firing)
  are the observational stand-in; a real B4 needs its corpus.
- **B8 (memory write quality):** the 19.3 nudge + write-scan now exist, but
  B8 is a human-scored rubric over 10 scripted sessions — not run for this
  delta. The B5/B6/B3-drift sessions ran with the nudge active and wrote
  zero notes (quiet sessions — consistent with the design's
  zero-notes-in-quiet-sessions target, but not a rubric result).

---

## Environment (citability)

| | |
|---|---|
| newt | v0.6.7, workspace @ `cf1aa3e` (`bench-context-after`), release build |
| Baseline compared | `context-baseline-f0f4f6e.md` (@ `f0f4f6e`, v0.6.7, same host) |
| Host (all runs) | gnuc (`geforcenuc`): i7-11700B (16 threads), 30 GiB RAM, RTX 4060 Ti 16 GiB (driver 580.142), Linux 6.8.0-111-generic |
| Inference | gnuc's own Ollama **0.20.3** via `https://gnuc-ollama.home.lab` (operator rule: never DGX/LB for these runs) — same version as baseline |
| Models (pinned) | `llama3.1:8b` Q4_K_M (4.9 GB) · `qwen2.5-coder:14b` Q4_K_M (9.0 GB) · `qwen3-coder:30b` Q4_K_M (18.6 GB) — same as baseline |
| B1 disk | tempdir on local ext4 (LVM); SQLite WAL + `synchronous=NORMAL` (what the store actually applies) |
| Toolchain | rustc 1.96.0, hyperfine 1.20.0, Python 3.12.3, bundled SQLite (rusqlite 0.31) |
| Live-session harness | identical to baseline: sandbox `$HOME` per run (never the real `~/.newt`), `NEWT_DEBUG=1`, `TERM=dumb`, prompts piped on stdin; capture proxy on `127.0.0.1:18434` (`scripts/ollama_capture_proxy.py`, unchanged) |
| `num_ctx` | 8192 for B3 sessions; **4096** for B5/B6 (the squeeze); model default for B7 (no inference turn) — same as baseline |
| B3 corpus | the baseline's pinned capture logs (`capture-b3*-*.jsonl`, 47 unique bodies) replayed verbatim — the baseline kept them precisely so the 18.1 delta would compare identical requests |

Scripts: `docs/testing/results/scripts/` — `*_after.*` variants beside the
baseline harnesses (the b1 bin was rewritten in place for the SQLite store;
the baseline JSON version is in git history at `7aaa724`).

---

## B1 — turn-write cost & store size

### Method

Same scratch bin (`scripts/b1-store-bench/`), updated for the SQLite store:

- **Phase A:** unchanged — one conversation, 1,000 `append_turn` calls with
  the byte-identical deterministic 1–4 KB payloads; p50/p95 over the same
  windows; on-disk size at each N (now `conversations.db` + its WAL — the
  WAL is transient, checkpointed back into the db).
- **Phase A2 (new):** `verify_chain()` (10 iters) and FTS `search()` with
  snippets (5 queries × 20 warm iters, limit 10) at 1,000 turns.
- **Phase B:** unchanged — `list()` at 100 / 1,000 conversations × 5 turns,
  20 warm iterations.
- **Phase C (new — the §6 isolation):** tick+chain are not toggleable in
  the production store (by design), so a **replica** of the store's schema
  and pragmas (same `turns`/`conversations`/`writer_clock` DDL, WAL +
  `synchronous=NORMAL`, **no FTS triggers**) runs 1,000 appends per variant,
  the three variants interleaved per-turn so background drift (WAL
  checkpoints, writeback) lands on all three equally:
  V0 bare insert+update+commit · V1 +Lamport tick (the store's `next_tick`
  SQL) · V2 +content chain (last-turn SELECT + BLAKE3 over the replicated
  v1 canonical encoding). p50 is the comparison statistic — per-append
  commits have a multi-ms checkpoint-stall tail that makes means lie (first
  attempt used sequential variants + means and produced sign-flipping
  "overheads" between −31% and +34%; the interleaved p50 design below
  reproduces within a few points across runs).

3 runs (`/tmp/newt-bench-after/b1-run{1,2,3}.txt`, appendix).

### Results

`append_turn` latency (µs), 3 runs:

| N (window) | p50 r1 | p50 r2 | p50 r3 | p95 r1 | p95 r2 | p95 r3 |
|---|---:|---:|---:|---:|---:|---:|
| 1 (turn 1) | 369.3 | 131.9 | 139.0 | — | — | — |
| 10 (turns 2–10) | 275.9 | 102.8 | 110.2 | 347.4 | 118.2 | 124.2 |
| 100 (turns 90–110) | 366.9 | 335.5 | 104.0 | 450.0 | 418.2 | 116.1 |
| 1000 (turns 900–1000) | 194.6 | 235.0 | 229.8 | 580.7 | 584.7 | 565.7 |

On-disk size (identical across runs — deterministic payloads):

| N turns | db (bytes) | wal (bytes) |
|---:|---:|---:|
| 1 | 4,096 | 127,752 |
| 10 | 4,096 | 519,152 |
| 100 | 389,120 | 4,152,992 |
| 1000 | 4,231,168 | 4,185,952 |

`verify_chain` @ 1,000 turns (10 iters): p50 **3.16–3.42 ms**, p95
4.37–5.12 ms across the 3 runs.

FTS `search()` @ 1,000 turns (limit 10, snippets on, warm, run 1; runs 2–3
within ~10%):

| query | hits | p50 (µs) | p95 (µs) |
|---|---:|---:|---:|
| `failing test` | 10 | 1,432 | 1,756 |
| `blake3` | 10 | 1,301 | 1,613 |
| `regenerates` | 10 | 2,795 | 3,004 |
| `"cargo test"` | 10 | 1,378 | 1,524 |
| `#245` (sanitizer path) | 10 | 1,210 | 1,266 |

`list()` warm, 20 iters:

| conversations | p50 (µs) | p95 (µs) | store size |
|---:|---:|---:|---:|
| 100 | 102–146 | 108–154 | 2.15 MB db + 4.17 MB wal |
| 1000 | 979–1,044 | 1,067–1,191 | 22.5 MB db + 5.6 MB wal |

§6 isolation (Phase C, replica schema, p50 of 1,000 interleaved appends):

| variant | r1 p50 (µs) | r2 p50 (µs) | r3 p50 (µs) |
|---|---:|---:|---:|
| V0 bare insert+update+commit | 38.7 | 42.4 | 21.7 |
| V1 V0 + Lamport tick | 50.7 | 54.0 | 27.5 |
| V2 V1 + BLAKE3 chain | 67.7 | 73.6 | 38.1 |
| **overhead, tick** | +31.0% | +27.3% | +26.6% |
| **overhead, tick+chain** | **+74.9%** | **+73.7%** | **+75.5%** |

§6 primitive microbench (1,000 iters each, on the populated replica):

| primitive | p50 (µs) |
|---|---:|
| tick UPDATE+SELECT | 8.4–23.1 |
| last-turn SELECT | 5.7–6.0 |
| BLAKE3 canonical hash (~2.5 KB turn) | **1.2–1.3** |

### Interpretation (honest)

- **O(1) confirmed, with numbers:** no growth trend from turn 1 to turn
  1000 (the windows wobble ~100–370 µs with page-cache/checkpoint noise;
  the baseline's 50× turn-1→turn-1000 growth is gone). At the turn-1000
  window this is **~15–17× faster** than the JSON rewrite (p50 195–235 µs
  vs 3,318–3,437 µs) and the gap widens with conversation length —
  that was the entire point of 17.1.
- **`list()` is ~10× faster** at 1,000 conversations (p50 ~1.0 ms vs
  ~10 ms) and no longer reads+parses every record.
- **The <5% §6 budget is missed, and the miss is structural, not crypto:**
  the design doc's "blake3 is nearly free" claim is *confirmed* (1.3 µs ≈
  3–6% of a bare insert — inside the budget on its own). What the budget
  did not price is the two extra SQL statements per append (`next_tick`'s
  UPDATE+SELECT and the `last_turn` SELECT for the chain): together they
  roughly *halve* append throughput on the replica (+~75% p50 cost).
  In absolute terms the §6 work is ~16–30 µs per turn — about 0.5–1% of
  the baseline's 3.4 ms JSON append it replaced, and unmeasurable next to
  any inference call — but a future hot path (mesh sync, bulk import)
  should batch or cache the chain tip instead of re-SELECTing it per
  append. The result wins over the design doc's framing; flagged as a fix
  candidate below.
- **Disk footprint regression, stated plainly:** 4.23 MB db at 1k turns vs
  2.65 MB JSON (+60%; the FTS index, the chain columns and SQLite paging
  all cost), and 22.5 MB vs 13.6 MB at 1,000×5-turn conversations. The WAL
  adds ~4–6 MB transient. Nobody promised smaller; the baseline number is
  beaten on time, not space.
- Max-latency outliers (one 349 ms append in run 1's 900–1000 window;
  49–60 ms in runs 2–3) are WAL-checkpoint/writeback stalls on a busy dev
  box — the p95s stay under 0.6 ms.
- Same caveat as baseline: tempdir + page cache, `synchronous=NORMAL` under
  WAL means group-commit durability, not per-append fsync. Honest
  comparison: the JSON store didn't fsync either.

---

## B2 — recall quality & latency

**Feature exists now; the planned quality measurement does not.** 17.3–17.5
shipped the FTS5 index, the sanitizer, `/recall`, and the model-facing
`recall` tool, and B1.A2 above prices the latency (p95 ≤ ~3 ms warm at 1k
turns with snippets — comfortably under the plan's <10 ms target). But the
plan's **seeded fixture corpus + 20-query golden set**
(`newt-eval/fixtures/recall-corpus/`, precision@3) was not built by any
landed PR, so **recall *quality* on realistic queries remains unmeasured**.
What exists instead: unit-level sanitizer/format tests riding `cargo test`
(adversarial inputs included). Keeping the baseline's discipline: recorded
as a gap, not papered over with the unit tests.

---

## B3 — token-estimate accuracy

### Method

Same probe, deliberately on the **same requests**: the baseline kept its
capture logs (47 unique `/api/chat` bodies across the three models)
precisely so 18.1 could be scored on identical inputs. This re-run replays
those bodies verbatim (`scripts/b3_replay_estimate_after.py`) — same
cache-buster, same `num_predict: 1`, same upstream — and recomputes the
estimate as the **current fallback estimator** does
(`trim.rs::estimate_request_tokens` @ cf1aa3e: per-message ceil-div
chars/4 **plus the serialized tool schemas**, which the baseline estimator
ignored). Schema-cost replays (every 4th body, `tools` stripped) now also
record what the estimator *adds* for the schema block, so the schema
correction itself is scored.

What this measures is the **fallback path** (first dispatch, no backend
report). 18.1's other half — anchoring on backend-reported prompt tokens
(`PromptTracker`) — is exact by construction *within* a turn; its session
truth is measured by the **drift check**: one fresh 20-turn scripted
session (`llama3.1:8b`, `num_ctx` 8192, same trivial prompts) on the new
binary, comparing newt's tracked per-turn input and the tuning-cache
ratchet against every backend-evaluated prompt in the capture.

### Results

Fallback-estimator error on the pinned corpus, |est−actual|/actual
(baseline values in parentheses):

| model | n | median | p95 | mean signed (est−actual)/actual |
|---|---:|---:|---:|---:|
| llama3.1:8b | 20 | **18.8 %** (19.1 %) | **49.0 %** (35.5 %) | **+25.6 %** (−3.1 %) |
| qwen2.5-coder:14b | 9 | **6.5 %** (26.5 %) | **6.7 %** (28.1 %) | +6.5 % (−26.6 %) |
| qwen3-coder:30b | 18 | **4.4 %** (32.5 %) | **7.2 %** (37.2 %) | −3.6 % (−28.9 %) |

Tool-schema block, measured vs what the estimator now adds (13 paired
replays):

| model | measured schema tokens | estimator adds |
|---|---:|---:|
| llama3.1:8b | 702–706 | 802 (+14 %) |
| qwen2.5-coder:14b | 721–726 | 802 (+11 %) |
| qwen3-coder:30b | 1,057–1,065 | 802 (−25 %) |

**Drift check (the 5.4× poisoning), fresh 20-turn session, 86 captured
requests:**

| quantity | baseline | after |
|---|---:|---:|
| largest single prompt the backend evaluated | 4,748 | **6,482** |
| largest per-turn "input" newt tracked | 25,602 | **6,482** |
| tracked ÷ largest real prompt | **5.4×** | **1.0×** |
| tuning cache `max_ok_input` after session | 25,602 (`high` confidence) | **6,482** (`high` confidence, `accounting_version: 1`) |

The per-turn footers now report the **max** single prompt of the turn
(verified turn-by-turn against the capture: e.g. the largest turn's footer
`6,482 in` = that turn's largest `prompt_eval_count`, not the old
probe+stream+rounds sum), and the ratchet records a number the backend
actually evaluated.

### Interpretation (honest)

- **The 18.1 target (p95 ≤ 15 %) is met on both qwens** (6.7 % / 7.2 %) —
  counting the schema block removed almost the entire structural
  undercount, exactly as the baseline's error analysis predicted.
- **It is NOT met on llama3.1: p95 *worsened* 35.5 % → 49.0 %.** Stated
  plainly: the baseline's two llama error sources (schema-blind undercount
  vs chars/4 *overcounting* llama's tokenizer on dense English) partially
  cancelled; fixing the first leaves the second uncompensated, so llama
  errors are now uniformly positive (+8.8 % … +50.2 %). Two mitigating
  facts, not excuses: (1) the error is now **one-sided conservative** —
  an overcount fires compression early; it can never let a request sneak
  past the window, which is the failure direction that caused #223 —
  the baseline's mixed-sign errors included dangerous *under*counts;
  (2) the median is unchanged (18.8 %). The fix the data asks for is the
  plan's per-family chars/token calibration, which 18.1 did not ship.
  Flagged as a fix candidate below.
- The flat **802-token schema estimate** is a single chars/4 guess for a
  per-model quantity (llama ~704, qwen3 ~1,061 — qwen3's chat template
  renders schemas more verbosely). Right order of magnitude everywhere,
  ~25 % under on qwen3 — same per-family-calibration fix.
- **The ratchet de-poisoning is complete and verified** (1.0× vs 5.4×):
  `merge_round_usage` keeps the max, the migration
  (`accounting_version: 1`) invalidates old poisoned entries, and the
  pre-send guard now keys on a number that was genuinely evaluated. The
  consequence shows up in B5 below — the guard *fires*, which is the
  point — including one over-eager edge worth knowing about
  (see B5 interpretation).
- Same caveats as baseline: Ollama tokenizers, cache-buster adds ~20
  tokens (noise), replay is the same pinned set so model-behavior
  nondeterminism is excluded by construction.

---

## B4 — compression efficiency

**Still unmeasured as specified, same justification as the baseline's
implicit skip:** B4 requires 10 anonymized real transcripts (15–60K tokens)
committed as fixtures; that corpus does not exist on `main` today. The
compression pipeline's per-firing reclaim is visible in the B5 results
below (`context compressed: ~X → ~Y` notices) as an observational
stand-in, but a corpus-based prune-vs-summary split (the hermes "prune
does most of the reclaim" check) still needs its fixtures. Open follow-up.

---

## B5 — long-horizon "compression gauntlet" probe

### Method

Identical scripted probe (`scripts/b56_gauntlet_after.sh b5` — same model,
`num_ctx=4096`, same `ACTIVE TASK GAUNTLET-7f3d9c` marker, same two-turn
prompt file, same seeded `ws-b5` fixture files as the baseline run), fresh
sandbox HOME. Analysis via `b56_analyze_after.py`: the baseline analyzer's
`mid-loop trim:` / `pre-send trim:` greps match strings 18.4 deleted, so
the after-analyzer counts the new visibility surface (`context
compressed:` notices, compression debug lines, overflow/anti-thrash
notices, refused sends) alongside all baseline fields.

### Results

| metric | baseline | after |
|---|---|---|
| `/api/chat` requests | 6 (2 turns) | 8 (2 turns) |
| non-2xx responses | 0 | 0 |
| biggest request: estimated → backend-evaluated | **27,434 → 3,059** | **2,919 → 2,708** |
| newt compression events (visible / debug) | **0 / 0** | **1 / 1** |
| `result.txt` written with marker | yes | yes |
| marker in last request body | yes | yes |
| marker restated from memory (turn 2, displayed reply) | yes | **no** (probe round generated the restatement; the displayed streamed reply was degenerate `<|python_tag|>` pseudo-tool-call text) |

The compression event, verbatim:

```
⧉  context compressed: ~1,593 → ~1,190 est. tokens (prune + summary, still over budget)
[debug] compression: 6 → 6 messages (budget ~703 tokens, +~1432 tool-schema tokens ride along)
```

### Interpretation (honest)

- **The trim path engages now.** Baseline B5's central finding was that
  newt's own context machinery never fired under pressure (0 events, the
  backend silently absorbed a 27k request). In this run the shared
  compression pipeline fired mid-loop with the full visible notice — and
  honestly reported "still over budget" when prune+summary couldn't reach
  the 703-token target.
- **But the run is not comparable head-to-head, and saying otherwise would
  be dishonest:** llama3.1 *skipped the ten file reads entirely* this time
  (it wrote `result.txt` immediately — twice — without reading anything),
  so the ~100 KB overflow pressure the baseline measured never built.
  Model nondeterminism, not context management. The marker landing in
  `result.txt` is therefore model laziness producing a "pass", same
  caveat the baseline attached to its own "task completed".
- **Why compression fired at a mere ~3k tokens:** turn 1's largest real
  prompt (2,135 tokens) ratcheted `max_ok_input = 2135`, and turn 2's
  pre-send guard then enforced budget = 2,135 − 1,432 schema tokens =
  703 message tokens. The de-poisoned ratchet (B3) is *truthful* but its
  consumption as a **cap** is over-eager: one small first turn caps every
  later turn at "the largest prompt I happen to have proven", compressing
  sessions that are nowhere near any real limit. Fix candidate below.
- The displayed-reply degeneration (probe content had the restatement;
  the streamed re-issue emitted `<|python_tag|>` garbage) is a
  probe-vs-stream divergence on the weakest pinned model — visible in the
  baseline too (its post-overflow round). Scored against newt as "not
  restated" per protocol.
- Single run on the weakest model; same no-p-values caveat as baseline.
  The real 017/018 eval cases (with `active_task_retained`) still don't
  exist — that gap survives Phases 17–19. Fix candidate below.

---

## B6 — overflow-400 incidence (#223 class)

### Method

Identical to baseline: 10 scripted single-turn sessions
(`scripts/b56_gauntlet_after.sh b6 10`), `llama3.1:8b`, `num_ctx=4096`
forced via `NEWT_NUM_CTX`, fresh sandbox HOME per run, same three ~50 KB
`ws-b6` fixture files, same ACTIVE-TASK-marker question. Scoring per the
brief: **n/10 correct** (marker in the displayed answer), **n/10 visibly
degraded** (any compression/overflow/anti-thrash/refusal notice), **n/10
silently wrong** (wrong answer, no notice anywhere).

### Results

| run | reqs | non-2xx | overflow req: est → evaluated | visible events | marker in answer |
|---:|---:|---:|---|---:|---|
| 1 | 3 | 0 | 41,355 → 836 | 0 | **yes** |
| 2 | 3 | 0 | 41,378 → 833 | 0 | no |
| 3 | 3 | 0 | 41,364 → 829 | 0 | no |
| 4 | 3 | 0 | 41,385 → 835 | 0 | no |
| 5 | 3 | 0 | 41,493 → 923 | 0 | no |
| 6 | 3 | 0 | 41,364 → 826 | 0 | **yes** |
| 7 | 3 | 0 | 41,356 → 824 | 0 | no |
| 8 | 3 | 0 | 41,359 → 826 | 0 | no |
| 9 | 3 | 0 | 41,380 → 829 | 0 | no |
| 10 | 3 | 0 | 41,380 → 840 | 0 | no |

**Correct: 2/10. Visibly degraded: 0/10. Silently wrong: 8/10.**
(Baseline: 1/10 correct, 0/10 visible, 9/10 silently wrong — with run
counts this small, 2 vs 1 correct is noise, not signal.)

### Interpretation (honest — this is the loudest flag in the document)

- **The headline target — "every run either completes or visibly degrades
  through newt-side compression" — is NOT met. The B6 scenario is
  statistically unchanged from baseline.** A request newt itself estimates
  at ~41k tokens went to a model newt itself addressed with
  `options.num_ctx = 4096`, in all 10 runs, with zero compression, zero
  warnings; Ollama silently evaluated the last ~830 tokens and the model
  answered about raw file contents. The design doc's expectation loses to
  the measurement.
- **Why, precisely (code-and-data-confirmed):** the compression triggers
  key on `send_budget = max_ok_input ∥ safe_context`. On a fresh cache,
  `max_ok_input` is unset until the turn *ends* (the post-run caches
  record a truthful 2,042–2,044 — too late for a single-turn session) and
  `safe_context` bootstraps from `/api/show`'s declared 131,072-token
  window → 104,857. The **operator-forced `num_ctx=4096` is sent in the
  request body but never feeds the budget**, the token trim threshold
  defaults to off, and 3 reads can't reach the message-count trigger. So
  every guard 18.x added is armed with numbers that say "fine" while the
  request is 10× over the real window.
- **What did improve, and where the improvement is visible:** the same
  machinery *does* fire from the second turn of a session on (B5: the
  ratchet from turn 1 armed the guard, compression ran with a visible
  notice, honestly labelled "still over budget"). The B6 failure is
  specifically the **fresh-cache first-turn + forced-num_ctx** hole —
  newt knows the number (it puts it in the request!) and doesn't use it.
  Fix candidate #1 below; it is a one-line-ish budget wiring
  (`send_budget = min(send_budget, eff_num_ctx)`), not a redesign.
- The literal #223 hard-400 still does not reproduce against Ollama
  (0 non-2xx in 10 runs) and still *cannot* from this gnuc-only bench —
  unchanged from baseline; the cw-400 recovery path (parse → tighten →
  compress → retry) exists but is exercised only by unit tests here.
- Turn accounting in these runs is at least truthful now: each run's
  footer tracked the **max** single prompt (2,04x), not the baseline's
  inflated sum — so the post-run ratchet writes honest caps. In a
  *multi-turn* version of this scenario, turn 2 would be guarded (that is
  exactly what B5 showed). The single-turn case is the worst case and the
  baseline's protocol measures precisely it.

---

## B7 — resume & startup cost

### Method

Same as baseline (`hyperfine`, 2 warmups, 10 runs, `exit` piped, sandbox
HOMEs, `env -i`), with one forced change: the seeder now drives the **real
SQLite store API** (`b7_seed` bench bin) instead of writing the retired
JSON schema — seeding JSON today would measure the one-time legacy import,
not steady-state startup. 0 / 100 / 1,000 conversations × 10 turns
(25.6 MB db + 4.6 MB wal at 1,000). Auto-resume (17.7) is on by default,
so startup now genuinely opens the store and resumes the
latest-by-activity-tick conversation — verified in the captured session
output:

```
>  resumed conversation 178121173318  synthetic conversation 999  (10 turns, …) — /new starts fresh
```

Control runs against a connection-refused URL (`http://127.0.0.1:9`)
isolate the store cost from the network-probe constant, exactly as the
baseline's control did. (`scripts/b7_startup_after.sh`.)

### Results

Live URL (`https://gnuc-ollama.home.lab`):

| stored conversations | mean | σ | min … max |
|---:|---:|---:|---|
| 0 | 3.023 s | 0.005 s | 3.017 … 3.031 s |
| 100 | 3.026 s | 0.008 s | 3.010 … 3.033 s |
| 1000 | 3.026 s | 0.008 s | 3.008 … 3.035 s |

Refused-port control (`http://127.0.0.1:9` — no DCGM probe wait, no
`/api/show`):

| stored conversations | mean | σ |
|---:|---:|---:|
| 0 | 12.1 ms | 1.0 ms |
| 100 | 12.4 ms | 0.1 ms |
| 1000 | 14.2 ms | 0.2 ms |

### Interpretation (honest)

- **The 3.02 s cold-start constant is unchanged and is still #255** (the
  DCGM telemetry probe's 3-second timeout against a black-holed :9400).
  Attributed, not hidden: it dominates everything else at startup on this
  host and is exactly as the baseline measured it. #255 remains open.
- **Auto-resume costs ~2 ms at 1,000 stored conversations** (control:
  12.1 → 14.2 ms total process lifetime including store open, WAL setup,
  resume query, and record load). The 17.7 budget ("resume adds <50 ms at
  1,000 conversations") is met with ~25× headroom. Resume correctness at
  this scale: the banner names the highest-tick conversation
  (`synthetic conversation 999`), per §6 — activity tick, not timestamp.
- The WAL-fallback startup notice (17.4/N7) did not appear — correct
  behavior on local ext4 where WAL takes. Exercising the notice needs an
  NFS home; out of scope here, covered by unit tests.
- The multi-workspace / clock-skew / `--ephemeral` acceptance items are
  17.7 unit/integration-test territory and were not re-benchmarked here.

---

## B8 — memory write quality

**Feature exists now (19.3 nudge + 19.2 write-scan + 19.4 close-time
extraction); the B8 rubric was not run.** B8 as specified is 10 scripted
live sessions (5 quiet / 5 fact-rich) scored by a human against a
committed rubric — that protocol needs the operator and is deferred. What
this delta can honestly say: the B5/B6/B3-drift sessions all ran with the
nudge active at its default interval and **wrote zero notes** (verified:
no note files in the sandbox HOMEs) — consistent with the
"zero notes in quiet sessions" target, but it is an observation from
sessions designed for other measurements, not a rubric result.

---

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
    --upstream https://gnuc-ollama.home.lab --log /tmp/newt-bench-after/capture-b3-drift-after.jsonl &
bash docs/testing/results/scripts/run_newt_session.sh llama3.1:8b http://127.0.0.1:18434 \
    /tmp/newt-bench/ws-b3 /tmp/newt-bench-after/prompts-drift.txt \
    /tmp/newt-bench-after/b3-drift-after.log 8192

# B5 / B6 (live; ~5 min + ~15 min) — reuses the baseline's seeded
# /tmp/newt-bench/ws-b5 (10×10KB) and ws-b6 (3×50KB) fixtures
bash docs/testing/results/scripts/b56_gauntlet_after.sh b5
bash docs/testing/results/scripts/b56_gauntlet_after.sh b6 10

# B7 — startup (no inference turn; ~3 min) + control
bash docs/testing/results/scripts/b7_startup_after.sh https://gnuc-ollama.home.lab 10
bash docs/testing/results/scripts/b7_startup_after.sh http://127.0.0.1:9 10
```

All live steps target `https://gnuc-ollama.home.lab` and a sandbox HOME;
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

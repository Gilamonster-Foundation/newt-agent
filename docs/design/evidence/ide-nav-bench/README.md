# IDE-for-LLMs navigation bench — the #548 decision gate (#1286)

> **Status: protocol + skeleton, awaiting the live run.** This is the empirical
> half of #1277 conformance (spec `docs/spec/semantic-cheat.md` §6.3). Everything
> held *below the line* — focus lens (PR-4), crew payload (PR-5/D4), ANN, L3 — is
> funded / held / dropped by **this data, not instinct**. Fill the results tables
> from a live run, then post the decision record to #1277.

## The observed failure this prices

Ornith-1.0-35B, on the #548 rollup task, **guesses file paths from training
priors** (`dgx.rs`) instead of navigating the actual tree. The navigation stack
merged in Phases 1–3 exists to convert that guess into a *grounded* first move.
The bench measures whether — and by how much — each mechanism does so.

## Arms (what shipped, in utility order)

The code for every arm below the map floor is **merged on `main`** as of this
protocol; the bench measures the *shipped* artifacts, not prototypes.

| # | Arm | Ships in | Hypothesis it tests |
|---|-----|----------|---------------------|
| A0 | **baseline** (no map, no retrieval, fixed 3k surface) | pre-#1277 | the guess-from-priors failure reproduces |
| A1 | **honest gather** floor | #1281 | deterministic walk + declared cuts — the precondition for every arm (no silent truncation confound) |
| A2 | **code_search** on (retrieval by meaning) | #1279/#1280 | recall lowers rounds-to-first-correct-file when the name is unknown |
| A3 | **project map** in frozen head | #1284 | the untruncatable unit list kills the priors-guess on turn one |
| A4a/b/c | **map-size** 16k / 32k / 64k chars (D1) | #1283/#1284 | degradation curve vs RULER/lost-in-middle; the winning arm → the shipped family-default ceiling pin |
| A5 | **where_is** + miss-injection | #1285 | halve exploratory rounds (target); queries for nonexistent + cut-region symbols → confabulated-path rate, rounds-to-honest-miss, cut-flip honesty |
| A6 | **lens** auto-focus ON/OFF (A/B) | *held — PR-4* | prices the D3 default: rounds saved vs prefill latency (KV-cache invalidation per swap), per benched family |
| A7 | **L2.5 crew payload** (*iff funded — D4*) | *held — PR-5* | leaf rounds saved; precondition: SC-L7 filter + snapshot coherence |

A6/A7 are the **held items** — run them only if A0–A5 justify the investment;
their columns feed the decision record below.

## Metrics (§6.3)

Per session, against a declared **correct-file set** for the task (below):

- **rounds-to-first-correct-file** — *primary*. Tool-call rounds until the model
  first reads/locates a file in the correct set. Lower is better.
- **total rounds to PR** — full agentic rounds to a passing change.
- **first-tool-call-correct-crate rate** — did round 1 land in the right unit?
- **confabulated-path rate** — fraction of file references to a path that does
  not exist (the priors-guess signal). Lower is better.
- **rounds-to-honest-miss** — for A5 miss-injection: rounds until the model
  accepts `NoSuchSymbol` / `NotGathered` instead of hunting a phantom.
- **prefill latency** (A6 only) — added KV-cache invalidation cost per focus swap.

## Design (§6.3)

- **Models:** Ornith-1.0-35B (the failure subject) **vs** Claude (the ceiling).
  Ornith serves on the DGX; see `~/ornith.sh` / the dgx1 router memory.
- **Repetition:** **N ≥ 5** repeats per (arm × model), **paired** — same task,
  same seed discipline, same base ref; only the arm's mechanism varies.
- **Controlled variable:** `max_symbols_per_file` is **held constant** across the
  map-size arms (A4) — the arms vary *breadth*, deliberately not depth. A
  depth-scaling arm is a recorded follow-up, not part of this gate.
- **Scale:** ≈ (3 map-size + 5 fixed arms) × 2 models × N=5 ≈ **80 agentic runs**.
- **D1 ceiling rule:** the **smallest** map-size arm whose primary metric is
  within one standard error of the best becomes the shipped family-default
  `ceiling_chars` pin (model card #852). Smaller ties win — cheaper is better
  when it costs nothing.

## Run recipe (turnkey for the live session)

Each run is one newt session on the #548 task, per arm, captured so the metrics
are recoverable. **Note:** the persisted `ConversationStore` digests tool-arg
paths for privacy (`ToolEvent.args_digest` — key names + BLAKE3, never raw
values), so **rounds-to-first-correct-file must be read from the live capture**,
not the store.

1. **Pin the arm.** Toggle the mechanism via config, not code:
   - A0 baseline: `[context] api_surface` off, `code_search` off, no map.
   - A3+: `knowledge_base` technique on (map + surface); `where_is` is always-on.
   - A2: `newt models pull-embed` first (#1279), then semantic on.
   - A4a/b/c: set `[context.api_surface] ceiling_chars = 16000 | 32000 | 64000`
     (hold `max_symbols_per_file` fixed).
2. **Capture the session.** Run under `tmux` (the standing UAT harness) or with
   `--trace`, and `tee` the terminal to
   `runs/<arm>-<model>-<repeat>.log` so every `read_file` / `where_is` /
   `code_search` call with its path is on disk.
3. **Tally** each session into `results.csv` (schema below) — one row per run.
4. **Score:** `python3 score.py results.csv` → per-arm means, paired A→B deltas,
   confab rates, and the D1 ceiling pick. Paste its table into "Results" below.
5. **Decide:** fill the decision record; post it to #1277; land the family
   default as a model-card datum (#852).

### `results.csv` schema

```
arm,model,repeat,rounds_to_first_correct_file,total_rounds,first_call_correct_crate,confabulated_paths,total_path_refs,rounds_to_honest_miss,prefill_ms
A0,ornith,1,,,,,,,
```

- `first_call_correct_crate`: `1`/`0`. `confabulated_paths`/`total_path_refs`:
  counts (the score is their ratio). Blank cells are treated as missing, not `0`.
- A template with all (arm × model × N=5) rows pre-seeded is in
  `results.template.csv` — copy it to `results.csv` and fill.

## The task (declared ground truth)

**#548 rollup task** — *(fill the exact prompt + the correct-file set before the
run; keep them here so the bench is reproducible).*

- **Prompt:** `<the #548 task, verbatim>`
- **Correct-file set (glob):** `<e.g. newt-core/src/dgx*.rs, newt-cli/src/dgx*.rs>`
- **Miss-injection queries (A5):** `<a nonexistent symbol>`, `<a cut-region symbol>`

## Results

*(paste `score.py` output here after the run)*

| arm | model | N | rounds-to-first-correct (mean ± se) | Δ vs prior arm | confab rate | honest-miss rounds |
|-----|-------|---|-------------------------------------|----------------|-------------|--------------------|
| A0 | ornith | | | — | | |
| … | | | | | | |

**D1 ceiling pin (from A4a/b/c):** `<winning ceiling_chars>` — the smallest arm
within 1 se of the best.

## Decision record — the held items (post to #1277)

Each verdict must cite the arm that justifies it.

| Held item | Verdict | Justifying arm + number |
|-----------|---------|-------------------------|
| **Focus lens** (PR-4, D3) | fund / hold / drop | A6: rounds saved `<x>` vs prefill `<y>` ms |
| **Crew L2.5 payload** (PR-5, D4) | fund / hold / drop | A7 (iff run): leaf rounds saved `<x>` |
| **ANN index** | fund / hold / drop | crossover vs exact search at the observed corpus size |
| **L3 jail** | hold (unchanged) | out of scope for this gate |

**Family default landed:** `ceiling_chars = <pin>` for the Ornith family (#852
model card); other families inherit until benched (§6.3 "per benched family").

---
*Protocol authored for #1286 (Phase 4 of #1277). Spec: `docs/spec/semantic-cheat.md`
§6.2–6.3, §9 (D1–D4). Scorer: `score.py`. Fill from the live run, then this file
is the bench report of record.*

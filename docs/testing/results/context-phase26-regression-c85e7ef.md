# Phase 26 regression — agentic scorecard vs dgx1 @ `c85e7ef`

**Verdict: NO REGRESSION.** Current `main` (`c85e7ef`, Step 26.6b — every Phase 26
`[context.features]` active) passes the full `newt-eval` agentic scorecard with a
**perfect score across two models served by dgx1: 130/130 evaluator checks `ok`**
(13 cases × 5 evaluators × 2 models, zero non-`ok` rows).

Run by Claude at Shawn's request once all Phase 26 features landed on `main`
(the standing plan). Live inference; never mocked.

## What this tests — and what it doesn't

- **Tests:** the end-to-end agentic regression — with every Phase 26 context
  feature on (ToolOffload, Scratchpad, Semantic, Provenance, Experiential,
  Scheduled, plus the composable schema / manager-as-preset / `/context stats`),
  does newt still complete real coding tasks and emit diffs that **apply,
  compile, and pass tests**? Driven by **real inference on dgx1**.
- **Not covered here (by design):**
  - The **B-series context-memory micro-benchmarks** (B1/B3/B5/B6/B7) are
    operator-pinned to gnuc's Ollama (*"never DGX/LB for these runs"*,
    `context-baseline-f0f4f6e.md`) for baseline comparability — they are a
    separate gnuc arm, not run on dgx1.
  - The **per-feature `/context stats` token-impact telemetry** (Step 26.2) is a
    TUI command, not wired into `newt-eval`; capturing it is a follow-on.

## Results

| Arm | Model (on dgx1) | Cases | Evaluator checks | Pass | Wall-clock (13 cases) |
|-----|-----------------|-------|------------------|------|------------------------|
| 1 | `qwen3-coder:30b` (30.5B, Q4_K_M) | 13 | 65 | **65/65 (100%)** | ~62 s (warm) |
| 2 | `Qwen3-Coder-Next` (79.7B, UD-Q4_K_XL — dgx1 daily driver) | 13 | 65 | **65/65 (100%)** | ~136 s (warm) |

Every case passed all of `diff_nonempty`, `diff_applies` (`git apply --check`),
`rust_compiles` (`cargo check`), `tests_pass` (`cargo test`), `pattern_match`.

Cases (13; no `009`): `001-rename-function`, `002-add-doc-comment`,
`003-add-error-handling`, `004-add-test-case`, `005-extract-constant`,
`006-handle-empty-input`, `007-add-struct-method`, `008-extract-helper`,
`010-decompose-god-function`, `011-state-machine-drain`, `012-trait-display-enum`,
`013-generic-bounds`, `014-multi-file-extract`.

## Environment (citability)

| | |
|---|---|
| newt | `main` @ `c85e7ef` (Step 26.6b — all Phase 26 features merged), release build |
| Build | `cargo build --release --bin newt --bin newt-eval`, `CARGO_TARGET_DIR=/tmp/.cargo-target` |
| Inference | **dgx1** (`REDACTED-HOST:11434`, REDACTED-IP; GB10, ~121 GiB unified), Ollama |
| Endpoint forcing | `OLLAMA_HOST=http://REDACTED-HOST:11434` — **verified**: each model became resident on dgx1's `/api/ps` during its run (the harness leaves `OLLAMA_HOST` unset only in mock mode, so the export propagates to the worker) |
| Harness | `newt-eval run --mode live --model <m> --coder --worker-timeout-ms {180000,240000}`; ACP worker subprocess per case; `NEWT_EPHEMERAL=1` (eval never touches the store) |
| Date | 2026-06-23 |

## Reproduce

```bash
cd ~/workspaces/newt-agent
cargo build --release --bin newt --bin newt-eval   # CARGO_TARGET_DIR=/tmp/.cargo-target
export OLLAMA_HOST=http://REDACTED-HOST:11434
$CARGO_TARGET_DIR/release/newt-eval run --mode live \
  --model qwen3-coder:30b --coder --worker-timeout-ms 180000
# arm 2: --model "hf.co/unsloth/Qwen3-Coder-Next-GGUF:UD-Q4_K_XL" --worker-timeout-ms 240000
```

## Reading

Phase 26's composable context-feature layer (offloading, scratchpad, semantic
retrieval, provenance-preserving compaction, scheduled compiled views,
manager-as-preset) is **transparent to task success** at this scale: with all
features on, newt produces correct, applying, compiling, test-passing diffs on
**every** case, with both a mid-size (30B) and a large (80B) coder model. No
feature interaction degraded the agent loop on the suite as it stands.

Caveat on sensitivity: the 14-case suite is a *floor* check (these are bounded
single-goal refactors that strong coder models clear easily), so a 100% pass is
"no gross regression," not "no behavioral change." The finer-grained signals live
in the follow-ons below.

## Follow-ons worth running

1. **B-series compression/overflow gauntlet on gnuc** — the context-specific
   micro-regression (B5/B6), comparable to the `f0f4f6e` baseline; this is where a
   compression/offloading regression would actually show.
2. **`/context stats` token-impact capture** in a driven TUI session — Step 26.2's
   experimentation surface (per-feature elided-token accounting).
3. **Feature-ablation arm** — patch `base_features()` to toggle features off and
   re-run, to *attribute* any token savings / behavior to specific features rather
   than measuring them only all-on.

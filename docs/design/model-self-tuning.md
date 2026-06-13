# Phase 20 — Model self-tuning: learning from observed usage

**Status:** Step 20.1 in progress; Step 20.2 planned.
**Motivating failure:** a live nemotron3:33b session (2026-06-12, v0.6.7)
died with `context (~8704 tokens) exceeds the model's input budget and
auto-compression is disabled after repeated ineffective passes` — while the
same session's transcript shows the backend **accepting an 8,734-token
prompt** (`prompt_eval_count=8734`). The system held proof that its budget
was wrong and discarded it.

## 1. The failure chain

1. A prior turn ratcheted `max_ok_input = 6068` for the model in
   `~/.newt/model-capabilities.json` — the largest prompt *seen*, not the
   largest the model *accepts*.
2. `initial_send_budget` treats `max_ok_input` as a ceiling and prefers it
   over `safe_context`, so the send budget was 6,068; minus ~1,432
   tool-schema tokens it became a 4,636-token compression target.
3. The chars/4 estimator under-counts this model's chat template + `thinking`
   field by ~30% (estimated ~6.6k where the backend reports 8.7k). Compression
   reclaim is measured in estimate space, so honest passes look ineffective —
   one even *grew* the estimate (summary outweighed the prune on an already
   tiny history). Two strikes latched the anti-thrash disable.
4. The over-budget dispatch proceeded anyway (by design) and was **accepted at
   8,734 tokens** — but the only capability write-back lives in the TUI's
   `Ok`-arm epilogue. The turn ended in `Err` (the Refused bail), so the
   evidence was discarded. `max_ok_input` stays 6,068; every later session
   repeats the spiral.

Compounding defects found during the audit:

- The epilogue keys success/overflow on `reply.is_empty()`, but every loop
  failure path returns non-empty placeholder text — so failed turns call
  `record_success` (ratcheting confidence on garbage) and `record_overflow`
  is effectively dead code.
- `record_overflow` lowers only `safe_context`, but both budget resolvers
  prefer `max_ok_input` — overflow learning was inert.
- `tune_confidence` / `tune_date` / `overflow_at` are persisted but consumed
  by no decision logic.
- `ensure_context_window` re-queries `/api/show` every turn when the endpoint
  reports no context length (no negative caching).
- The `thinking`-only response quirk is re-detected from scratch every turn,
  each time paying a prompt-inflating corrective retry.

## 2. Design: evidence flows both directions, at the moment of observation

Three principles:

1. **Every accepted dispatch is evidence.** If the backend evaluated an
   N-token prompt and returned a usable response, the budget is at least N.
   Record it when it is observed — not in a turn epilogue that an error can
   skip.
2. **The high-water mark is a floor, not a ceiling.** `max_ok_input` ratcheted
   from successes means "proven good up to N". The believed window comes from
   claims (`safe_context` = 80% of the declared window) and hard failures
   (cw-400 caps, overflow reining). The send budget is the *max* of proven
   and believed, bounded by the per-request `num_ctx` ceiling.
3. **Estimates get calibrated per model.** The observed/estimated prompt-token
   ratio is learned (EMA) and applied wherever chars/4 numbers meet
   backend-reported numbers, so compression triggers, targets, and the
   anti-thrash reclaim measurement operate in honest token space.

### 2.1 Field semantics after this change (`CapabilityEntry`)

| Field | Meaning | Raised by | Lowered by |
|---|---|---|---|
| `max_ok_input` | highest input proven accepted (floor of known-good); also the authoritative cap after a cw-400 | per-round acceptance, cw-400 (to 80% of reported limit) | overflow (reined to 75% of failure point), cw-400 |
| `safe_context` | best believed safe window (claim-derived) | `/api/show` bootstrap (80% of declared) | overflow, cw-400 — never raised automatically (VRAM) |
| `estimate_ratio` *(new)* | observed/estimated prompt tokens, EMA, clamped [0.5, 3.0] | per-round calibration samples | same |
| `emits_thinking` *(new)* | model returned thinking-only responses (empty content, non-empty `thinking`) | observed once | manual reset |

Send budget = `max(max_ok_input, safe_context)` composed via `min` with the
80%-of-`num_ctx` ceiling. The cw-400 path already reins `safe_context` to its
cap, so `max()` still lands on the authoritative number after a hard 400.

### 2.2 The per-round observation hook

`newt-core` cannot depend on the TUI's probe cache, so the loop reports
observations through a new `ChatCtx` hook (the `recover_cw_400` pattern,
generalized to the success direction):

```rust
pub enum RoundObservation {
    /// Backend evaluated `prompt_tokens` and the round produced a usable
    /// response (tool calls or non-empty content). `estimated_tokens` is the
    /// loop's chars/4 estimate of the same request, for calibration.
    Accepted { prompt_tokens: u32, estimated_tokens: usize },
    /// Persistent empty responses at `prompt_tokens` after retries (the
    /// 85%-of-safe-context silent-overflow exit).
    SuspectedOverflow { prompt_tokens: u32 },
    /// Response carried only non-content fields (thinking/reasoning) with
    /// empty content.
    ThinkingOnly,
}

pub on_round_usage: Option<&'a mut dyn FnMut(RoundObservation)>,
```

The TUI handler applies observations to the capability cache and saves
immediately — evidence survives turns that later bail, crash, or hit the
round cap. `Accepted` is **quality-gated** (the round produced usable output)
and **truncation-gated** (skipped when `prompt_tokens >= 95%` of the request's
`num_ctx`, where Ollama may have silently dropped the head of the prompt).

The loop also raises its in-turn `send_budget` on *window* evidence alone
(the backend processed N tokens inside the `num_ctx` it was sent), capped by
the `num_ctx` ceiling — so one over-budget acceptance stops the
compress-every-round thrash within the same turn.

### 2.3 Calibration currencies

Two token currencies exist: backend-reported (real) and chars/4 (estimate).
The learned `estimate_ratio` converts at every boundary where they meet:

- the trigger's `current` figure and tool-schema overhead are scaled
  estimate-to-real before comparing against the (real-token) send budget;
- the trigger's real-token budget is scaled real-to-estimate before it becomes
  a compression target, because the pipeline measures and reclaims in chars/4;
- reclaim *fractions* (anti-thrash) are ratio-invariant and unchanged.

Calibration samples are skipped when `observed < 0.5 * estimated` — an
Ollama prompt-cache hit reports only newly-evaluated tokens and would poison
the ratio downward. Under-reporting can never poison the `max_ok_input`
ratchet (it only raises on larger observations).

## 3. Step 20.1 — passive feedback (this PR)

- `RoundObservation` + `on_round_usage` + `estimate_ratio` threaded through
  `ChatCtx`; emission sites in both the Ollama and OpenAI loops.
- `initial_send_budget` / `resolve_memory_budget`: `max(max_ok_input,
  safe_context)` semantics; mid-turn budget raise on accepted dispatches.
- `record_accepted_prompt` / `record_estimate_sample` /
  `record_thinking_only` / `apply_observation` in `probe.rs`;
  `record_overflow` also reins `max_ok_input`.
- Epilogue: success accounting gated on the turn having produced at least one
  `Accepted` observation; the dead `reply.is_empty()` overflow branch removed.
- `ensure_context_window` negative caching (once per model per session even
  on fetch failure).
- The Refused bail message names the model and points at
  `newt tunings reset <model>`.
- `newt tunings show` displays the calibration ratio and thinking quirk;
  community tuning TOML round-trips `estimate_ratio` (additive optional
  field, format version unchanged).

## 4. Step 20.2 — active discovery (`/probe` extension, follow-up)

The passive path learns from traffic; a fresh model deserves day-zero
correctness. Extend the `/probe` suite from tool conformance to a full
capability discovery pass:

- context window: `/api/show` (with staleness refresh — a re-pulled model can
  change its Modelfile `num_ctx`), then an empirical padded-prompt binary
  search between `safe_context` and the declared window to find the real
  acceptance boundary, recording `max_ok_input` with `High` confidence;
- thinking probe: one tiny request inspecting response shape; persist
  `emits_thinking`, and for models that support it, probe Ollama's `think`
  option as a candidate mitigation;
- calibration bootstrap: derive an initial `estimate_ratio` from the probe
  requests' own `prompt_eval_count` vs chars/4;
- consume `tune_date` for staleness (re-probe nudge after N days / model
  re-pull).

## 5. Out of scope (both steps)

- Headless surfaces (`newt-acp-worker`, `newt-mcp-server`, `newt-eval`)
  reading or writing the capability cache — tracked separately; the hook
  is `Option` and absent there, preserving today's behavior exactly.
- Mining `usage.jsonl` / conversation-store token columns offline.
- The documented-but-unimplemented `[[model_tuning]]` auto-append at High
  confidence.
- Changing generation behavior for thinking models (e.g. sending
  `options.think`) — Step 20.2 probes it; acting on it is its own step.

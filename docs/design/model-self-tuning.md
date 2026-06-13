# Phase 20 — Model self-tuning: learning from observed usage

**Status:** Step 20.1 complete (PR #313); Step 20.2 in progress.
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

## 4. Step 20.2 — active discovery (`/probe` extension)

The passive path (20.1) learns from real traffic. A *fresh* model deserves
day-zero correctness, so `/probe` is extended from tool conformance to a full
capability-discovery pass. Cost is the design tension: the cheap probes run on
every `/probe`; the expensive empirical window search is its own opt-in
sub-command.

### 4.1 Command surface (Ollama-only, as today)

- `/probe [model]` — **cheap discovery** (3 small requests): tool conformance
  (unchanged), context-window refresh, thinking probe, and a calibration
  bootstrap derived from those same requests. Default arg = the active model.
- `/probe all` — cheap discovery over every *untested* model (unchanged
  selection rule).
- `/probe window [model]` — **active boundary search** (the expensive empirical
  pass); records `max_ok_input` at `High` confidence. Prints per-step progress.

`newt tunings show` gains a staleness marker and points stale/unprobed models
at `/probe window`. No `newt probe` CLI subcommand — probing is interactive
(warm-up, per-step timing) and stays TUI-only.

### 4.2 Context-window refresh

A new `refresh_context_window(entry, endpoint, model)` is the staleness-aware
sibling of 20.1's `ensure_context_window`: it **always** calls `/api/show`
(`/probe` is the explicit re-discover command) and updates `context_window`,
catching a re-pulled model whose Modelfile `num_ctx` changed. It re-bootstraps
`safe_context` to 80% of the declared window **only when `safe_context` is
unset** — never auto-raises it (the VRAM rule from 20.1 §2.1). The passive
session path keeps using `ensure_context_window` (fetch-once); only `/probe`
forces the refresh.

### 4.3 Thinking probe

One tiny request ("reply with the single word: ok", `stream:false`). If
`message.content` is empty/whitespace while a non-empty `thinking` /
`reasoning` / `reasoning_content` field is present, set
`emits_thinking = Some(true)` (sticky; matches 20.1's `record_thinking_only`).
A clean content response leaves the field untouched (absence ≠ proven-false).
Probing Ollama's `think` *option* as a mitigation is explicitly out of scope
(see §5) — 20.2 only records the quirk.

### 4.4 Calibration bootstrap

Every probe request above returns `prompt_eval_count` (real tokens) and was
built from a known chars/4 estimate. Each pair feeds 20.1's
`record_estimate_sample(observed, estimated)` (same EMA, same `< 0.5×` cache-hit
skip), so `estimate_ratio` is seeded before the first real turn instead of
converging over early traffic. The boundary-search requests (§4.5) feed it too.

### 4.5 Empirical input-boundary search (`/probe window`)

Goal: find the largest input the model genuinely accepts at a matching
`num_ctx`, recorded as `max_ok_input` with `High` confidence — exactly the
number 20.1's `initial_send_budget` trusts as the proven floor.

- **Bounds.** Low = current `safe_context` (or a 2,048 floor); high = declared
  `context_window` (from §4.2). If the window is unknown, probe upward by
  doubling from the low bound until the first rejection, then binary-search the
  bracket. High is hard-capped at the declared window — never probe a larger
  `num_ctx` than the model declares (VRAM safety; the model was pulled to run
  at that window).
- **One probe at candidate N.** Build a padded user prompt of ≈ N real tokens
  (filler sized by chars, corrected by the current `estimate_ratio` so the
  observed `prompt_eval_count` lands near N), send with `options.num_ctx = N +
  reply_margin` and a minimal `num_predict` (we test *acceptance*, not output).
- **Classification** (pure fn `classify_boundary_probe`, unit-tested without
  HTTP), given the result and the sent estimate:
  - **Accepted** — HTTP 200, non-empty/usable completion, and
    `prompt_eval_count ≥ 0.9 × N` (the model evaluated essentially the whole
    prompt). Record the observed `prompt_eval_count` via
    `record_accepted_prompt`; raise the low bound.
  - **Truncated** — HTTP 200 but `prompt_eval_count` well below N (Ollama
    silently dropped the head): treat as rejected, lower the high bound. This
    is the silent-overflow signal 20.1 can only infer after the fact.
  - **CtxWindow400** — parse the endpoint's hard limit, feed
    `record_context_window_400`, lower the high bound to it.
  - **Inconclusive** — any other transport/5xx error or OOM: stop raising,
    keep the last accepted value, surface the error (do not record a false
    boundary).
- **Convergence.** Binary-search until `high − low` is within the larger of
  1,024 tokens or 5% of high, bounded by a step cap (~12). On completion set
  `max_ok_input` to the highest accepted `prompt_eval_count`,
  `tune_confidence = High`, `tune_date = today`, and persist once.

### 4.6 Staleness

`is_tuning_stale(tune_date, today, max_age_days)` (chrono date math, default
30 days). `newt tunings show` renders `(tuning N days old — run /probe window)`
for stale entries and `(window not empirically probed — run /probe window)`
when `tune_confidence < High`. No automatic re-probe — discovery stays an
explicit, user-initiated action.

## 5. Out of scope (both steps)

- Headless surfaces (`newt-acp-worker`, `newt-mcp-server`, `newt-eval`)
  reading or writing the capability cache — tracked separately; the hook
  is `Option` and absent there, preserving today's behavior exactly.
- Mining `usage.jsonl` / conversation-store token columns offline.
- The documented-but-unimplemented `[[model_tuning]]` auto-append at High
  confidence.
- Changing generation behavior for thinking models (e.g. sending
  `options.think`) — Step 20.2 probes it; acting on it is its own step.

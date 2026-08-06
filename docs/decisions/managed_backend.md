# Managed Backend — cooperative model-swap awareness for shared inference hosts

**Status:** Proposed · **Date:** 2026-08-06 · **Relates to:** the out-of-the-box
epic (#1126, the `Serving` axis + `backend_probe::adopt`), the unboxing wizard
(#1549), the DGX-Spark tb-30 survey (`docs/findings/2026-07-29-…`).

## Context

A benchmark campaign against the dgx1 **llama.cpp router** (an OpenAI-compatible
local endpoint) kept quarantining runs on transient `502 / 503 / "error sending
request" / 429`. A four-reader diagnosis (adversarially verified against the
checkout) established:

- **Retry is not missing.** The agentic solve loop's `kind="openai"` dispatch —
  both `/v1/chat/completions` (the ornith path) and `/v1/responses`, including the
  final-summary POST — is wrapped in `newt_core::retry::with_backoff_notify` under
  `RetryPolicy::for_local_inference()` (6 retries, 2 s base, **30 s ceiling, ~90 s**,
  extendable to ~200–300 s via `NEWT_BENCH_HTTP_RETRIES → NEWT_HTTP_MAX_RETRIES`).
  `502/503/429` and transport `"request failed"` all classify as `Retry`.
- **The stalls outlast the window.** The router is `role: router`,
  `max_instances: 1`, `models_autoload: true` (per its `/props`) — it serves **one
  model at a time** and **auto-loads on request**. A cold load / model **swap** of
  a large GGUF can exceed even the patient retry window, so the turn ends `Failed`.
- **Retrying harder is a marginal lever.** Raising the backoff ceiling helps a
  single cold load, but does nothing about the deeper problem: on a *shared* box,
  several consumers requesting *different* models make the router **thrash** — each
  swap is a multi-second stall for everyone. The client is fighting the box instead
  of cooperating with it.

The missing idea is **awareness**: newt currently treats every endpoint as a
dedicated black box. A shared, model-swapping host is a different thing, and the
harness should know it.

## Decision

Introduce a **`ManagedBackend`** — a backend newt actively *tends* rather than
merely consumes. It is a new posture layered on the existing axes
(`BackendKind` = the wire; `Serving` = Multiplexer/Instance), **not** a new wire or
a forked backend.

### 1. Modes: `Shared` | `Dedicated`

- **`Shared`** — the box may serve other consumers (including other newt-agents).
  newt is a **cooperative guest**: it does not assume exclusive control and does not
  force swaps by default.
- **`Dedicated`** — "I own this box." newt may force-load and hold its target model
  exclusively.

### 2. Default = **adopt-warm** (the concurrency-safety primitive)

When a `ManagedBackend` is engaged, the default is to **use whatever model is
currently warm on the box**, rather than forcing a swap to a configured model —
*unless explicitly overridden*. This is what prevents two newt-agents from clashing:

- Agent A has `ornith` warm; Agent B arrives, sees `ornith` warm, **adopts it** and
  runs — no swap, no stall, no thrash.
- Only an **explicit override** (a pinned `model` + `force = true`, or `Dedicated`)
  makes newt cause a swap — a deliberate act that knowingly disrupts other guests.

Adopt-warm reuses the existing `backend_probe::adopt` decision (which already picks
"which model this session uses" from a probe) and the `Serving::Multiplexer`
classification (which already tags a many-model OpenAI endpoint as a swapping box).

### 3. Control channels: HTTP-only vs SSH

- **HTTP-only** — newt can only *observe and tolerate*: adopt-warm from the serving
  tech's warm signal (best-effort), and treat a `503 / loading` as an **expected
  swap** (a swap-aware wait) rather than a fault.
- **SSH-capable** — "I can reach my hardware." newt can *actively manage*: load and
  hold a model, keep it warm, read `nvidia-smi`, restart the model server. This is
  the reliable path for adopt-warm and for `Dedicated` keep-warm.

  **Boundary (workspace rule):** SSH manages the **inference service only** — load
  models, check the GPU, bounce the server. It never runs dev work, builds, or
  agents on the box. The box stays a model server.

### 4. Setup-wizard integration

Extend the unboxing wizard's `BackendChoice` (today: Ollama / DGX / Remote, with a
custom-host auto-probe). When the operator says **"I have my own hardware"**:

1. Managed? → write a `ManagedBackend`.
2. Shared or Dedicated?
3. Can newt SSH to it? → collect + **encrypt** credentials via the wizard's existing
   `newt-core/src/secrets.rs` path.
4. Probe + record `Serving` and a warm-model hint as provenance (as `newt setup`
   already does for plain backends).

## Consequences

- **Unblocks the dgx1 bench honestly.** The swap is *avoided* (adopt-warm /
  keep-warm), not the score gamed — no round-cap or gate changes.
- **A general capability**, not a bench hack: anyone pointing newt at their own
  shared inference box gets cooperative behavior instead of swap-thrash.
- **New surface to keep honest:** SSH credentials (encrypted, never logged), and a
  clear `Shared` vs `Dedicated` contract so a cooperative guest never silently
  evicts another.

## Open questions (resolve in the slices / review)

1. **Warm-model detection per serving tech.** The llama.cpp router's `/props`
   reports the *router* config, not the loaded leaf; llama-swap may not cleanly
   expose "which model is resident." Options: infer from first-response latency
   (fast = warm), a serving-tech-specific status probe, or require SSH for reliable
   adopt-warm. HTTP-only is best-effort; SSH is authoritative.
2. **`Dedicated` lease/claim.** How does one dedicated manager signal ownership so a
   second doesn't clash? (A marker on the box via SSH, a warm-model claim, or an
   honor-system config flag.) Defer to a later slice; `Shared`/adopt-warm needs no
   lease.
3. **`reasoning_content` handling** (ornith/nemotron emit it with empty `content`) is
   a *separate* concern from swaps and is tracked on its own.

## Rollout — slices (one PR each)

- **Stopgap (config-only, uncommitted):** raise `NEWT_BENCH_HTTP_RETRIES` +
  `NEWT_HTTP_BACKOFF_MAX_MS` so the retry window covers a single cold load. Buys
  time; not the fix.
- **Slice 1 — `ManagedBackend` config type + `Shared` adopt-warm + swap-aware wait
  (HTTP-only).** Widen `backend_probe::adopt` to prefer the warm model; treat a
  `Serving::Multiplexer` stall as an expected swap. Unblocks the bench.
- **Slice 2 — wizard "my hardware" step** (managed? shared/dedicated? SSH?).
- **Slice 3 — SSH control channel** (load/hold/keep-warm, `nvidia-smi`, restart),
  encrypted creds.
- **Slice 4 — `Dedicated` lease/claim + coordination.**

Each slice is TDD, fully mocked at the unit tier (wiremock for the HTTP warm-probe,
an injected SSH seam for the control channel — no real network or box in the gate).

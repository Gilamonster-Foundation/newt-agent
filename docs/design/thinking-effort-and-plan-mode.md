# Thinking as a resource: `effort` + `plan_mode` techniques

**Status:** design note (pre-build) · **Composes with:** [`technique-library.md`](technique-library.md) (the technique abstraction), [`model-family-profiles.md`](model-family-profiles.md) (profile = composition) · **Builds on:** the canonical [`Plan`](../../newt-core/src/plan.rs) (#338/#334), per-session `plan.md` ([`session_plan_path`](../../newt-core/src/conversation.rs), #220), the `MemoryProvider` system-prompt seam, the `/mode` caveat presets (#307) · **Prerequisite:** [#385](https://github.com/Gilamonster-Foundation/newt-agent/issues/385) (the `<think>` split) · **Tracks:** [#381](https://github.com/Gilamonster-Foundation/newt-agent/issues/381) (`/effort` + `/probe` learning)

## Abstract

A thinking model's reasoning is a **resource to leverage, not noise to strip**.
Today newt does neither well: it sends no effort knob, and inline `<think>…</think>`
reasoning leaks unfiltered into replies and into the content the coding loop parses
(#385). This note specifies two composable profile techniques that turn that around:

- **`effort`** — control the reasoning dial (the right way per backend), so a profile
  asks for exactly as much thinking as a model+task warrants.
- **`plan_mode`** — *capture* the reasoning instead of discarding it: for a thinking
  model, a high-effort first turn **is a plan**. Persist it as a canonical
  [`Plan`](../../newt-core/src/plan.rs) + `plan.md`, optionally gate on approval, then
  execute against it with thinking turned back down.

Both are techniques in the [model-family-profile](model-family-profiles.md) suite,
alongside `knowledge_base` / `verify_gate` / `retry`.

## The reality today (verified, 2026-06-15)

- **No effort knob is sent.** The only per-request dials are `num_predict`/`max_tokens`
  + `num_ctx` (`newt-inference/src/local.rs:181-188`, `:339-348`). No `reasoning_effort`,
  no `think`, no `detailed thinking` directive, no `/effort` command or config.
- **Thinking is detected, never stripped, and inline tags leak.** The *separate-field*
  case (Ollama `message.thinking`/`reasoning`/`reasoning_content`) is detected by
  `ollama_non_content_fields` (`newt-core/src/agentic/mod.rs:1492`) and drives the
  thinking-only recovery retry — but **inline `<think>…</think>` inside `content` is
  unhandled** (#385). The probe already records `emits_thinking` (`newt-tui/src/probe.rs:288`).
- **Nemotron's control is a system-prompt directive**, `detailed thinking on|off`
  (NVIDIA Llama-Nemotron) — *not* `reasoning_effort` and *not* (canonically) the Ollama
  `think` option. It is binary, and it rides the **same system-prompt seam** as
  `knowledge_base`.

### #385 should be a *split*, not a *strip*

The prerequisite fix (#385) removes `<think>…</think>` from `content`. `plan_mode`
needs the reasoning, so the sanitizer should **return both halves** —
`{ content, reasoning }` — discarding `reasoning` by default (today's behavior) but
making it available to `plan_mode` / transcript / the probe. One pass, two consumers.

## Technique 1 — `effort`

**Mechanism (per-backend, not uniform):**

| backend / model class | how `effort` is applied |
|---|---|
| OpenAI reasoning models (o-series, gpt-5) | `reasoning_effort: low\|medium\|high` request field |
| Ollama thinking models that wire it (deepseek-r1, qwen3, glm) | the `think: true\|false` option (+ qwen `/think` `/no_think`) |
| **Nemotron** | inject/toggle **`detailed thinking on\|off`** in the system prompt (the `MemoryProvider` seam) |
| non-reasoning (gpt-4.1, llama) | no-op with a clear message |

**Knobs:** `effort.level: minimal | low | medium | high | off`, mapped down to what the
active model actually supports (a binary model collapses `{minimal,low}→off`,
`{medium,high}→on`). **Probe learning:** extend `emits_thinking` into an
effort-support record — is it a reasoning model? which levels return sane output? —
so `/effort` offers only what the model can do and a sane default is *learned*.

### Spec sheet — `effort`

| field | value |
|---|---|
| **buys** | match thinking budget to model+task: more grounding on hard tasks; less latency/pollution on easy ones (esp. `off` for a coding agent on a verbose reasoner) |
| **failure mode** | wrong backend mapping → a silent no-op (sent `reasoning_effort` to Ollama) or a leaked directive; a model that *needs* thinking starved by `off` |
| **caveat / context** | the dial only helps if the reasoning is then handled (#385) — high effort + a leak is worse than low |
| **knobs** | `effort.level` (default: learned per model; else `off`) |
| **presupposes** | #385 (the split); the probe's effort-support record for the default |
| **composes with** | `plan_mode` (turn it up to plan), `knowledge_base` (the directive shares the seam) |
| **measured by** | the rig × `--profile` matrix — per-family lift of each level |

## Technique 2 — `plan_mode`

The **leverage**. A thinking model's first high-effort turn is a plan; capture it as a
durable, structured artifact instead of letting it evaporate (or leak).

**Mechanics — two phases:**

1. **Plan phase.** `effort` up (`detailed thinking on` / high). Run the turn under
   **read-only caveats** (a `/mode`-style preset, #307 — no writes yet). Capture the
   model's reasoning (via the #385 split) and have it emit a canonical
   [`Plan`](../../newt-core/src/plan.rs): `goal` + `[[subtask]]`s, each with its
   `context` files, `verify` command, and **default-deny `caveat_policy`** (the model
   *names* the authority each step needs; the harness grants no more — "the harness
   stamps, the model never asserts"). Persist to the session `plan.md`
   (`session_plan_path`). Optionally **gate on human approval** (the newt analogue of
   Claude Code's plan-mode → approve), or on the verify-oracle.
2. **Execute phase.** `effort` *down* (thinking off — the plan is the reasoning now).
   Execute the plan's subtasks; the persisted `Plan` is durable context that
   **survives compression** where free-form reasoning would not, and per-subtask
   `verify`/`caveat_policy` gate each step. `status`/`result` make `plan.md` a resumable
   run-log (`/plan resume`).

This is the disclosure principle at the reasoning layer: *don't summarize-and-hope —
capture the plan as a budgeted, addressable artifact and execute against it.*

### Spec sheet — `plan_mode`

| field | value |
|---|---|
| **buys** | reasoning becomes a durable, gate-able, resumable plan; execution is grounded in an approved structure that survives compression |
| **failure mode** | a plan that over-claims authority (mitigated: default-deny `caveat_policy`); a plan that drifts from execution (mitigated: `verify` per subtask) |
| **caveat / context** | only as good as the plan the model writes; the read-only plan phase + approval gate are the safety, not the model's good intentions |
| **knobs** | `plan_mode.approval: auto \| human \| verify-oracle`; inherits `effort` for both phases |
| **presupposes** | `effort` (to raise/lower thinking), #385 (to capture reasoning), the `Plan` struct + `plan.md` (both exist) |
| **composes with** | `knowledge_base` (plan against the real surface), `verify_gate`+`retry` (gate/repair execution), the swarm scheduler (a `Plan` is already its dispatch unit) |
| **measured by** | the rig — does plan-then-execute lift task success vs. one-shot, per family? |

## Sequencing

1. **#385 as a split** (`{content, reasoning}`) — correctness; unblocks the Nemotron
   rig leg of #80, and gives `plan_mode` its input.
2. **`effort`** — the per-backend dial + the Nemotron system-prompt directive + the
   probe's effort-support record (extends `emits_thinking`). This is #381.
3. **`plan_mode`** — capture reasoning → `Plan`/`plan.md`, read-only plan phase +
   approval, execute phase. Reuses the `Plan` struct, `session_plan_path`, the `/mode`
   presets, and the verify-oracle — net-new is the two-phase driver.

Each lands as a focused PR with a spec sheet kept honest by the rig, exactly like
`knowledge_base` / `verify_gate` / `retry`.

## Out of scope

- The `<think>` strip mechanics themselves (#385) — this note only asks it to return
  both halves.
- The swarm scheduler that fans a `Plan` across backends (Workstream C) — `plan_mode`
  produces the same `Plan` it will one day consume, but single-agent execution first.
- Per-family auto-tuning of the default effort level (R5 self-tuning).

Refs #381, #385, #338, #334, #220, #80.

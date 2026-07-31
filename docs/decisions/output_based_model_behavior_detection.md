# Output-based model-behavior detection and adaptation

**Status:** PROPOSED (design-first — no implementation until this ADR is agreed). Epic: #1506.
**Drivers:** the 0.7.6 bench cycle's quarantines, #1500, and the operator ruling:
*"Rather than trusting a configuration I'd like the system to auto detect the
model's behavior NOT by name but by outputs — and to adjust to switching models
by adapting to the outputs."*
**Review:** this revision incorporates a 33-finding adversarial design review
(4 lenses: no-harm, architecture/reuse, evidence, security).

## Context

### The name-gate problem

`newt-core/src/reasoning.rs` gates reasoning handling on a **model-name list**
(`emits_leading_reasoning`: `["nemotron", "deepseek-r1", "qwen3"]`), and
`newt-core/src/model_card.rs` carries a per-card `emits_leading_reasoning`
override — two label-keyed sources of truth for the same behavior. Name/label
gates fail three ways, all observed:

1. **Renames.** Anyone can serve any weights under any name.
2. **Silent re-uploads.** Gemma 4 (2026-07-15) changed tool-calling behavior on
   Hugging Face under identical names with no version bump (anecdotal but
   load-bearing for bench provenance: it is why bench records pin
   `model_digest`).
3. **New models are invisible** until someone edits a list — and only the
   model's *outputs* can say how the model behaves.

Per-model *configuration* as the primary mechanism is rejected for the same
reason: it trusts a label. (Cards survive as **priors** — see §Profile.)

### What already exists (compose, don't duplicate)

This ADR does **not** introduce content-recovery to newt. It already ships:

- **`agentic/tool_recovery.rs`** — recovers tool calls a weak model emitted in
  CONTENT (fenced/bare JSON, `<function=NAME>` tags, root-tags), engaged at the
  dispatch site **only when the native `tool_calls` array is empty**, grounded
  in `docs/research/weak-model-plan-mode-findings.md`.
- **Suspicious-empty retry** (`suspicious_empty_retries` +
  `SUSPICIOUS_EMPTY_RETRY_CAP` in `agentic/mod.rs`) — a capped retry-with-nudge
  for empty replies.
- **Tenacity/nudger** — the non-action counters and nudge authority.
- **Model cards** — declarative per-model hints (incl. `emits_leading_reasoning`).

What is missing is a **shared, evidence-driven brain**: each mechanism fires on
its own local heuristic, none of them learn from what the model actually emits,
and reasoning handling still keys on names. The decision below turns these
existing mechanisms into rungs driven by one observed profile — it widens them;
it does not stand up parallel machinery.

### Bench evidence (0.7.6 cycle, honesty-classified + digest-pinned)

- `nemotron-3-nano_30b` / `-canonical`: **quarantined** — 16/30 and 20/30 tasks
  with a real tool-call attempt (not transport noise).
- `granite4.1_30b`: quarantined at 22/30 despite passing a trivial edit smoke.
- `gpt-4.1-mini` (hosted control): 3.3%. `ornith-1.0-35b-q8`: 36.7% champion.

### Probe evidence (2026-07-31 — raw non-streaming bodies, fixtures captured with request params)

Two-shot probes (tools-declared; reasoning-shape at a **350-token output cap**)
against six models via the llama.cpp router and the OpenAI API:

| Model | Structured `tool_calls` (tools probe) | Reasoning channel | Reason-probe content |
|---|---|---|---|
| nemotron-3-nano_30b | ✅ 1, `finish=tool_calls` | `reasoning_content` | normal content + reasoning |
| nemotron-3-nano-canonical | ✅ | `reasoning_content` | normal content + reasoning |
| granite4.1_30b | ✅ | none | normal content |
| qwen3.6_35b | ✅ | `reasoning_content` | **empty content** (budget spent in reasoning at the 350-token cap) |
| ornith-1.0-35b-q8 | ✅ | `reasoning_content` | **empty content** (same, at the cap) |
| gpt-4.1-mini | ✅ | none | normal content |

Conclusions, with their limits stated:

1. **On trivial single-turn prompts, the structured channel is healthy for all
   six** — so simple parse failure does not explain the losses *on these
   prompts*. (`tool_recovery.rs`'s weak-model findings show content-emitted
   calls do occur in harder multi-turn sessions — both facts stand.)
2. **Reasoning arrives server-split in `reasoning_content`** in these
   *non-streaming bodies*; inline `<think>` was not observed here. Streaming
   delta shape is **unverified** — streaming fixtures are a W1 obligation
   before the name gate is removed (§Kill order).
3. **Session-level failure signatures are the working hypothesis, not yet a
   measured fact.** Current bench traces record parsed results only, so the
   nano cadence-collapse mechanism is unattributed until W0 lands raw-output
   observability. The verification gate treats approach A's lift as a
   hypothesis (approach B may carry the lift); no-harm is the hard constraint
   either way.

## Decision

### 1. `BehaviorProfile` — one observed profile, hysteresis, fail-safe

One profile per **(session, backend, request-slot)** — the request slot
partitions multiplexer backends (a router serving several models); using the
request-side slot as a *partition key* is not name-trust, and a known
request-side model change re-selects the profile immediately rather than
waiting for contradiction. Never keyed on model name for *behavior*.

Axes (all evidence comes from assistant output; **quoted/echoed regions are
excluded from ALL profile evidence**, per the echo rule in §Security):

- **reasoning channel**: `Unknown | ServerSplit | InlineThink | LeadingCloser |
  None`. Positive evidence only: `reasoning_content` presence (ServerSplit);
  `<think>` at content **start** (InlineThink); a bare `</think>` in the first
  N bytes of content with no opener (LeadingCloser — the one shape the current
  name gate actually protects, `ThinkFilter::with_leading_reasoning`).
  Mid-content tag occurrences are never evidence (they are quotable).
- **tool-call channel**: `Unknown | Structured | TaggedInContent | BareJson` —
  where actionable output has actually appeared, fed by dispatch results
  including `tool_recovery` outcomes.
- **budget signature**: exhausts output budget inside reasoning —
  `finish_reason = length` **and** empty content **and** populated reasoning.

**Evidence semantics (per axis):** a positive observation of a *different*
channel is a contradiction; **absence is decay, never contradiction** (a short
"Done." turn must not flip a reasoning model's profile). A behavior-changing
transition requires **N consecutive contradicting observations** (hysteresis;
N is a config default, target N=2 — a *design target to be verified by a
swap-mid-session fixture*, not a promise). Ambiguity ratchets toward the
default pass-through path (fail-safe). This resolves hot swaps in both
directions: swap-to-reasoning shows positive new-channel evidence; swap-to-
non-reasoning shows sustained absence *plus* positive `None`-channel evidence
(normal content with no reasoning field), which counts as contradiction.

**Priors, not gates:** model cards (incl. `emits_leading_reasoning`) and any
`(endpoint, digest)` cache become **warm-start priors** with identical
standing: they may pre-set *read-side expectations only* (which channels to
watch), may only tighten toward the default path, and **never arm a recovery
rung or a destructive filter mode** without live in-session confirmation. The
digest in a cache key is operator-supplied provenance (bench pins, local file
hash); when absent, there is no warm start — cold profile, fail-closed. This
subsumes the model-card retirement plan (#384): cards stop being behavior keys
and become priors that evidence overrides.

### 2. The adaptation ladder — existing mechanisms, profile-driven, bounded

The native parse path runs **unchanged, first, always**. Rungs engage only on
failure signatures, are bounded, and every engagement is observable. **Nudge
ownership is single:** rungs emit signals that the *existing tenacity
accounting* consumes — one counter, one nudge authority; recovery rounds count
against the existing round limits.

- **Rung 1 — reasoning-overflow recovery** *(widens the existing
  suspicious-empty retry; does not add a second retry site).* Trigger is the
  **full budget signature**: `finish_reason = length` AND empty content AND
  populated reasoning AND no tool calls. At most **one** auto-continue per
  turn, with a **capped** budget raise (bounded by the existing context/output
  ceiling machinery), counted against the round limit, and surfaced on the
  operator surface (TUI line), not just the trace. On failure after the raise,
  the turn **is** recorded as a non-attempt so tenacity engages — recovery must
  not starve its own backstop.
  A `finish_reason = stop` turn with empty content + populated reasoning is a
  **different, non-mutating** case: surface the reasoning tail as a content
  candidate (display/continuation), never inject a "now act" nudge — the
  probes show this shape can be *healthy* for the champion, and the must-NOT-
  fire fixture set includes ornith's `finish=stop` reasoning-only bodies.
- **Rung 2 — content dialect recovery** *(this IS `tool_recovery.rs`, widened
  into a data-driven registry — parsers stay, dialect knowledge moves to pure
  data; three Cs).* Engagement keeps today's precondition (native array empty)
  **plus** a profile guard: on a backend whose observed tool channel is
  `Structured` (a successful native call this session), a recovered candidate
  additionally requires the echo-check pass + schema match, and a *first*
  recovery on such a backend is surfaced for operator confirmation when a
  decision surface exists (headless: read-only tools only). The profile
  narrowing the live dialect set is an explicit design property — recovery is
  broad when nothing else works and near-unreachable on a stack where the
  structured channel is proven. Registry config is **operator-scope only**
  (built-ins + `~/.newt`); a workspace drop-in may *narrow*, never extend, the
  dialect set.
- **Rung 3 — non-action spiral** ⇒ the existing tenacity machinery, unchanged.

### 3. Kill the name gate — in the right order

`emits_leading_reasoning` (the list) is deleted and the splitter becomes
profile-driven, **after** the streaming question is answered:

- **Streaming asymmetry (acknowledged):** the stream filter's mode must be
  chosen before output exists, so this axis cannot literally "engage on
  failure". Fail-safe rule: wrongly-OFF leaks cosmetic reasoning text;
  wrongly-ON destroys real content — therefore the inline/leading-closer
  filter **defaults OFF** and arms only via the profile's multi-turn ratchet
  (positive start-of-content evidence on N consecutive turns). A ServerSplit
  observation suppresses inline handling outright.
- **Unknown-profile first turn:** bounded provisional hold-back — scan the
  first N bytes for a bare `</think>` before committing the stream to
  display; past N bytes, commit and accept a possible one-turn cosmetic leak
  (documented, fail-safe direction).
- The model-card field survives as a warm-start prior only (§1); the stale
  shipped ornith cards get corrected as data, and the scoped law is:
  **no *recognition or recovery* behavior keyed on model name** (operator-
  configured disposition like tenacity family defaults, and card priors that
  evidence overrides, are explicitly exempt — they are operator intent, not
  identity guessing).

### 4. Optional active probe = the same detector, warmed eagerly

An operator-invoked (or session-start) two-shot probe feeds the passive
detector to warm the profile. Convenience, never authority — a probe cannot
see a mid-session swap; continuous adaptation can.

### 5. Observability — trace, contract, AND the operator surface

Every detection/recovery emits a trace event (`reasoning_overflow`,
`recovered_tool_call{dialect}`, `no_parseable_tool_call`) feeding the external
evaluator's artifact-vs-weakness split (#1500). Recovery provenance is also
carried on the call object and **rendered in the permission prompt and the TUI
tool line** — an operator approving a recovered call sees that it was recovered
and from which dialect.

## Security boundary (non-negotiable, decidable)

Dialect recovery widens what newt can *read*, never what the model may *do*:

1. **Assistant output only**; quoted/echoed regions are excluded from all
   profile evidence and all recovery scanning.
2. **Echo rule (the decidable mechanism):** a recovered candidate is
   **rejected** when its extracted `(name, args)` span is a verbatim
   (whitespace-normalized) substring of any prior *untrusted* context — task
   text, file-read results, tool results, untrusted-data bodies — checked by
   equality against the transcript. Fenced blocks are *not* rejected per se
   (that would break genuine recovery, which often arrives fenced); **echo
   matching is the criterion.**
3. **Declared-set rule:** a recovered call's name must be present in THIS
   session's merged declared set (gated builtins + live MCP registry). Any
   dispatch-time exemption for namespaced (e.g. MCP `__`) calls does **not**
   apply to recovery-sourced calls; arguments must validate against the
   declared JSON schema *at recovery time*, before the executor. A
   recovery-sourced MCP call with no covering caveat axis and no operator
   present fails closed.
4. Recovered calls then pass the **same OCAP gates** as native ones — with the
   provenance marker of §5 on the approval surface.

## Verification gate (release-grade; A and B together)

This ADR is approach **A**; #1492 (psyche/cognition) is approach **B**. Goal
gate, 0.7.6 methodology (tb-30 confined, honesty classifier, digest-pinned,
same window/tenacity):

- `nemotron-3-nano_30b` — **lift**: quarantined (16/30 real) → scoreable → scored.
- `gpt-4.1-mini` — **lift**: above 3.3%.
- `ornith-1.0-35b-q8` — **no harm**: holds ≥ 36.7%, *plus an A/A control*
  (adaptation force-disabled vs enabled) because rung 1's signature is known to
  occur on the champion — the A/A run is what proves engagement ≠ harm.

Both lift targets probed clean on every detectable axis, so **A's lift
contribution is a hypothesis until W0 traces attribute the losses**; B may
carry the lift. No-harm is the hard constraint regardless.

Unit tier (fixture corpus, fully mocked):

- **Firing fixtures** per rung from *real captures* (probe bodies with request
  params; weak-model traces for rung 2's dialects; 0.7.6 quarantine traces for
  the nano spiral once W0 lands; a vLLM/template-mismatch capture for inline
  `<think>`; streaming SSE fixtures for all six probed models). A rung with no
  real firing fixture does not ship.
- **Must-NOT-fire fixtures**: clean structured output; ornith's healthy
  `finish=stop` reasoning-only bodies.
- **Adversarial fixtures**: per-dialect echo attacks (task text quoting a tool
  call — must never execute), unknown-name, schema-mismatch, MCP-namespaced —
  each proving fail-closed.
- **Swap fixture**: mid-session model swap converging within the hysteresis
  target.

## Alternatives rejected

- **Per-model configuration as the mechanism** — trusts a label (cards survive
  only as evidence-overridable priors).
- **Name gates (status quo)** — see Context; mis-aimed for the primary stack's
  non-streaming shape and blind to renames/re-uploads.
- **Probe-only detection** — blind to mid-session swaps; subsumed as warm-start.

## Out of scope

- Server-side chat-template repair (#1500's audit track).
- Suite authoring and the external evaluator (separate repos).
- exec confinement hardening (Landlock) — separate ADR.

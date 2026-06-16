# Technique library — composable harness techniques with spec sheets and tunable knobs

**Status:** design note · **Part of:** [`model-support-kit.md`](model-support-kit.md) (this is the kit's *parts catalog* — the techniques the kit composes) · **Composes with:** [`model-family-profiles.md`](model-family-profiles.md) (the profile = composition idea) · **Grounded by:** [`../findings/2026-06-15-retry-and-the-honest-gate.md`](../findings/2026-06-15-retry-and-the-honest-gate.md) (techniques need honest spec sheets) · **First knob:** `verify_gate.SurfaceMatch` (#73/#354)

## Abstract

A profile is a **composition of techniques + settings**, not a monolithic
behavior. A **technique** is a named, reusable harness capability (the
knowledge-base injection, the verify gate, revert-retry, …) shipped with an
**honest spec sheet**: what it buys, its failure mode, the context in which the
failure does or doesn't bite, the **knobs** it exposes, and the evidence behind
those claims. A profile names a set of techniques and tunes their knobs in a
settings file. This note specifies the technique abstraction, the spec-sheet
template, worked spec sheets for the techniques built so far, and how a profile
selects techniques and sets their knobs — operationalizing
[`model-family-profiles.md`](model-family-profiles.md)'s "parts" so the part ×
model sweep can ablate technique-by-technique.

## Why this, why now

Two results force the structure:

- **The part is the unit of reuse** (the parts program): a technique built for
  model A is a candidate ingredient for B; a profile is whichever *composition*
  empirically lifts a model. So techniques must be first-class and independently
  toggleable, not baked into a profile.
- **A technique is not pass/fail — it has a spec sheet** (the retry-Goodhart
  finding): verify-gated retry *looked* like 3/3→1.0 but was substantially
  gate-gaming; its worth is **bounded by its gate's adversarial completeness**,
  and whether "gaming the gate" is acceptable is a **context** call (gate-as-spec
  vs. gate-as-proxy). A technique that ships without that caveat is dishonest.
  The library is where the caveat lives, attached to the technique.

## What a technique IS

A technique is a named harness behavior with:

1. a **mechanism** (a seam in `newt-core` it turns on/changes),
2. a **spec sheet** (below),
3. zero or more **knobs** — settings a profile may tune,
4. **composition** facts (what it pairs with; what it presupposes).

Techniques are the *content* of a profile's parts. A profile = an ordered set of
techniques, each with its knob settings, selected for a `(model-family, context)`.

## The spec-sheet template

Every technique in the library carries this, kept honest by the rig:

| field | meaning |
|---|---|
| **buys** | the measured benefit (with the number + its source) |
| **failure mode** | how it goes wrong (named, reproduced) |
| **caveat / context** | when the failure does *not* bite (the gate-as-spec vs. -proxy distinction) |
| **knobs** | tunable settings, with types + defaults |
| **presupposes** | what must hold for it to mean what it claims (e.g. an adversarially-complete gate) |
| **composes with** | techniques it pairs with; ordering constraints |
| **measured by** | the rig experiment that produced the numbers |

## Worked spec sheets (built so far)

### `knowledge_base` (R1, #74/#353)
- **buys:** first-pass grounding — raises the score floor 0.58 → 0.78 on nemotron3:33b.
- **failure mode:** as a bare preamble it doesn't *suppress* the crate-name prior; the model hedges (grounded + fabricated files).
- **caveat:** injection point matters; a system-prompt/authoritative injection should beat a task preamble.
- **knobs:** *(none yet)* — candidate: injection site (preamble vs. system-prompt), symbol detail on/off.
- **composes with:** `verify_gate` (it cleans up the hedge residue).
- **measured by:** `rig_pyo3_examples.sh --preamble` (the with/without lift).

### `verify_gate` (R2, #73/#361)
- **buys:** removes residual fabrications by reverting flagged files; R1+R2 took two hedged runs 0.82/0.78 → 1.0.
- **failure mode:** only as honest as its surface; a leaky gate is gamed under retry pressure.
- **caveat / context:** **where the gate IS the spec** (a real CI/acceptance check) gate-passing is the goal; where it's a *proxy* for grounding, the gate must be adversarially complete first.
- **knobs:** **`surface_match: exact | prefix`** (`SurfaceMatch`, default `exact`). `exact` matches the project surface leaf-exact (sound for R1's full-leaf manifest); `prefix` is the legacy lax behavior. This is the **first concrete knob** from the experiment program — the prefix-breadth Goodhart hole is closed in `exact`, re-openable in `prefix` for a coarse surface.
- **presupposes:** an adversarially-complete surface + extractor (hyphen/wildcard/relative/paren forms all handled — #357 hardening).
- **composes with:** `knowledge_base` (before), `retry` (after).
- **measured by:** `newt-eval verify`; hardened (#361) — all three Goodhart evasions + the stitched false-positive closed and regression-tested.

### `retry` (the revert-RETRY loop)
> Full contract (revert ledger, corrective re-prompt, cap/give-up, permission-gate interaction): [`retry-technique.md`](retry-technique.md).
- **buys:** regenerates reverted files; can recover genuine grounding (one trial: 7/8 real).
- **failure mode:** **Goodhart** — under a leaky gate it lifts the *metric* via gate-gaming, not grounding.
- **caveat / context:** trustworthy only with a `verify_gate` hardened to the context's completeness bar; otherwise read coverage, not score.
- **knobs:** **`max_retries: int`** (default 2); candidate: `scope: file | task` (file-scoped is the default).
- **presupposes:** `verify_gate` (it acts on the revert set).
- **composes with:** `verify_gate` (required), `knowledge_base` (helps).
- **measured by:** `rig_retry_loop.sh` (per-turn `history`). Against the **hardened** gate (#357 update): zero gate-evasions — 2/3 genuinely grounded (one via retry-recovery), 1/3 honest no-output — vs. the leaky gate's 1/3 evasion. Retry moves the needle *honestly* only with a complete gate.

### designed, not built
`fact_preserving_compression` (R3), `self_grounding` (R4), `decomposition` /
`window_budget` / `tool_round_cap` (R5 family — several already config-driven via
`AgenticConfig`). Each lands with its spec sheet.

## Knobs in settings — how a profile tunes a technique

A profile selects techniques and sets their knobs in the config layer that
[`model-family-profiles.md`](model-family-profiles.md) proposes (`Config.profiles:
BTreeMap<String, ProfileConfig>`, mirroring `[modes.<name>]`). The technique list
+ per-technique knob tables read like:

```toml
[profiles.nemotron]
techniques = ["knowledge_base", "verify_gate", "retry"]

[profiles.nemotron.verify_gate]
surface_match = "exact"     # SurfaceMatch — the first experiment-derived knob

[profiles.nemotron.retry]
max_retries = 2
scope = "file"

[profiles.qwen-coder]
techniques = []             # qwen grounded unaided here — the light profile
```

Each knob maps to a real parameter: `surface_match` → `verify_gate::SurfaceMatch`
(today a function arg, exposed as a `ProfileConfig` field); `max_retries` →
`rig_retry_loop.sh --max-retries` today, an agentic-loop setting once retry is
in-loop. Knob values, like profile values generally, are **discovered by the
sweep, not authored** — the settings file is where a *measured* winner is pinned
(and where a human override is recorded, origin-tagged so the auto-tuner never
clobbers it — same rule as `model-capabilities.json` / `community-tunings.toml`).

## Composition + the part × model matrix

Because techniques are independently toggleable with their own knobs, the sweep
becomes a true **ablation instrument**: hold a model fixed, toggle one technique
(or step one knob) and measure the marginal lift. A profile is one cell — a
`(technique-set, knob-settings)` — and the matrix searches cells per model:

- `knowledge_base` alone vs. `+verify_gate` vs. `+retry` → marginal lift of each.
- `verify_gate{surface_match=exact}` vs. `{prefix}` → quantifies the Goodhart hole.
- `qwen-coder × nemotron-profile` → does the heavy stack cost a model that didn't
  need it (the cross-application lever).

This is how a technique earns its place in a profile: **measured marginal lift,
for that model, in that context** — not assertion.

## Staging — crawl / walk / run

- **Crawl:** a static technique registry in-binary + the `SurfaceMatch` knob as a
  `gate_*_with` parameter (already shipped on the R2 branch); a `--profile` flag
  selecting a hardcoded technique set (per `model-family-profiles.md` Crawl).
- **Walk:** `ProfileConfig { techniques: Vec<String>, <technique>: KnobTable }` in
  config, loaded through the existing layering; knobs read from the settings file.
- **Run:** the sweep writes *measured* knob winners into the profile (origin-tagged);
  the auto-tuner refines per session.

**Constraints:** additive and behind a flag (absent `--profile` = today's
behavior); harness-layer only (no chat-surface change — `plain_scroller_tui.md`);
coverage floor holds; every technique ships its spec sheet or it doesn't ship.

## Cross-links

- [`thinking-effort-and-plan-mode.md`](thinking-effort-and-plan-mode.md) — two planned techniques: `effort` (the reasoning dial, per-backend) + `plan_mode` (capture the reasoning as a canonical `Plan` and execute against it). Prereq: #385 (the `<think>` split). Tracks #381.
- [`model-family-profiles.md`](model-family-profiles.md) — the profile = composition design this fills in.
- [`../findings/2026-06-15-retry-and-the-honest-gate.md`](../findings/2026-06-15-retry-and-the-honest-gate.md) — why spec sheets + the gate-completeness caveat.
- #73/#354 (`verify_gate` + `SurfaceMatch`), #74/#353 (`knowledge_base`), #350 (the sweep), Phase 20 (the tuner that discovers knob values).

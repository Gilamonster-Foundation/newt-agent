# The model support kit

**Status:** framing / umbrella note · **Unifies:** [`model-family-profiles.md`](model-family-profiles.md) (assembly per family), [`technique-library.md`](technique-library.md) (the parts + spec sheets), [`thinking-effort-and-plan-mode.md`](thinking-effort-and-plan-mode.md) (reasoning/structure parts), [`retry-technique.md`](retry-technique.md), [`model-self-tuning.md`](model-self-tuning.md) (the tuner) · **Fitness function:** the ground-truth rig (#75) + the `--profile` sweep (#350)

## The concept

**newt is a model support kit.** You bring a model — especially a weaker, local, or
open one — and the kit supplies the *parts* its specific failure modes need. A strong
model needs few parts; a weak one needs a fuller assembly. Which parts a given model
needs is **discovered by measurement, per family**, not guessed.

This is the "harness as a compiler/linter/IDE **for a model**" framing, given a name:
the kit doesn't *use* a model, it *supports* one.

## Vocabulary (use these words)

| term | meaning |
|---|---|
| **model support kit** | the catalog of composable **support parts**, organized by **axis**, newt can apply to support a model. Model-agnostic — every part is a candidate for any model. |
| **support part** (= *technique*) | one named, reusable harness capability with a **spec sheet** + tunable **knobs**. The unit of reuse. |
| **axis** | a category of support (reasoning · structure · grounding · gating/repair). A profile picks across axes. |
| **profile** | a specific *assembly* from the kit — which parts, with which knob settings — selected and **measured** for a `(model family, context)`. "newt-agent for Nemotron" is a profile. |
| **the rig** | the fitness function (#75 + the sweep #350) that decides which parts earn a place in a profile. Measured, never asserted. |

A profile is not a monolith; it is *parts assembled from the kit*. The **part**, not
the profile, is the unit of reuse — a part built to help model A is a candidate
ingredient for B (cross-pollination is the point).

## The axes (the kit's shape)

| axis | question | parts (built ✅ / planned ⬜) |
|---|---|---|
| **reasoning** | how does it think? | `effort` ⬜ (the dial, per-backend) · `think` ⬜ (split — *always-on baseline* — + leverage) |
| **structure** | how is work decomposed? | `plan` ⬜ (plan → approve → execute; a *mode*) |
| **grounding** | what does it know? | `knowledge_base` ✅ (FFI surface) · *symbol-index* ⬜ |
| **gating / repair** | check & fix output | `verify_gate` ✅ (deterministic) · `review` ⬜ (LLM self-critique vs the gate) · `retry` ✅ (revert + re-prompt) |

`review` is the repo's north star brought in-loop: *"every PR review here will be done
by an arbiter LLM voting against a real CI gate"* (CLAUDE.md).

## The part contract (extends the spec sheet)

Every part carries [`technique-library.md`](technique-library.md)'s spec sheet, plus
three fields the kit needs to compose parts honestly:

- **axis** — which of the four it serves.
- **kind** — `per_turn` (e.g. `effort`, `verify_gate`, `review`), `mode` (e.g. `plan` —
  reshapes the turn into plan→approve→execute), or `loop` (e.g. `retry`). Kind tells the
  driver how to run it.
- **presupposes** — parts that must be present (`plan` ⊃ `effort`; `retry` ⊃
  `verify_gate`; `review` composes-with `verify_gate`). `Config::validate()` should warn
  when a profile lists a part without its presupposition.

## The pipeline (parts have an order)

A profile is an *ordered pipeline*, not a set — within a turn/session the parts run:

```
effort/think  →  plan  →  knowledge_base  →  PRODUCE  →  verify_gate  →  review  →  retry
  (reasoning)   (struct)   (grounding)                     (gating)     (gating)   (repair)
```

Toggling a part in/out of that fixed order is how a profile is assembled; the order
itself is the kit's, not the model's.

## Honest constraints (the discipline)

- **Correctness is not a toggle.** The `<think>` *split* (#385) is an always-on
  baseline whenever a model emits reasoning — an unstripped block corrupts output
  regardless of profile. Only the *leverage* of that reasoning is profile-worthy.
- **A part is offered with its failure mode, not as a verdict** — the gate-as-spec vs.
  -proxy caveat (the retry-Goodhart finding). The spec sheet is where the caveat lives.
- **Cost is real.** `review` and `plan` add tokens + latency; their spec sheets carry
  the cost caveat, and a part is enabled because the rig **measured** it lifting *that*
  family — never because it exists. The sweep is the discipline that tames the
  combinatorial blow-up.
- **Harness-layer only, additive, behind the profile** — no chat-surface change
  ([`plain_scroller_tui.md`](decisions/plain_scroller_tui.md)); absent a profile = today's behavior.

## Status / sequencing

Built (the first kit): `knowledge_base`, `verify_gate`, `retry` — the nemotron profile
is the first real assembly. Next parts, in order:

1. `think` **split** (#385) — correctness baseline; unblocks the nemotron rig leg (#80).
2. `effort` (#381) — the reasoning dial + the nemotron `detailed thinking` directive +
   probe-learned default (extends `emits_thinking`).
3. `plan` — capture reasoning → canonical [`Plan`](../../newt-core/src/plan.rs) → execute.
4. `review` — the in-loop arbiter pass.

Each lands as a focused PR with a rig-kept spec sheet, exactly like the first three.

## The paper

This is the publishable thesis (NTECH): *"newt-agent: a model support kit — harness as
composable, empirically-discovered per-model support parts."* Discovered-not-declared
all the way down.

Refs #381, #385, #338, #334, #220, #74, #73, #75, #350, #80.

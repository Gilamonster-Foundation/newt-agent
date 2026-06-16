# The model support kit

**Status:** framing / umbrella note · **Composed by:** [`loadout-composition.md`](loadout-composition.md) (the `loadout` — the top-level `provider → model → kit → role → settings` selection; the kit/bundle/profile here are three of its axes) · **Unifies:** [`model-family-profiles.md`](model-family-profiles.md) (assembly per family), [`technique-library.md`](technique-library.md) (the parts + spec sheets), [`thinking-effort-and-plan-mode.md`](thinking-effort-and-plan-mode.md) (reasoning/structure parts), [`retry-technique.md`](retry-technique.md), [`model-self-tuning.md`](model-self-tuning.md) (the tuner) · **Fitness function:** the ground-truth rig (#75) + the `--profile` sweep (#350)

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
| **model support kit** | the **catalog** of composable **support parts**, organized by **axis**, newt can apply to support a model. Model-agnostic — every part is a candidate for any model. (In code: the in-binary `ComponentRegistry`.) |
| **support part** (= *technique*) | one named, reusable harness capability with a **spec sheet** + tunable **knobs**. The unit of reuse. (In code: a registry entry + the seam it mounts at.) |
| **axis** | a category of support (reasoning · structure · grounding · gating/repair). A profile picks across axes. |
| **bundle** | the **loadable, swappable unit** — a manifest that pins a subset of the kit's parts + ships the profiles built from them + family→profile bindings + defaults. The thing you `load`. (In code: `[bundles.<name>]` + `--bundle`.) **Cannot carry authority** (no caveats field). |
| **profile** | a specific *assembly* — which parts, with which knob settings — selected and **measured** for a `(model family, context)`. "newt-agent for Nemotron" is a profile a bundle ships. |
| **the rig** | the fitness function (#75 + the sweep #350) that decides which parts earn a place in a profile. Measured, never asserted. |

The chain: the **kit** is what newt *has* (the catalog); a **bundle** is what you
*load* (a loadout pinned for a model class / use-case); a **profile** *tunes* it for
a specific `(model, context)`. The **part**, not the profile, is the unit of reuse — a
part built to help model A is a candidate ingredient for B (cross-pollination is the
point).

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

## Architecture (design-panel validated, 2026-06-15)

A 4-architecture design panel (incremental / distribution / typed-seams / constraints
lenses → judged → synthesized; all 25–28/30, no fatal flaws) converged on this shape.

**1. The kit = a typed `ComponentRegistry`** (grows `KNOWN_TECHNIQUES`, in
`newt-core/src/kit.rs`). Each part is an entry — `id` (preserves the string key),
`kind` (`provider | per_turn | loop | mode | request_knobs`), `axis`, `presupposes`,
**`tier` (`Headless | TuiOnly`)**, typed `knobs`. The three shipped parts populate it
byte-for-byte-equivalent: `knowledge_base` (provider, grounding, Headless),
`verify_gate` (per_turn, gating, Headless), `retry` (loop, gating, Headless,
presupposes `verify_gate`). `Tier` makes the amphibious split a *checked property* —
Headless parts fly on wyvern unchanged; a `TuiOnly` surface is **skipped, not errored**
when headless. `validate()` turns a missing presupposition into a **load-time error**.

**2. The bundle = a manifest** (`[bundles.<name>]`, struct `BundleConfig`): `about`,
`applies_to` (model-id prefixes, **longest-prefix-wins** reusing the `pricing.rs`
matcher; empty ⇒ a use-case bundle, never auto-inferred), `default_profile`, and a
`families` map. It ships ordinary `[profiles.*]` — a bundle does not invent a profile
format. It has **no `caveats` field**, so authority-by-omission is *structurally*
impossible (the #319/#332 "harness stamps" lesson at the load layer).

```toml
[bundles.nemotron]
about = "Support bundle for the nemotron family"
applies_to = ["nemotron"]                       # "nemotron3:33b" matches
default_profile = "nemotron"
families = { "nemotron" = "nemotron", "qwen" = "qwen-coder" }

[profiles.nemotron]
techniques = ["knowledge_base", "verify_gate", "retry"]   # order = the kit's pipeline
[profiles.nemotron.verify_gate]
surface_match = "exact"
[profiles.nemotron.retry]
max_retries = 2
```

**3. Resolution** — one total order, printed by the banner, all feeding the *existing*
`active_profile` slot so every downstream seam is today's path verbatim:
`--profile`/`NEWT_PROFILE` (explicit assembly wins) → `--bundle`/`NEWT_BUNDLE` →
inferred bundle (`applies_to`) → `None` = bit-for-bit today.

**Safety properties locked by the panel:** *data, not code* — a bundle is TOML naming
**vetted** parts; no dylib/plugin ABI, so a hostile/third-party bundle physically
cannot introduce mechanism, a widget, a tool, or authority (the same bet skills make).
*Tier* keeps the wyvern build strippable. *Frozen system prompt* (`memory.rs`) means
provider-kind parts can't hot-swap without `/new` — so the MVP is **static at
startup**; `/kit`-style hot-swap is a later increment that must force `/new` for
provider parts, never silently no-op.

## Status / sequencing

Built (the first parts): `knowledge_base`, `verify_gate`, `retry` — the nemotron
profile is the first real assembly. The migration is additive, behind the flag, and
bit-for-bit default-preserving — ordered PRs:

- **PR-1 (Crawl, internal):** the `ComponentRegistry` schema over the existing 3 parts;
  `validate()` enforces `presupposes` (load-time error). No `[bundles]`, no `--bundle` —
  a golden test asserts the resolved technique set is byte-identical to today.
- **PR-2 (Walk):** `BundleConfig` + `[bundles.<name>]` + `--bundle`/`NEWT_BUNDLE` + the
  resolution arm + `announce_bundle` (prints which selector won); built-in bundles for
  zero-config.
- **PR-3 (Walk):** disk bundles (`~/.newt/bundles/*.toml`) + family auto-inference.
- **PR-4 (Run):** reconcile this doc's `kind` table; wire `Tier::TuiOnly` skip-when-headless.

Then the **new parts**, each its own PR + rig-kept spec sheet, in order:

1. `think` **split** (#385) — correctness baseline; unblocks the nemotron rig leg (#80).
2. `effort` (#381) — the reasoning dial + the nemotron `detailed thinking` directive +
   probe-learned default (extends `emits_thinking`).
3. `plan` — capture reasoning → canonical [`Plan`](../../newt-core/src/plan.rs) → execute.
4. `review` — the in-loop arbiter pass.

### Open questions (from the panel)

- The `mode` kind (`plan`'s approval surface) vs. the plain-scroller guardrail — needs
  its own decision doc before `plan` lands.
- Fixed in-binary `KnobSchema` vs. the sweep wanting to *discover* + write back knob
  values — a registry-release-cadence policy is needed before Run/Fly.
- Family-prefix collision semantics (`qwen` vs `qwen2.5` vs unrelated) — a test matrix
  before auto-inference (PR-3); decide miss → `default_profile` silently vs. warn.
- `[bundles]` vs `[modes]` (#307) overlap once a `mode`-kind part can live in a bundle —
  documented precedence needed.

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

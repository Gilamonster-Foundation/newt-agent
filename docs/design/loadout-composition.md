# The loadout — one nameable composition of provider → model → kit → role → settings

**Status:** design note (pre-build) · **Sits above:** [`model-support-kit.md`](model-support-kit.md) (the kit/bundle/profile layer this composes) · **Depends on:** the catalog/model-cards epic (#387/#384/#383/#382), the kit bundle PRs ([`model-support-kit.md`](model-support-kit.md) §Architecture) · **Reuses:** `RoleProfile`/personas (`newt-core/src/role_profile.rs`), `ModelTuning`/`find_model_tuning` + `CapabilityEntry`, `NamedPermissionPreset`/`meet`, `Config::resolve`/`merge_toml`, `ModeConfig` (#307)

## Abstract

A complete newt configuration is a *path*: `provider → model → kit → role →
settings` — e.g. `dgx → nemotron → nemotron-kit → python-developer`. Those five
levels **already exist as orthogonal seams** (or are in-flight); what's missing is a
thin **composition layer** that ties them into one nameable, selectable, shareable
thing: a **loadout**. A loadout is a *dispatcher, not a merger* — it selects one input
per axis and hands each to the resolver that already owns it, so the axes stay
orthogonal (authority `meet`-only, parameters keep their chain, the prompt stays
soul+persona+framing) and a loadout can **never widen authority**.

## Vocabulary (locked)

The loadout is one new top noun; nothing below it is renamed.

| term | meaning | seam |
|---|---|---|
| **loadout** | the one nameable composition — a value per axis + per-axis overrides | net-new `[loadouts.*]` + `LoadoutResolver` |
| **provider** | where models come from (endpoint, kind, auth, owner) | catalog `provider.toml` / `BackendConfig` |
| **model** (+ card, `@variant`) | the model + its unified honest metadata | catalog `models/<slug>.toml` / `CapabilityEntry`+`ModelTuning` |
| **kit** (= bundle) | a shippable catalog-of-parts + the profiles it ships | `[bundles.*]` (in-flight) |
| **profile** | a per-`(model, context)` **assembly** of techniques + knobs — *unchanged* | `ProfileConfig`/`[profiles.*]` |
| **role** | a model-agnostic persona: prompt + tools + caveats + model/tier hints | `RoleProfile` / `~/.newt/personas/*.md` |
| **settings** | per-axis overrides (authority clamp / parameters / framing) | `ModelTuning` / presets / `framing` |

`loadout` = what you *load*; the five levels are the axes it pins. It does **not**
replace `--profile`/`--mode`/`--persona` — it's a named default you can still poke
with an explicit per-axis flag.

## The model — dispatcher, not merger

**Source of truth:** a named object `[loadouts.<name>]` (same grain as `[modes.<name>]`),
every field optional, a missing reference is a **hard error** (never a silent no-op —
the `ModeConfig` rule). **CLI sugar:** an address string parsing to the same struct,
and `/loadout` prints the resolved selection *back* as an address + per-axis
provenance (honest about which selector won each axis).

```toml
[loadouts.dev-nemotron]
provider = "dgx"                  # → catalog/<provider>/provider.toml (#387)
model    = "nemotron@deep"        # → catalog model-card + the @deep setting-variant
kit      = "nemotron-kit"         # → the bundle (catalog of parts + its profiles)
profile  = "nemotron"             # OPTIONAL; omitted ⇒ the bundle/model implies it
role     = "python-developer"     # → ~/.newt/personas/python-developer.md (RoleProfile)
  [loadouts.dev-nemotron.settings]
  num_ctx = 24576                 #   parameter-axis override (top of the ModelTuning chain)
  framing = "Ship small, verify." #   prompt-axis override (ModeConfig.framing-shaped)
```

```
newt --loadout dev-nemotron
newt --loadout dgx/nemotron@deep/nemotron-kit/python-developer   # inline address
newt --model nemotron --role python-developer                    # partial; other axes inferred
/loadout dev-nemotron        # mid-session switch (atomic, mirrors /mode)
/loadout                     # show the resolved address + per-axis source
```

The resolver does **not** produce one merged blob — it produces a struct of
already-resolved-per-axis outputs, each via that axis's own resolver and precedence:

```rust
// newt-core (net-new); called at session build + on /loadout switch
struct ResolvedLoadout {
    backend:   BackendChoice,   // axis 1: catalog / resolve_backend_choice (url, model, kind, api_key)
    profile:   ResolvedProfile, // axis 2: kit resolve_profile → AgenticConfig baseline (or default)
    prompt:    PromptOverlay,    // axis 3: persona prompt + framing (rebuild_system_prompt)
    authority: Caveats,          // axis 4a: base.meet(role.caveats).meet(preset.clamp) — CLAMP ONLY
    params:    ModelTuning,      // axis 4b: loadout.settings → [[model_tuning]] → [tui] → empirical → default
    sources:   LoadoutSources,   // per-axis provenance, for /loadout's honest output
}
```

This is why the axes stay legitimately different: authority/technique/parameter/prompt
remain four functions; the loadout only *selects their inputs* and *records
provenance*.

### Worked example — `dgx → nemotron → nemotron-kit → python-developer`

1. **provider/model:** `dgx`→`provider.toml` (endpoint, kind, api_key); `nemotron@deep`
   →the model-card + its `[effort.deep]` variant → `BackendChoice`.
2. **kit/profile:** `nemotron-kit` + `nemotron`→`resolve_profile`→the technique baseline
   (knowledge_base + verify_gate(exact) + retry). Absent the kit ⇒ today's defaults.
3. **role:** `python-developer`→`RoleProfile`; its `prompt` joins the system prompt; its
   `caveats` lower to a **clamp** (below); `model`/`tier`/`tools` are hints (model hint
   can seed axis 1 when the loadout omits a model). Enforcement of `tools`/`caveats`
   stays deferred (`role-profiles.md`).
4. **settings:** `num_ctx=24576` enters the *top* of the parameter chain; `framing`
   enters the prompt overlay as one line.

Printed back: `dgx/nemotron@deep/nemotron-kit/python-developer  (provider: catalog,
model: catalog@deep, profile: explicit, role: persona, num_ctx: loadout)`.

## Reuse vs net-new

- **Reused:** catalog #387 (provider/model), `ProfileConfig`/`resolve_profile` (kit
  axis), `RoleProfile`/`PersonaStore` (role), `ModelTuning`/`find_model_tuning` +
  `CapabilityEntry` (params), `NamedPermissionPreset`/`meet` (authority),
  `Config::resolve`/`merge_toml` (layering), `ModeConfig.framing` (prompt).
- **Net-new (small):** the `[loadouts.*]` object + `LoadoutResolver`; **role as a
  first-class selectable** (today only `--persona`); a `--model` flag (doesn't exist
  yet); binding role/settings under a model.

## Authority safety — a loadout cannot widen authority

Three structural guarantees, all reusing existing mechanism:

1. **The authority axis is `meet`-only by construction.** Effective authority is
   `base.meet(role.caveats).meet(preset.clamp)` — lattice intersection, which can only
   narrow. A loadout *referencing* a role/preset can therefore only attenuate.
2. **`settings` has no widening grammar.** Authority overrides are restricted to the
   clamp-only `NamedPermissionPreset` shape (readonly / exec_allow / deny / max_calls);
   there is deliberately no "grant" field. A loadout can name a preset; it cannot author
   a grant.
3. **The default-deny base is untouched.** The session base is harness-minted ("the
   harness stamps, the model never asserts", #319/#332); the signed `AgentKey`
   delegation remains the unforgeable authority. A loadout selects *inputs to a clamp*,
   never the base — so a loadout naming `role = full-access` still `meet`s to ≤ the
   harness base and can never exceed `Caveats::top()`.

Net: on the authority axis a loadout is **isomorphic to today's `/mode` clamp** — same
`meet`, same hard-error-on-missing-reference, same "ceiling wins over `--yolo`".

## Build roadmap (additive · behind `--loadout` · degrades when deps absent)

Role **enforcement stays deferred** throughout — a loadout may *select* a role before
the harness enforces its caveats/hints (its `prompt` + the authority clamp already
work).

- **Slice 0 (inert):** `Loadout` struct + `[loadouts.<name>]` map + reference
  validation + `/loadout` show-only. Zero behavior change. *Depends: nothing.*
- **Slice 1 (ships today):** `LoadoutResolver` for the **role + settings** axes (both
  resolvers exist now) + `--loadout`/`--model`/`--role` flags, gated on `--loadout` ⇒
  absent it's bit-for-bit today.
- **Slice 2 (alongside catalog #387):** the provider/model axis — catalog-aware backend
  resolution honoring a pinned provider/`model@variant`; no-ops if the catalog is absent.
- **Slice 3 (alongside the kit bundle PRs):** the kit/profile axis (`resolve_profile` →
  baseline; "model implies profile, explicit `profile` overrides").
- **Slice 4 (ergonomics):** address-string parse/print, `/loadout` atomic mid-session
  switch, per-axis provenance, project-local `[loadouts.*]` layering.

## Open questions

1. `kit` vs `profile` field redundancy — default `profile` to the bundle/model-implied one.
2. Address-string grammar for omitted slots — recommend an explicit `-` placeholder.
3. Where `[loadouts.*]` live — `config.toml` only vs a shareable pack (align with the
   community-tuning format).
4. `model@variant` ownership — the catalog defines the variant set, the profile selects
   among them (needs the catalog epic's input).
5. `/loadout` switch atomicity — recommend all-or-nothing (like `/mode`); does it force
   a fresh conversation?
6. role `model` hint vs explicit `model` — explicit wins (matches `--flag` > config).

Refs #387, #384, #383, #382, #338, #334, #307, #319, #332.

# Nudger — the harness's disposition (working name)

> **Status:** design, not yet built. Working name `/nudger` is deliberately
> provisional (see Naming). This doc is the plan; GitHub issues will be the
> state once the phases open.

## The idea, and the flip

The harness has a set of numeric "how hard do I push the tool loop" knobs —
`max_tool_rounds`, `workflow_grace_rounds`, `narration_nudge_cap`,
`note_nudge_interval`, `mid_loop_trim_threshold`, the context-budget nudge
thresholds. Today they're config/per-model tuning; there's no single
user-facing dial.

The instinct is to add `/effort {low,medium,high,max}` — a friendly slider.
**That instinct is upside-down.** `/effort` is the *finished sculpture*: a lossy
linear projection that hides a multi-dimensional truth. The fundamental is the
**profile** — a named bundle of knob values (a *posture*/*temperament*). `/effort`
is one *label-projection* onto an ordering of those profiles, and it should be
**data-mined from what operators actually use, not designed up front.**

Two load-bearing consequences:

1. **The axis is not monotonic.** `effort-ultra`/`effort-crew` aren't "more than
   max" on one slider — they're a *different kind* (the multi-agent workflow
   shape). Off-axis profiles exist. So "position on the effort ladder" is
   **optional metadata (`rank`), not intrinsic.** A profile with no rank is
   usable by name but invisible to the slider.
2. **`/effort` is earned.** Ship the ladder + profiles + usage, watch what people
   reach for, *then* mine the `low/medium/high/max` boundaries. Designing presets
   now guesses the answer the experiment is meant to produce.

## The pattern we mirror

`newt-core/src/model_card.rs` already is exactly this shape and is the template
to copy 1:1:

- All-`Option` structs with `#[serde(deny_unknown_fields)]`, field-by-field
  `merge()` via `.or()`.
- `parse_*(contents, ext)` dispatching TOML/YAML.
- Built-in seeds ship **compiled in** via a `const` array of `include_str!(...)`
  — adding a built-in = a new `.toml` + one array line (config, not code).
- `load_dropin_dir(dir)` best-effort scans `~/.newt/<kind>/*.toml|yaml`
  (missing dir → empty; bad file → skipped, not an error).
- `resolve(builtin, dropins, one_off)` = merge-by-name with a precedence chain.
- Loud typos where it matters (`deny_unknown_fields`, `validate()`), silent
  forward-compat at runtime.

## Architecture decisions (reconciled from four parallel designs)

- **Storage = open map; resolver = key lookup.** A profile is
  `{ name: String, rank: Option<i32>, description: Option<String>, knobs: BTreeMap<String,i64> }`.
  The `[knobs]` table is an **open extensible map** (config-not-code
  extensibility). A single `KNOWN_KNOBS` table maps each knob key → its
  per-turn cascade site + a **scope tag** + `(min,max)`. The resolver reads the
  map *by key* (`resolve_knob(profile, model_tune, key, config_fallback)`), which
  keeps the hot-loop change to one-line swaps. We deliberately do **not** reuse
  `newt_tuner::ModelTuning` as the profile type.
- **Scope tags** (the "does this knob even apply per-model" question): (a)
  *per-model-eff* — folded at the `eff_*` block (`lib.rs:4828-4845`); (b)
  *global-inline* (`input_ceiling_pct`, `low_budget_pct`) — overlaid just before
  the existing `.clamp()` at ChatCtx build; (c) *session-once*
  (`note_nudge_interval`) — takes effect next session only (the `NoteNudge`
  counter is seeded once). `show` must annotate scope so a knob's reach is
  legible.
- **Ordering field is `rank`, not `effort_order`.** Naming it after `/effort`
  would weld the ladder to a projection we said must be earned. `rank:
  Option<i32>`, sparse (10,20,30…), `None` = off the axis. `cmp_axis =
  rank.cmp().then(name.cmp())` is the canonical total order (also the
  same-rank tiebreak → deterministic + a `validate()` lint, never a silent
  blend).
- **crew/ultra are off-axis solely by withholding `rank`** — one mechanism
  (absent rank), same as an unranked user drop-in. No load-bearing
  `kind: Linear|Shape` field.
- **Unknown-knob policy splits by surface:** `deny_unknown_fields` on the
  *structural* keys (a typo'd `[knbos]` or `efort_order` is a hard error), but
  keys *inside* `[knobs]` are open — `resolve()` ignores unknown keys silently
  (runtime compat) while `nudger validate` surfaces them and **exits non-zero**.
- **`/rounds` stays the outermost override.** Final precedence, low→high:
  `baked default < [tui] config < [[model_tuning]] < active nudger profile <
  explicit /rounds session override`. For `max_tool_rounds`, the profile feeds
  the `configured` baseline and `effective_tool_round_limit(configured,
  max_tool_rounds_override)` is applied unchanged so `/rounds` keeps winning.
- **Knob value type = `i64`** in the map (stores the `10000` "effectively
  unlimited" sentinel, room for future negative sentinels), with **checked
  casts** at every consume site (the harness uses `usize`/`u32`) — saturate/skip,
  never panic. `validate()` range-checks against `KNOWN_KNOBS`.

## Phased plan (strictly additive; each PR small)

**Phase 0 — dormant core module** (`newt-core/src/nudger.rs`, ships fully unwired
→ cannot change runtime behavior):
- PR1: `NudgerProfile` schema + `parse_profile` + `merge()` + `validate()` +
  `KNOWN_KNOBS` (scope tags & ranges). Fs-free tests.
- PR2: ~5 ranked linear built-in seed `.toml`s (ordered by conservatism, **not**
  labelled low/med/high) + one deliberately-unranked seed + `builtin_profiles()`
  `include_str!` array + `resolve()` merge-by-name + `load_dropin_dir`.
- PR3: ordering projection — `cmp_axis`, `axis()`, `rank_of()`,
  `step(current,delta)`, `renumber()` returning a pure diff. Tie-determinism
  tests.
- PR4: `nudger_config_dir()` (copy `classifiers.rs` pattern) added to **both**
  `Config` impl paths (so `--no-default-features` compiles) + `catalog()`
  source-tagging.

**Phase 1 — resolver integration** (additive hot-loop fold, still inert —
`active_nudger` stays `None` until Phase 2):
- PR5: `resolve_knob(...)` helper + `let mut active_nudger: Option<NudgerProfile>
  = None;` (beside `max_tool_rounds_override`, `lib.rs:3198`) +
  `let profile = active_nudger.as_ref();` after `find_model_tuning`. Unit-test
  the `None ⇒ today's expression` equivalence.
- PR6: swap the per-model-eff bindings (`lib.rs:4832-4845`) to `resolve_knob` —
  **supplying the config fallback that `workflow_grace_rounds`/
  `narration_nudge_cap` currently lack** (they're `model_tune`-only today; verify
  byte-for-byte against the live expression, do NOT assume symmetry with
  `max_tool_rounds`). Keep the `mid_loop_trim` `.min(max-3)` clamp.
- PR7: global-inline knobs overlay (`input_ceiling_pct`/`low_budget_pct`) before
  the `.clamp()` at ChatCtx build. `note_nudge_interval` documented
  session-fixed.

**Phase 2 — `/nudger` read + activate command** (first observable/reachable
surface; mirrors the `/rounds` trio):
- PR8: `nudger_command_arg` matcher + `NudgerCommand` enum + `parse_nudger_command`
  + slash-dispatch block beside `/rounds`. Implements `show` + `list` (read-only:
  rank column, active marker, off-axis section).
- PR9: `use <name>` (assign `active_nudger`), `reset`, `up`/`down` (`step` along
  axis), `reload`.
- PR10: `validate [name]` (loud, non-zero exit) + per-knob provenance in `show`
  (profile / model-tuning / config-default) + drop-in vs built-in(overridden)
  tags.

**Phase 3 — authoring / custom profiles** (pure extension — only writes files the
Phase-0 loader already reads):
- PR11: `set <knob> <value>` (fork overlay → "custom (based on <base>)", drops
  off the ladder) + `unset` + `diff`.
- PR12: `save <name> [--rank N]` → `~/.newt/nudger/<name>.toml`.
- PR13: `renumber [--write] [--start N] [--step M]` (persists built-in re-ranks as
  thin rank-only override files).

## Risks (top hazards to test)

- **i64 → usize/u32 casts:** negative or `>u32` value must be rejected by
  `validate()` and skipped (not panicked) by `resolve()`. Top correctness hazard.
- **Open map loses compile-time typo detection:** a mistyped knob is silent at
  `resolve()`. Mitigate with `validate()` non-zero exit + surface
  `(unknown, ignored)` in `show`/`list`.
- **Two `Config` impl blocks** (full vs `--no-default-features`): `nudger_config_dir()`
  must compile in both, or the strip-down CI lane breaks.
- **`mid_loop_trim_threshold`** is re-clamped to `max_tool_rounds-3` downstream —
  `validate()` should warn when a profile sets it too high (silently ineffective).
- **`low_budget_pct` has a hard `.clamp(1,50)`** — a profile setting `0` to
  "disable" becomes `1`. Don't advertise a disable that doesn't exist.
- **`note_nudge_interval` session-fixed** — `/nudger use` won't move it mid-session;
  `show` annotates "(takes effect next session)".
- **Precedence policy call:** active profile *outranks* ambient `[[model_tuning]]`.
  Intended (explicit activation beats passive per-model config) — flag for
  explicit confirmation; the inverse is defensible.
- **`renumber --write`** minting thin override files can leave inert same-value
  overrides / clutter `catalog()`. Needs a GC/absorb decision before Phase 3 (or
  accept clutter for v1).

## Explicitly NOT in v1 (YAGNI)

- **`/effort` itself** — the label→rank map + `/effort <level>`/`auto` sugar.
  Earned from usage, not designed now.
- **`auto` mode + its "pressure" signal** (budget %, round exhaustion,
  repair-progress) — that's the actual nudge *policy*, a separate design. Build
  `axis()/step()/rank_of()` + manual `up`/`down` only.
- **Tensor / multi-D interpolation** between profiles — `auto` (when it exists)
  *snaps* to a ranked profile; never blend two hand-curated bundles.
- **Textual-prompt dimensions** — a future *separate* `[prompts]` axis with its
  own value type, structurally distinct from the numeric `[knobs]` map. Do not
  anticipate in v1.
- **Live re-seed of `note_nudge_interval`**, per-model-keyed `active_nudger`, a
  `--nudger` CLI flag, a family-defaults shared-base layer, unifying `/rounds`
  into the overlay, `no_hardware_leak()`-style guard (profiles are pure ints).

## Naming

`/nudger` names one mechanism for a broader concept: the harness's
**disposition / temperament** — how hard it pushes, how much slack it gives, its
persistence posture per turn. Hold loosely: `/disposition`, `/temperament`,
`/posture`, `/stance`, `/grit`, `/gumption`. Leaning **disposition** or
**temperament** (accurate, model-agnostic, doesn't pre-commit to "effort").

Two disciplines matter more than the final word: (1) keep the ordering field
`rank`/`order`, never `effort_order`; (2) the module (`newt-core/src/nudger.rs`),
drop-in dir (`~/.newt/nudger`), and seeds rename cheaply *while dormant* — so
**pick the real name before Phase 2** exposes a user-facing slash command, after
which it's expensive to change.

---

*Design produced by an 8-agent design pass (3 codebase scouts → 4 parallel
component designs → adversarial synthesis), 2026-07-12. `/effort` is deliberately
the last thing built, not the first.*

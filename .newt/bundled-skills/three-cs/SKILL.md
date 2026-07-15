---
name: three-cs
description: The three Cs — Composition, Configuration, Convention. Knowledge belongs in DATA, not logic — hardcoded keyword lists, magic constants, and domain rules become pure data that is composed, configured (droppable/overridable), and convention-driven. But working code over all — hardcode to ship, then circle back and de-hardcode.
when_to_use: When you're about to hardcode a list, table, or constant that encodes language- or domain-specific knowledge; when reviewing code and a match arm / keyword list / magic number smells like it will grow; when adding "one more case" to an existing hardcoded list (the second case is the signal); when designing a new subsystem and deciding what is code vs. what is data; when a user asks to support a new language/domain/format and the answer today would be "edit the source".
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: ["git"] }
  fs_read: all
  net: { only: [] }
  max_calls: unlimited
---

# The Three Cs — Composition, Configuration, Convention

The house data-vs-logic doctrine. One sentence:

> **Knowledge belongs in data, not logic.** Language- or domain-specific
> knowledge — keyword lists, magic constants, recognition rules, mappings —
> should be pure data that is *composed*, *configured* (droppable /
> overridable), and *convention-driven*, so a new language or domain is
> **config, not code**.

The three Cs, unpacked:

- **Composition** — knowledge comes in units that combine. Packs merge by
  name; layers stack (built-ins → global drop-ins → project drop-ins →
  inline config); a later layer adds or overrides without touching the
  earlier ones. The engine never knows how many packs exist.
- **Configuration** — every knowledge unit is droppable and overridable by
  the operator: a `.toml` in a drop-in directory, an inline config section,
  an overridable property. Never a recompile. Never an inline `json!`/list
  literal pretending to be config (that's code wearing a data costume).
- **Convention** — the *shape* and *location* of the data follow house
  conventions so new units need no wiring: `<name>.toml` in a known drop-in
  dir, merge-by-name, tolerant loading (a malformed file is skipped loudly,
  never fatal). Convention is what makes composition and configuration cheap.

## Why this matters

Hardcoded knowledge is **lock-in at the source level**. A keyword list baked
into a `match` means every new language is a source patch, a review, a release.
The same list as a droppable pack means a new language is a file a user writes
at their desk. The doctrine is the workspace philosophy applied to code:
plain-text, replaceable, no lock-in — don't confuse the instrument (the engine)
with the thing observed (the knowledge).

There's a second payoff for agents specifically: pure data is **legible and
diffable**. An agent (or a reviewer) can audit a `.toml` of rules at a glance;
auditing the same knowledge threaded through control flow requires reading the
whole function.

## The load-bearing counterweight: working code over all

**Functional results come first.** It is fine — *expected* — to compromise to a
hardcoded, simple implementation to **get a feature working**. Do not let the
three Cs block shipping a working result. A hardcoded list that ships today
beats a pluggable pack system that ships next month.

Then **return to the three Cs**: once it works (tests green, behavior proven),
refactor the hardcoded values into pure-data config, composition seams, and
conventions. The sequence is deliberate — the working version teaches you the
real shape of the data before you design its schema.

The discipline in both directions:

- Don't gold-plate first: no speculative pack systems for knowledge with one
  known instance. (YAGNI still binds; the *second* instance is the signal.)
- Don't skip the circle-back: when you hardcode to ship, **flag it** — a
  `// three-Cs: de-hardcode into <pack/config>` comment or a follow-up issue.
  A hardcoded list that encodes domain knowledge and has no flag is debt
  hiding as design.

## What counts as "knowledge" (and what doesn't)

Move to data:

- **Keyword / phrase / pattern lists** — language keywords, domain phrases,
  recognition regexes, redaction patterns.
- **Magic constants that encode a domain fact** — model context windows, file
  extensions per language, danger classifications per command.
- **Mappings and tables** — command → tool, extension → language, verb →
  capability class.
- **Schemas** — an inline `json!` schema literal is an anti-pattern; it
  belongs in a data file with an overridable property.

Keep in code:

- **Invariants and safety floors** — the danger-tier *enforcement*, the
  attenuation-only rule, precedence laws. The *table* of dangerous targets is
  data; the rule "high-danger is never auto-allowable" is logic. Policy
  *mechanics* are code; policy *content* is data.
- **Algorithms** — the merge, the matcher, the renderer. The engine is code;
  what it consumes is data.

Litmus test: **would a competent operator ever want a different value without
wanting different behavior?** If yes, it's configuration. If changing it changes
what the program *means*, it's code.

## What "good" looks like (the canonical example)

The **language-pack** model (`newt-core/src/api_surface.rs`): the API-surface
engine needs to know, per language, the file extensions, entry-point globs, and
symbol-extraction rules. That knowledge was once hardcoded per-language; now a
`LanguagePack` is pure data:

- **Built-in packs** (Rust, Python, Bash, C/C++, Go, Java) ship as data in the
  binary and double as canonical examples.
- **External packs**: drop a `<name>.toml` into `~/.newt/language-packs/`
  (global) or `.newt/language-packs/` (project-local), or inline in config.
- **Merge-by-name**: a community Ruby pack *adds*; a local `rust.toml`
  *overrides* the built-in — without ever touching the binary.
- **Tolerant loading**: a malformed pack is skipped loudly, never fatal.

Adding a language is *config, not code*. The same shape repeats across the
house: model cards, nudger profiles, backend drop-ins (`~/.newt/backends/`),
the OCAP policy store (`~/.newt/ocap/*.toml`), the danger-tier table. When you
build a new drop-in, copy these conventions — same layering, same
merge-by-name, same tolerant loading — so operators learn the pattern once.

## How to apply it (the de-hardcode recipe)

1. **Find the knowledge.** In the working (possibly hardcoded) code, identify
   what is *domain fact* vs. *mechanism*. The facts move; the mechanism stays.
2. **Design the data shape from the working code** — the fields you actually
   needed, not the fields you imagine. Prefer flat, obvious TOML.
3. **Ship the built-ins as data too.** The hardcoded values become the
   built-in layer expressed in the same schema — they double as documentation
   and as the test fixtures.
4. **Wire the three Cs**: layered composition (built-in → global → project →
   inline), merge-by-name override, conventional drop-in location, tolerant
   loading with loud skips.
5. **Test the engine against data, not constants.** The unit tests feed
   crafted packs/tables; adding a domain never adds a test of the engine.
6. **Document the drop-in** (one example file in `examples/`, one line in the
   relevant doc) — a pluggable seam nobody knows about is a hardcoded list
   with extra steps.

## Relationship to the other doctrines

- **Functional cohesion** (see the `functional-cohesion` skill) decides *where
  units live and how they connect*; the three Cs decide *what a unit is made
  of*. They compose: the target is a cohesive, loosely-coupled module whose
  knowledge is pure data behind a narrow seam.
- **Library/consumer split**: libraries expose structs/API seams and pure-data
  contracts; consumers supply the UI and the operator's config. A library that
  hardcodes consumer knowledge violates both doctrines at once.
- **TDD**: data-driven engines are the easiest things to test — the fixture
  *is* a pack. If testing requires patching constants, that's the three-Cs
  smell showing up in the test suite.

## Checklist

- [ ] Does any list/constant/table here encode **language- or domain-specific
      knowledge**? → candidate for data.
- [ ] Is there an inline `json!` / literal schema or config? → move to a data
      file with an overridable property.
- [ ] Can an operator add/override a knowledge unit **without recompiling**
      (drop-in file, merge-by-name)?
- [ ] Does loading **tolerate** a malformed unit (skip loudly, never fatal)?
- [ ] If you hardcoded to ship (correct choice!): is the **circle-back
      flagged** (comment or issue)?
- [ ] Are the built-ins expressed **in the same schema** as the drop-ins?
- [ ] Did the safety floors stay in **code** (only the content moved to data)?

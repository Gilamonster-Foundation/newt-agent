---
name: functional-cohesion
description: How to organize long-lived code — Loosely Coupled, Functionally Cohesive. Group what works together to do one job into a unit named for that job; expose only the seam. The house structural doctrine for this repo, paired with TDD.
when_to_use: When a source file has sprawled and you're deciding how to split it; when naming a new module; when reviewing whether code is in the right place; when a change "here" keeps breaking something "there"; any time you feel cognitive load rising because you can't hold a file in your head. Reach for this before any large reorg or module extraction.
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: ["git"] }
  fs_read: all
  net: { only: [] }
  max_calls: unlimited
---

# Loosely Coupled, Functionally Cohesive

The house structural doctrine. Two properties, always together:

> **Cohesive:** everything in a unit is there because it works together to do
> **one job**. The unit is named for that job.
>
> **Loosely coupled:** the unit reaches the rest of the system through a
> **narrow, deliberate seam** — and nothing wider. You can understand it, change
> it, test it, or lift it out without dragging the world behind it.

Cohesion is what goes *inside* the box. Coupling is how many wires cross the
box's *edge*. You want a full box with few wires.

## Why this matters (and when)

Code is read far more than it is written, and long-lived code is read by people
(and agents) who did not write it, months later, under time pressure. The
scarce resource is not CPU — it's **the reader's working memory**. Good
structure is **cognitive-load management**: it lets a reader hold one job in
their head at a time and trust that the rest is behind a seam they can ignore.

The three disciplines this serves:

- **Pattern matching** — cohesive, well-named units are patterns a reader
  recognizes instead of re-derives.
- **Repetition reduction** — a cohesive home for a job is where the duplicate
  copies collapse into one.
- **Cognitive-load management** — a narrow seam means you only load the unit
  you're touching, not its whole neighborhood.

**Honest caveat about scale.** The *real need* for this surfaces only with a
large corpus. On a small program, disciplined **TDD** (write the test first,
let the design fall out of what's easy to test) makes a lot of this take care of
itself — tightly-coupled code is painful to test, so TDD pushes you toward seams
without you naming the principle. Past a certain size the tests are not enough:
you have to organize *deliberately*. TDD and this doctrine are partners — TDD
proves each unit; cohesion decides where the units live.

> **The computer thinks like a submarine swims.** It gives us a *mechanical
> approximation* of our thoughts. If we are not swimming well — if our own model
> of the problem is a tangle — the machine cannot mechanically swim well either.
> Organizing for cohesion is organizing your own thinking first; the code is the
> cast of it. -- Shawn Hartsock

## Cohesion has kinds — insist on the *functional* one

Not all "grouping" is equal. The classic ladder, worst to best:

- **Coincidental** — thrown together for no reason ("utils.rs", a junk drawer).
- **Logical / by-category** — grouped because they're the *same kind of thing*:
  a `constants.rs`, a `settings.rs`, a `types.rs`, an `enums.rs`. This is the
  seductive trap. It *looks* organized, but a reader chasing one feature has to
  visit five category-files to assemble it.
- **Functional** — grouped because they *work together to do one job*. The vi
  editor's state, its key handling, and its motion helpers live together because
  they collaborate to be vi — not because they happen to be "the structs."

**Prefer functional cohesion.** A `constants.rs` / `settings.rs` file is fine in
small doses and actively harmful taken too far — it scatters each feature across
category boundaries. The trick is balance: put the prompt's tokens next to the
prompt's parser next to the prompt's handler, because someone asking "how does
`/prompt` work" should open **one file named for that job**.

Litmus test: **"which code does X?" should answer itself** by pointing at one
well-named unit. If the answer is "bits of it are in four files," cohesion is
wrong regardless of how tidy each file looks.

## Loose coupling — expose only the seam

Cohesion without loose coupling is just a big box with a hundred wires. The
coupling rules:

1. **Publish the narrowest surface that works.** In Rust: `pub(crate)` exactly
   the items the other side reaches, and *nothing wider*. If a caller reads one
   field, expose one field — not the struct. If the whole family funnels through
   one entry point, expose one `dispatch()` and keep the rest private.
2. **Prefer a thin interface over shared internals.** A caller that routes to
   `family::dispatch(cmd, args)` is loosely coupled. A caller that reaches into
   `family`'s state machine is not. Route; don't reach.
3. **A high reach-count is a coupling smell.** If a struct is referenced 27
   times threaded through one giant function, that function and that struct are
   one tangled unit pretending to be two. Either give it a real seam or admit
   they're one thing.
4. **The seam is the documentation.** A module's `pub(crate)` list *is* its
   contract. Keep it small enough to read.

## How to actually do it — the non-breaking extraction

When a file has sprawled, split it. **First pass is pure, non-breaking
code-motion.** Do not "improve" while you move — moving and refactoring at the
same time is how you introduce bugs you can't see. Behavior frozen → refactor
fearlessly. Then, and only then, a separate pass may refactor the seam.

The mechanical recipe (Rust; the shape generalizes):

1. **Pick the unit by job**, not by kind. Find the cohesive cluster (a
   subsystem, a command family, an editor mode) and its boundaries — including
   its co-located tests.
2. **Move code + tests verbatim** into a new file named for the job.
   Co-located `#[cfg(test)]` tests move *with* the code they test; a child test
   module still sees the parent's private items.
3. **Rebalance imports both ways.** The new module imports what it reaches in
   the old; the old module sheds imports whose only consumer just left (watch
   for the macro-expansion false-positive where an "unused import" warning is
   really the *old* file's, its consumer having moved out).
4. **Make cross-references `pub(crate)`** — but only the exact items each side
   reaches. This step *is* the coupling audit: a long list means the split line
   is wrong; a short list means you found a real seam.
5. **Preserve the public API** with `pub use` re-exports so external callers
   don't notice the move.
6. **Prove it's non-breaking**: `fmt` + `clippy --all-targets` clean, the full
   test suite green, downstream crates still build. The tests didn't change, so
   green means behavior didn't either.

Land each cohesive unit as **its own small PR** (one concern per PR). A router
that delegates to `family::dispatch` for every family — a **pure router** — is
the natural end state: the dispatch file becomes a table of contents, and each
family is one legible file.

## What "good" looks like (worked example)

This repo's `newt-tui` was one ~21k-line `lib.rs`. Decomposed by functional
cohesion, non-breaking, one PR per unit:

- `splash.rs`, `vi.rs`, `setup_tui.rs` — one screen / one editor mode each.
- `permissions.rs` — the whole interactive-permission subsystem, ~2k lines,
  behind a `pub(crate)` seam of a handful of items.
- `commands/{model,meta,settings,crew}.rs` — one command *family* each; the
  `dispatch_slash` mega-match went from **627 lines to a 33-line pure router**
  that just routes each family to its module.

Every cut: arm bodies moved *verbatim*, only the reached items made
`pub(crate)`, public API preserved with `pub use`, tests green throughout.
Afterward, "which code runs `/probe`?" answers itself.

## Relationship to the other doctrines

- **The three Cs** (Composition, Configuration, Convention — knowledge belongs
  in data, not logic) decides *what a unit is made of*. Functional cohesion
  decides *where units live and how they connect*. They compose: a
  cohesive, loosely-coupled module whose knowledge is pure data is the target.
- **TDD** is the partner discipline: it proves each unit and, by making
  tightly-coupled code painful to test, pulls the design toward seams. Cohesion
  is what you reach for when the corpus is too big for the tests alone to keep
  honest.

## Checklist

- [ ] Is this unit named for the **one job** it does?
- [ ] Does everything inside it earn its place by collaborating on that job
      (functional), not by being the same *kind* of thing (logical)?
- [ ] Is the `pub(crate)` / public seam as **narrow** as it can be?
- [ ] Does "which code does X?" point at **one** unit?
- [ ] If I split: did I move **verbatim** first and refactor **later**?
- [ ] Tests green, clippy clean, downstream builds — proving non-breaking?

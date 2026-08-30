# Decision: nothing is ready to extract into a Newt-owned crate (G0)

**Status:** Proposed (G0, #1933, epic #1803). **Outcome: not yet — for every
candidate.** The epic names this as a legitimate result: its own non-result
heading is *"moving coupled code into more packages is not reuse,"* and its
non-goals forbid *"publishing crates before the seams have two production
consumers."*

**Related:** `docs/decisions/agent_line_architecture.md` (the ADR this
reconciles against), `docs/findings/2026-08-newt-markup-a0-inventory.md`.

## The short version

Two independent reasons, either sufficient:

1. **No candidate has a consumer that a new crate would enable.** Every
   consumer of every candidate already compiles against `newt-core` today.
2. **The agent-line ADR points the other way.** Shared, stable functionality is
   to move toward **wyvern**, and newt's crates are to be **retired**. Minting
   `newt-markup` would create something that ADR says to unwind.

## The inventory

Production consumers only — `#[cfg(test)]` regions excluded by brace depth,
counted per consuming crate, with the owning module itself excluded.

| candidate | consumer crates | breakdown |
|---|---:|---|
| `markup::table` | 3 | newt-cli 5, newt-eval 1, newt-tui 1 |
| `markup::plain` | 2 | newt-core 3, newt-tui 2 |
| `markup::dialect` | 2 | newt-core 1, newt-web 1 |
| `markup::spans` | 2 | newt-core 1, newt-tui 1 |
| `progress` sink | 2 | newt-core 7, newt-tui 1 |
| `tty::raw_mode` | 2 | newt-tui 8, plus `tty::modal` by `super::` path |
| `markup::extension` | **1** | newt-web 6 |

`markup::extension` fails the two-consumer bar outright. The rest pass it — and
passing it is not sufficient, for the reason below.

**A caveat on the method, because it bit me twice.** The first pass cut each
file at its first `#[cfg(test)]` and under-reported: `newt-tui/src/probe.rs`
uses `markup::table` in production at `:1675`, below an *inline* `#[cfg(test)]`
at `:442`. That is the same trap #1898 hit in `rich_input.rs`. The table above
is the brace-depth count. Intra-crate uses are counted by path spelling, so
`tty::modal`'s `use super::raw_mode::…` is named rather than tallied.

## Why "two consumers" does not settle it

The bar exists to stop speculative packaging. It is necessary, not sufficient,
and here it is met by consumers that **already have access**:

- `newt-core`, `newt-tui`, `newt-cli`, `newt-eval` are members of one
  workspace. They import each other today.
- `newt-web` and `newt-mesh` are workspace-**excluded** (root `Cargo.toml:38`)
  but consume `newt-core` by path: `newt-core = { path = "../newt-core" }`.
  Same repository, same checkout.

So for every candidate, extraction into a new crate inside this repository
would move code across a package boundary that no consumer is currently
blocked by. That is the epic's stated non-result, and ponytail's rung 1 in its
sharpest form: **a crate that exists so that consumers who can already import
the code may import it differently is a package, not reuse.**

### The one consumer outside this repository imports none of it

`gilamonster-agent` consumes newt by a pinned git rev. What it actually
imports:

```
19  newt_core::agentic
 3  newt_core::agentic::transcript_lines
 2  newt_core::mcp
 1  newt_core::config
```

**Zero G0 candidates.** No `markup`, no `progress`, no `tty::raw_mode`, no
interaction protocol. A `newt-markup` crate would ship with no consumer outside
this repository at all.

`wyvern-agent` imports nothing from newt: no `newt` dependency appears in any
of its `Cargo.toml` files, which confirms the ADR's "wyvern is isolated"
against the checkout rather than against the document.

## The conflict, reported and not resolved

`agent_line_architecture.md` states, for the library-dependency graph:

> Shared, stable functionality belongs in wyvern, the minimal layer. … **A
> lower layer must never depend on a richer one.**

and, for the target state:

> newt's crates are retired in favour of it.

G0 asks which Newt modules to extract into durable crates. The ADR says that
durable shared functionality should end up in wyvern, and that Newt-owned
crates are the thing being wound down. Those are not obviously compatible:
extracting `newt-markup` now would mint a Newt-owned crate whose stated
long-term home is another project.

**This is an ownership decision across two repositories and is not resolved
here.** Three shapes are possible, and choosing among them is not this slice's
call:

1. **Extraction is premature, full stop** — revisit when wyvern is ready to own
   a markup layer. (Consistent with the inventory above, which finds no
   consumer that needs a crate today.)
2. **Extraction is Newt-owned and deliberately temporary**, with the ADR's
   retirement path applying to it like any other Newt crate. Then the crate
   needs a stated end-of-life, not just a semver policy.
3. **The functionality moves to wyvern directly**, skipping a Newt crate. That
   requires wyvern to accept ownership and would put markup below newt in the
   dependency graph — which is the direction the ADR wants, and a change to
   wyvern's scope that wyvern's maintainers decide.

Two rows in this repository's ratchets are already annotated wyvern-bound
(`lean_input`, `tidy_markdown_tables`), so the precedent for "this belongs
downward, later" exists and is recorded rather than acted on.

## What would change the answer

Stated as conditions, deliberately not as a test — there is nothing here to
assert mechanically, and a guard asserting the absence of a consumer we would
welcome would be worse than none:

- **gilamonster-agent imports one of these modules.** Then a real cross-repo
  consumer exists and the git-rev dependency becomes the thing a published
  crate would replace.
- **wyvern accepts ownership** of markup, the protocol, or the plain renderer.
  Then the work is a move downward, not an extraction sideways.
- **A third project appears** that needs one of these seams without needing
  `newt-core`.

Until one of those, the honest state is: the seams are real and were built by
this epic, the consumers are real, and none of them is separated from the code
by anything a crate would fix.

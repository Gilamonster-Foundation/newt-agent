# Decision: how newt publishes while agent-bridle depends on unreleased brush

**Status:** options costed, recommendation made — awaiting operator pick
**Date:** 2026-06-12
**Owner:** Claude (steward); operator decides
**Refs:** Gilamonster-Foundation/agent-bridle#20, reubeno/brush#1184,
the `[patch.crates-io]` block in the top-level `Cargo.toml`, issue #297
(`--disable-ocap`, the usability stopgap that is independent of this decision).

## The problem in one paragraph

agent-bridle's capability-confined shell (`agent-bridle-tool-shell`) needs a
hook — `CommandInterceptor` — that does not yet exist in a released `brush`.
The upstream PR is **reubeno/brush#1184 (open)**. Until it merges and brush
cuts a release carrying the hook, agent-bridle ships a **stub** shell tool
(invoke() returns a clear error) and newt patches in the real, brush-backed
shell from agent-bridle's `feat/stub-shell` branch via `[patch.crates-io]`.
That patch is a **git dependency**, and **crates.io forbids publishing any
crate that has a git (or unpublished-path) dependency** anywhere in its tree.

So today: newt builds and runs fine from source (the binary/PyO3-wheel path
is unaffected), but the three crates that transitively pull agent-bridle —
**`newt-core`, `newt-tui`, `newt-mcp-server`** — cannot be `cargo publish`ed.
This is the v0.6.x publishing block.

## What a git submodule does and does not change (the operator's question)

> *Can we make brush a submodule inside agent-bridle and compile our binary
> with brush inside it, to sidestep the publishing block?*

A submodule is a **source-tree** mechanism: it vendors files into a checkout.
It has **no effect on the dependency edge Cargo records**. If
`agent-bridle-tool-shell` depends on `brush = { path = "vendor/brush" }` (the
submodule path), `cargo publish` of agent-bridle **still fails** — crates.io
rejects path dependencies on unpublished crates exactly as it rejects git
dependencies. The submodule changes *where the brush files live*, not *whether
agent-bridle has an external dependency on them*.

A submodule only helps if paired with a decision to **not publish** the
brush-dependent crates at all, or to **vendor brush's source as first-party
modules** (Option B below) so there is no `brush` dependency edge to reject.
The submodule itself is an implementation detail of Option B, not a fourth
option.

## Options

### Option A — publish only the clean leaf crates; ship the binary from source
Publish the crates that do **not** pull agent-bridle (`plugins-protocol`,
`newt-identity`, `newt-tools`, `newt-skills`, `newt-inference`, `newt-coder`,
`newt-acp-worker`, `newt-cli`/`newt-agent` as a binary). Do **not** publish
`newt-core`, `newt-tui`, `newt-mcp-server` to crates.io while the patch
stands; they remain source/binary-distributed (which is already how `newt`
reaches users — release binaries + the PyO3 wheel).

- **Cost:** low. Adjust the release matrix to skip the three blocked crates;
  document why. Reversible the day brush#1184 lands.
- **Block removed:** partial — leaf crates publish; the three core crates do
  not get a crates.io presence yet.
- **Fork burden:** none.
- **Risk:** `newt-core` is the natural "library" entry point; not publishing it
  means no `cargo add newt-core` story until #1184. Acceptable if newt's
  distribution is binary/wheel-first (it is).

### Option B — vendor brush's source into agent-bridle as MIT modules
Copy the brush crates the shell tool needs into agent-bridle under a vendored
path (submodule or a literal source copy) with brush's MIT `LICENSE` + a
`NOTICE`. agent-bridle then has **no external brush dependency** and becomes
crates.io-publishable; newt drops `[patch.crates-io]` and depends on the
published agent-bridle; all three core crates publish.

- **Cost:** high, and ongoing. You now **own a brush fork**: track upstream,
  carry the CommandInterceptor patch yourself, re-vendor on brush updates.
- **Block removed:** full.
- **Fork burden:** yes — the maintenance is permanent until you de-vendor back
  onto released brush (which is Option C arriving anyway).
- **License:** clean — brush is MIT; vendoring with attribution is permitted.
- **Risk:** you maintain a shell-language fork to ship a coding agent. The
  CommandInterceptor hook is exactly what #1184 upstreams, so this is
  duplicate work with a built-in expiry.

### Option C — wait for reubeno/brush#1184 to land upstream
When #1184 merges and brush releases with the hook: agent-bridle drops its
stub, depends on the **published** brush, publishes itself, and newt deletes
the entire `[patch.crates-io]` block. No fork, no vendoring, no partial
publish.

- **Cost:** zero engineering; the wait is external.
- **Block removed:** full, cleanly.
- **Fork burden:** none.
- **Risk:** timeline is not ours to control. Mitigated because the
  **usability** pain (ocap fails closed on stub builds) is *already* solved by
  `--disable-ocap`/`--yolo` (#297) — publishing is the only thing gated on
  #1184, and newt ships fine from source meanwhile.

## Recommendation

**C now, A if publishing pressure arrives before #1184 lands.**

The thing that actually hurt — agentic coding being unusable on stub builds —
is fixed by #297 independently of this decision. Publishing to crates.io is a
*distribution nicety* for a tool that already ships as a binary and a wheel;
it is not on any critical path. So the cheapest correct move is to wait for
#1184 (Option C) and delete the patch wholesale when it lands.

Option A is the pressure-relief valve: if a concrete consumer needs
`cargo add`-able leaf crates before #1184, do A — it's a release-matrix edit,
fully reversible, no fork.

Option B (the submodule/vendor path) is **not recommended** unless an external
consumer specifically needs a published, brush-backed `agent-bridle` before
#1184 — it buys a permanent fork to win a few weeks against an upstream PR
that already exists. If that consumer materializes, revisit; otherwise the
fork is pure cost.

## What to watch

- **reubeno/brush#1184** — the unblock. When merged + released, execute C.
- If a downstream asks to `cargo add` a newt crate before then, execute A.
- `--disable-ocap` (#297) covers the usability gap in the interim; it is
  doc-marked to be removed when the real shell becomes the default everywhere.

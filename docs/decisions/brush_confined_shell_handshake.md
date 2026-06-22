# Brush confined-shell: crates.io capability handshake

**Status:** planned (waiting on brush ≥ 0.5.0 upstream). Tracking: #570.
**Refs:** agent-bridle#28 · brush crate (https://crates.io/crates/brush) ·
`just shell-real` / `just shell-stub` / `just shell-check` (justfile).

## Problem

newt's confined `run_command` shell is the real **brush** OCAP shell. It lives
on agent-bridle `main`, which (today) pulls a **brush git fork**. crates.io
**forbids git dependencies in any form — even optional / feature-gated** — so:

- The published newt build must use agent-bridle's crates.io-safe **stub** shell
  (the `feat/step-up-decision-mvp` branch — no brush git dep).
- The real shell is reachable only via a **dev-only** git override
  (`just shell-real`, which sed-swaps the `[patch.crates-io]` pin to agent-bridle
  `main`), guarded by `just shell-check` so the unpublishable pin never reaches
  `main`.

This branch-swap is *version/source thinking*: "do I have the confined shell?"
is answered by "which branch am I pinned to." It's brittle, needs a guard, and
doesn't scale across the Gilamonster agent line.

**What changed:** brush is now **on crates.io** (max `0.4.0` as of 2026-06-22).
The confined-shell API we need is expected in a future release (optimistically
**0.5.0**, pending the upstream brush PR). Once that lands, the whole git-fork
constraint dissolves and we can do this the right way.

## Decision

1. **Now:** keep the published newt on the **stub** shell. `just shell-real`
   stays the dev-only confined-shell build. No change to `shell-check`.
2. **Watch crates.io** for the brush release that carries the confined-shell API
   (target `>= 0.5.0`) — see `.github/workflows/brush-watch.yml`.
3. **When it publishes:** switch agent-bridle to depend on the **crates.io**
   brush and detect the capability with a **build-script handshake** (below), so
   the confined shell **auto-enables** — by capability, not by version pin — at
   the next `cargo update`. Retire `shell-real` / `shell-stub` / `shell-check`.

## The capability handshake (how auto-on works)

Cargo can't add a dependency to an *already-published* crate version, and it has
no stable "compile this if symbol X exists" (`cfg(accessible)` / `cfg(version)`
are nightly-only). The stable, idiomatic mechanism is a **`links` build-script
metadata handshake**:

1. **brush** (the published crate) declares `links = "brush"` and its build
   script emits `cargo::metadata=confined_shell=1` **when the confined-shell API
   is present**. (If brush won't advertise this, agent-bridle falls back to a
   version probe: `build.rs` reads the resolved brush version and treats
   `>= 0.5.0` as "capable".)
2. **agent-bridle** depends on `brush = ">=0.5"` (floating) and, in its
   `build.rs`, reads `DEP_BRUSH_CONFINED_SHELL`; when set it emits
   `cargo::rustc-cfg=confined_shell`. The real shell is `#[cfg(confined_shell)]`,
   the stub is `#[cfg(not(confined_shell))]`. Both compile from the **same
   published manifest** — default builds stay publishable.
3. **newt** just tracks agent-bridle from crates.io. `cargo update` pulls the
   capable brush → agent-bridle's build script flips the `cfg` → the confined
   shell compiles in. **No feature flag to flip, no code edit, no branch swap.**

> Prefer an explicit opt-in instead of full-auto? Gate the same handshake behind
> an optional `brush` cargo feature (`features = ["brush"]`) — identical
> detection, but you choose when to turn it on. Auto-on vs. opt-in is a policy
> knob on top of the same mechanism.

### Why this over the alternatives

- **vs. repoint (`shell-real` committed):** that's a source swap, unpublishable,
  needs the `shell-check` guard, and is version-thinking. The handshake is
  capability-thinking, publishable by default, and deletes the guard machinery.
- **vs. a bare cargo feature:** a feature flag still needs a human to flip it
  when brush lands. The `links` handshake flips itself on detection.
- **Shared truth:** both the old and new approaches are blocked until brush
  ships the API to crates.io. The handshake doesn't unblock publishing early —
  it makes the turn-on **automatic and honest** once the gate clears.

## Detecting the release

`.github/workflows/brush-watch.yml` runs on a schedule, queries the crates.io
API for brush's `max_version`, and opens a tracking issue once it reaches the
target (default `0.5.0`, overridable via `workflow_dispatch`). Set the target to
the exact release that carries the upstream patch once that version is known.
(Dependabot/Renovate can supplement once newt declares the brush dep directly.)

## Sequence / checklist

- [ ] **(done)** newt published on the stub; `shell-real` is the dev confined shell.
- [ ] **(this PR)** `brush-watch` workflow + this design note.
- [ ] Land the upstream brush PR; note the version that carries the API (≈0.5.0).
- [ ] Point `brush-watch` at that version; wait for the watcher / crates.io.
- [ ] **On publish:** agent-bridle depends on crates.io `brush = ">=X"` + the
      `links` (or version-probe) `build.rs` handshake; `#[cfg(confined_shell)]`
      real shell, stub otherwise. (agent-bridle#28.)
- [ ] Bump newt to the agent-bridle release that carries the handshake; verify a
      clean `cargo update` auto-enables the confined shell.
- [ ] Retire `just shell-real` / `shell-stub` / `shell-check` and the
      `[patch.crates-io]` block.

## Constraints to remember

- **A published crate's dependency graph is frozen.** An already-released newt
  can't grow brush later; auto-on happens across a `cargo update` of a newt
  release that already declares the floating brush dep. So end users get the
  confined shell in the *next* newt release after brush's API publishes.
- **`links` metadata only reaches direct dependents** — brush → agent-bridle.
  That's why the `cfg` is computed in agent-bridle (which owns the integration),
  not in newt.

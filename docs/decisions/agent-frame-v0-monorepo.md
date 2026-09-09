# agent-frame v0 is incubated in this repository

**Status:** accepted, 2026-09-08 · **Supersedes:** the separate-repo plan in the
private `Gilamonster-Foundation/agent-frame`

## Decision

`agent-frame` v0 lives here as a workspace member (`agent-frame/`), with its
machine-checked core at `formal/` and a CI job that builds it. It graduates to
its own public repository and crates.io at **0.1.0**.

## Why not the separate repo

The separate repo could not ship, for a reason no engineering step there could
clear. Its `docs/EMBARGO.md` binds *publication*, not merely text:

> This repository does not become public, and no crate built from it publishes
> to any registry, before the prior work's own disclosure.

That disclosure waits on an external party. A visibility flip, a text scrub, and
a history rewrite are all equally blocked by it, because the binding is on the
act of publishing rather than on any particular file. The repo also has **zero
lines of Rust** — there was no crate to release as 0.0.1 even if the gate were
open.

**The code was never what was embargoed.** The separate repo's own decision
record settles it:

> **Can agent-frame be built without the embargoed material? — Yes, completely.**
> What v0 loses: a footnote, and a private map of potholes someone already
> walked. No theorem, no invariant, no API decision, no priority claim.

So the kernel is written here, fresh, from the design record. What stayed behind
is prose — the embargo document itself, an architecture paragraph naming the
prior work's domain, and a history that is permanently reachable from that
repo's main. None of it is load-bearing for the crate, and none of it came
across. `formal/` was scanned for the six forbidden categories before the copy
and is clean.

The private repository is **archived, not deleted.** It holds the invariant
ledger, the embargo audit, the v0 decision record and the non-vacuity check —
the reasoning behind this crate. Deleting it would buy tidiness at the cost of
the record we will want when clearance lands.

## What v0 is

A library. No daemon, no socket, no async, no wire format, no storage.

**v0 mints elision only.** `Op::Concise` and `Op::Generate` exist in the type —
the tables mirror the Lean exhaustively, with no wildcard arm — but `Unit::seal`
refuses them. The referent is `checkOf .elide = .rederive`: elision asserts
nothing and is verified by re-derivation, so every v0 assertion is a
deterministic recompute. No model, no corpus, no grounding threshold, no flake.

The refusal is principled rather than not-implemented-yet. `checkOf .concise =
.ground` leaves concision inside the fence with a verification class that has no
implementation and no calibrated threshold; a mintable unit whose check nobody
can run reproduces *"unverified evidence is indistinguishable from no evidence"*
inside the product.

## Consequences

**It is a LEAF, and that is enforced.** `agent-frame/tests/no_service_edge.rs`
asserts no HTTP or inference dependency edge (invariant 7.4's static half,
landed day one so the crate stays daemon-able for free) and pins the closure at
a measured ceiling of 40. Living in this workspace must never give it
`newt-core`'s 644-crate closure. The test carries an anti-vacuous twin: the same
walker, pointed at `newt-core`, must come back finding `tokio`.

**Correction — the nine existing libraries were already gated.** An earlier
draft of this ADR claimed `formal/` held nine machine-checked libraries that no
CI job built. **That was wrong.** `.github/workflows/formal.yml` has been
running `lake build` over them, and `behavior-formal.yml` runs Lean plus a TLC
harness. Both are **path-filtered** to `formal/**`, which is why they do not
appear on an ordinary Rust PR — "built only when they change" is not the same
as "built by nothing", and the difference is the whole claim.

The path filter is the right design, not a gap: `lake build` checks the Lean,
while the Lean↔Rust correspondence is checked by
`agent-frame/tests/kernel_laws.rs`, which runs in the ordinary `test` job on
every push.

So this change adds **no new Lean job**. It adds the two agent-frame libraries
to the existing lakefile, and one anti-vacuous step to `formal.yml`: `lake
build` on a mis-configured package exits 0 having checked nothing, so the job
now requires an `.olean` for every library the lakefile declares — the list
derived from the lakefile rather than duplicated in the workflow, since a
hand-copied list is the next thing to go stale.

That step is not hypothetical. Copying agent-frame's `formal/` in initially
**replaced** this repo's lakefile and silently dropped all nine libraries from
the build, and nothing in CI would have caught it.

**The correspondence is checked on both sides.** `formal/` proves the laws;
`agent-frame/tests/kernel_laws.rs` asserts the Rust obeys the same ones, naming
the Lean theorem per test. Either half alone is a claim. The `formal` CI job
makes the Lean half real — a `formal/` folder nobody builds is decoration.

**It breaks the stack's shape, deliberately and temporarily.** `agent-bridle`,
`agent-mesh` and `agent-store` are separate public repos on crates.io.
`agent-frame` is the odd one out until 0.1.0. Read this as an incubator, not as
a new convention.

**Extraction later is cheap and already specified.** Promoting a workspace
member to its own crate is the pattern in
`knowledge/board/newt-agent/2026-09-08_crates-into-parts-PLAN.md`, and the
closure ledger (#2233) is the guard that keeps this crate extractable while it
lives here.

## Alternatives rejected

**Flip the private repo public.** Blocked by the embargo binding, which is on
publication and not on text — a scrub cannot unblock it, and that is by design:
*"a scrub is the chore that gets skipped."*

**Rewrite its history and re-release.** Same block, plus it destroys the record.
Force-pushing away the decision documents to publish a crate that does not exist
yet trades the reasoning for nothing.

**Wait for clearance.** The clearance is external and has been pending for most
of a year. The kernel does not need it, so waiting spends time to buy a footnote.

---

## Appendix — how the `formal` job nearly did not land

The first attempt to push this job was rejected:

```
refusing to allow an OAuth App to create or update workflow
`.github/workflows/ci.yml` without `workflow` scope
```

The cause was not the credential's scopes. This repository's `.git/config`
carried

```
[url "https://github.com/"]
	pushInsteadOf = git@github.com:
```

which silently rewrote every **push** from SSH to HTTPS, onto an OAuth token
without `workflow` scope, while fetches continued over SSH. The same section
sat next to a `[user]` block setting `codex@openai.com` — both left behind by an
earlier agent session, and both inherited by every worktree of this repo.

Removed on 2026-09-08. Pushes go over SSH, where the workflow-scope restriction
does not apply, and identity falls back to the global
`hartsock@users.noreply.github.com`. `gilamonster-agent` carried the same
rewrite and was corrected with it.

Worth recording because the failure mode is silent and misattributes its own
cause: the error names a missing OAuth scope, and the real defect is a local
config rewriting the transport.

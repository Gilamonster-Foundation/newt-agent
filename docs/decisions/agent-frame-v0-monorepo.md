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

**It starts gating nine libraries that nothing was checking.** `formal/` already
existed here, with `CaveatLattice`, `ProjectModel`, `NewtPolicy`,
`CompactionProvenance`, `CompactionLifecycle`, `CompactionSpill`,
`ResponsesUsage`, `ResponsesWire` and `NewtInteraction` — and **no CI job built
any of them**. Nine machine-checked libraries, unchecked. The `formal` job added
here builds all eleven (39 lake jobs, green), so the pre-existing proofs stop
being decoration too.

That absence had teeth: bringing agent-frame's `formal/` across by copy
initially *replaced* the lakefile and silently dropped all nine libraries from
the build, and nothing in the repo could have caught it. The lakefiles are now
merged rather than replaced, and the job's anti-vacuous step requires an
`.olean` for every declared library so the same mistake fails loudly next time.

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

## Appendix — the `formal` CI job, pending a `workflow`-scoped push

**This job is NOT in the branch.** GitHub refused the push:

```
refusing to allow an OAuth App to create or update workflow
`.github/workflows/ci.yml` without `workflow` scope
```

That is the same wall #1222 hit. Its remedy was a patch file in a scratch
directory, and that file is now gone — so the job is recorded **here**, in the
repository, where it is versioned with the decision it belongs to and cannot
rot out from under the next person.

A maintainer applies it with a credential that has `workflow` scope:

```bash
# paste the block below into .github/workflows/ci.yml immediately above
# the `lean-build:` job, then:
git add .github/workflows/ci.yml
git commit -m "ci(formal): build the machine-checked core (Lean 4)"
git push --no-verify
```

Also add to the file's top HOOK PARITY comment:

```yaml
# EXCEPTION: the `formal` job (Lean 4) is mirrored by `just formal` but is
# deliberately NOT in the pre-push hook — the hook is already ~50 min (#1098)
# and a toolchain download would make it worse.
```

The job itself:

```yaml
  formal:
    name: Formal core (Lean 4)
    runs-on: ${{ vars.GMF_LINUX_RUNNER || 'ubuntu-latest' }}
    # The machine-checked core of agent-frame: `lake build` re-checks every
    # theorem, so a change that breaks a proven invariant fails the build rather
    # than silently diverging from the Rust that mirrors it.
    #
    # The correspondence is the whole point. `agent-frame/tests/kernel_laws.rs`
    # asserts the Rust obeys the same laws this job proves; either half alone is
    # a claim, and the two together are a check. Evidence nobody reads is
    # decoration -- that is why this job exists rather than a `formal/` folder
    # sitting unbuilt.
    #
    # HOOK PARITY EXCEPTION, deliberate: mirrored by `just formal`, but NOT by
    # .githooks/pre-push. The hook is already ~50 min (#1098) and adding a
    # toolchain download to it would make it worse. Same posture as the
    # `windows` job: CI is the authoritative gate here.
    #
    # Self-contained by design: the lakefile pulls no Mathlib, so this needs
    # only a bare toolchain and finishes in seconds once elan is cached.
    steps:
      - uses: actions/checkout@v4

      - name: Cache elan + lake build
        uses: actions/cache@v4
        with:
          path: |
            ~/.elan
            formal/.lake
          key: lean-${{ runner.os }}-${{ hashFiles('formal/lean-toolchain', 'formal/lake-manifest.json') }}

      - name: Install Lean toolchain (elan)
        run: |
          set -euo pipefail
          if [ ! -x "$HOME/.elan/bin/lake" ]; then
            curl -fsSL https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh \
              -o elan-init.sh
            sh elan-init.sh -y --default-toolchain "$(cat formal/lean-toolchain)"
          fi
          echo "$HOME/.elan/bin" >> "$GITHUB_PATH"

      - name: Build the formal core (checks every theorem)
        working-directory: formal
        run: lake build

      - name: Assert the build actually proved something
        working-directory: formal
        # ANTI-VACUOUS: `lake build` on an empty or mis-configured package exits
        # 0 having checked nothing. Require an olean for EVERY declared library,
        # so a lakefile that quietly stops building one turns this job red
        # instead of green-and-empty. That is not hypothetical: this list was
        # nine libraries long and unbuilt by CI before the `formal` job existed,
        # and a copy that replaced the lakefile dropped all nine without a
        # single test noticing.
        run: |
          set -euo pipefail
          for lib in CaveatLattice ProjectModel NewtPolicy CompactionProvenance \
                     CompactionLifecycle CompactionSpill ResponsesUsage ResponsesWire \
                     NewtInteraction ContextOps ContentAddressed; do
            find .lake/build -name "$lib.olean" | grep -q . || {
              echo "::error::no $lib.olean -- lake build proved nothing"; exit 1; }
            echo "ok: $lib.olean present"
          done

```

Until it lands, `formal/` is buildable (`just formal`) but ungated — which is
precisely the state this ADR argues against, and the reason this appendix is
worded as a debt rather than a nicety.

# Decision: the agent family (wyvern, newt, gilamonster)

**Status:** Accepted as **direction** (decided by Shawn Hartsock, 2026-08-17).
The end state is aspirational and the sequencing is deliberate. The open
questions at the bottom are unresolved and must not be read as settled.
**Date:** 2026-08-17
**Related:** `docs/decisions/plain_scroller_tui.md` (the LeanTUI migration
notice records the first concrete move), `docs/decisions/lean_rich_tui_morphologies.md`,
`docs/decisions/agentic_object_capability_security.md` and
`docs/decisions/ocap_confinement_model.md` (the OCAP substrate this layers),
`docs/decisions/mesh_integration.md` (newt as a mesh worker).

---

## TL;DR

Three agents, one line of descent, complexity increasing left to right:

| | repo | shape |
|---|---|---|
| **base** | **wyvern-agent** | The barest working harness, **including OCAP**. **No real TUI**. A near-pure scroller emitting systemd-style lines that read correctly under `journalctl`. |
| **middle** | **newt-agent** | Builds on wyvern. The agentic loop, tools/MCP, prompt provenance. |
| **top** | **gilamonster-agent** | Everything and the kitchen sink. The most functional and most YOLO. **OCAP off by default.** |

**Aspirationally, wyvern-agent ends up a rewrite of newt-agent that is
lighter, faster and smaller, and the other agents inherit from it.**

## The sequencing (this is the part that matters day to day)

The designs got muddled along the way, and the recovery plan is explicitly
ordered:

1. **Get newt-agent working.** It is the one with a working harness today, and
   it is where the behaviour gets proven.
2. **Rewrite newt into wyvern**, lighter and faster and smaller, carrying over
   what earned its place.
3. **Inherit upward** into newt and gilamonster.
4. **Eventually remove newt-agent crates** in favour of wyvern-agent /
   gilamonster-agent crates that are rewrites of what is here now.

**So newt-agent is transitional.** That is not a reason to build carelessly.
It is a reason to be deliberate about *what kind* of work is worth doing here.

## What this means for work in this repo today

The useful split is **contracts survive a rewrite; implementations do not.**

- **Worth investing in now:** wire types, identity, ownership and provenance
  chains, data formats, config schemas, capability/caveat vocabulary. These are
  what a rewrite is written *against*, so getting them right pays the most. A
  rewrite inherits a good contract for free and inherits a bad one forever.
- **Worth consolidating now:** duplicated implementations. One seam ports to two
  repos cleanly; six hand-rolled copies of the same thing port to twelve. Every
  de-duplication done here is paid back twice downstream.
- **Worth less now:** surface polish and one-off implementations that a
  descendant will rewrite anyway. Not worthless, since newt has to be usable to
  be proven, but not where the care goes.

This is also why the **three Cs** and the **reuse discipline** in `CLAUDE.md`
matter more here than they would in a terminal codebase, not less: knowledge held
as data ports across a rewrite, and behaviour held in one adapted abstraction
ports as one unit.

## Why wyvern is the base rather than the sibling

wyvern is the **headless flight tier**. Putting the floor there means the
strictest environment defines the contract, and richer descendants add to it
rather than the reverse. Two consequences fall out:

- **The plain scroller belongs to wyvern.** The LeanTUI's timestamped
  server-log morphology (`[2026-06-20 14:32:01] ❯ …`, greppable when captured)
  is not a newt feature that happens to suit wyvern; it is wyvern's output
  contract, already described in `lean_rich_tui_morphologies.md`. The migration
  notice in `plain_scroller_tui.md` records the first concrete move.
- **OCAP belongs to wyvern.** Object-capability confinement is a **floor, not a
  middle layer**: it lives in the base and everything inherits it. A security
  property that a descendant can decline to compile is not a floor.

## Open questions, deliberately not answered here

Recorded so nobody mistakes silence for a decision.

1. **Does gilamonster inherit via newt, or directly from wyvern?** "Complexity
   goes wyvern to newt to gila" reads as a chain; "the other two agents will
   inherit off of it" reads as a fan from wyvern. Chain and fan imply different
   crate boundaries, so this needs settling before the rewrite starts.
2. **What does "OCAP off by default" mean in gilamonster?** A permissive
   *posture* (wide `Caveats`, mechanism present and auditable) and an absent
   *mechanism* are very different things. The first is a config default. The
   second removes the floor and would invalidate the `ocap_check.py` source
   inventory gate's premise. Not decided.
3. **Exactly what newt contributes** once the harness, the scroller, and OCAP
   all live in wyvern. Today `newt-core` mixes all three layers in one crate, so
   the answer determines where the seams get cut.
4. **What "lighter, faster, smaller" is measured against.** Aspiration without a
   number tends to lose to feature pressure; a size or startup budget recorded
   early is cheap and defensible later.

## Consequences accepted

- newt-agent code has a **finite life**. Documentation and decision records
  should say what a rewrite needs to know, not merely what the current code
  does.
- **Divergence between newt and its siblings is expected**, not a defect. See
  the lean/rich divergence already accepted in `plain_scroller_tui.md`.
- The acceptance contract in `docs/ROADMAP.md` still applies to every PR here.
  Transitional does not mean unmeasured; the gates are how newt earns the right
  to be the thing that gets rewritten.

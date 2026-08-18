# Decision: `/mode` Dispatch Topology (single · crew · mesh · remote) + plan

**Status:** Superseded as a command-surface proposal
**Date:** 2026-06-21
**Tracking issue:** Gilamonster-Foundation/newt-agent#554

> **2026-07-24 terminology decision:** `/mode` now selects an operating
> behavior and `/posture` owns permission floors. Dispatch topology remains a
> valid separate design axis, but it must receive a distinct control instead of
> overloading either command. See
> [`operating_modes_and_permission_postures.md`](./operating_modes_and_permission_postures.md).


---

**Condensed 2026-08-18.** The proposal body was retired: `/mode` and `/posture`
were split by the 2026-07-24 terminology decision above, so the command-surface
design this document argued for cannot be built as written.

What survives is the axis itself. Dispatch topology (single, crew, mesh,
remote) is still a real design dimension and still needs a control of its own
rather than overloading `/mode` or `/posture`. Whoever picks that up should
start from the terminology decision, not from here.

Retained because `docs/research/crew-vs-dynamic-workflows.md` cites this path.
The full proposal is in git history before this commit.

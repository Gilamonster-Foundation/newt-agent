# Crew vs. dynamic workflows: runner-agnostic orchestration + the floor/ceiling rule

**Status:** research / positioning note (Shawn Hartsock, 2026-06-22)

**Related (external):** [`2605.18747-code-as-agent-harness.md`](./2605.18747-code-as-agent-harness.md)
— the arXiv "Code as Agent Harness" survey (executable / verifiable / stateful
agent systems), the academic frame for the pattern below.

**Related (ours):** [`../design/workflow-swarm-harness.md`](../design/workflow-swarm-harness.md)
(the swarm-harness *design* this positions), [`../design/crew-swarm-overseer.md`](../design/crew-swarm-overseer.md)
(the overseer stack), [`../decisions/mode_dispatch_topology.md`](../decisions/mode_dispatch_topology.md)
+ #554 (the topology axis that swaps the runners), ROADMAP **Phase 22** (the
scheduler the ceiling needs) + **Phase 23** (Crew/Team/Overseer); #488
(MeshCrewRunner), #472 (BOOT — persistent identity); the `CrewRunner` trait +
`LocalCrewRunner`; the agent-store / §6 causal store.

## The observation

Anthropic's **dynamic workflows** — a primary agent writes a JS harness that fans
out fresh-context sub-agents, runs verification gates, and returns a *verified*
result (the pattern surveyed in [2605.18747](./2605.18747-code-as-agent-harness.md))
— and our **Crew** concept are **the same core pattern**: a *deterministic
harness* orchestrates *bounded-context leaf agents*, with *verification*, where
**the orchestration is code, not a model deciding what to do next.** We arrived at
it independently.

**The overseer is not a separate component — it is the newt-agent itself: the
interactive TUI session and its primary agent context.** That primary context
plays exactly the role Claude Code's main loop plays when it fires a workflow: it
plans, composes a roster, dispatches crews per plan-step, reviews the returned
diff, reports — the human approves the plan and the roster
(`crew-swarm-overseer.md`). So: *overseer = newt-agent-with-TUI*; *crew member = a
leaf agent*; *harness = the deterministic plan the overseer runs.*

## The divergence — where the cost lives

Dynamic workflows are cheap *because* they refuse two things our Airship crew
deliberately buys back. Each flip has a tax.

### Ephemeral → persistent (stateless agents → stateful agents)

- **Workflows:** the *agent* is a pure `task → result`, spun fresh, returns,
  vanishes; all durability lives in the *script*. That is why it can be a small
  file — no memory, no identity, no contamination; a dead agent is a `null` you
  filter.
- **Crew (persistent):** the *agent* holds durable state — a resident expert that
  knows the project. More powerful, and unobtainable from a fan-out — but now you
  own context drift, causal ordering, and recovery of half-done work.
- **Tax:** a durable causal store (agent-store / §6) + compression so a long-lived
  member's context doesn't rot (the Phase 24 / #559 work, now landed).

### In-process → distributed (one event loop → a mesh protocol)

- **Workflows:** one `await`-driven loop — shared concurrency cap, abort, token
  budget; a single process.
- **Crew (Airship):** members run across machines/pods; orchestration is
  CrewTask/CrewResult over agent-mesh. Buys **scale** (the GPU fleet, not one
  box), **resilience** (a parked crew outlives the overseer's session),
  **locality** (resident where the data/GPU is).
- **Tax:** OCAP across hops (caveats attenuate, never widen — `run_crew`/#494); a
  persistent identity root (BOOT / #472 — a script needs no identity, a parked
  agent does); admission/time-sharing of resident agents on finite GPUs
  (**Phase 22** — the ephemeral pattern never needs a scheduler because the
  process *is* one; a fleet of always-on members absolutely does).

## The seam that makes this one design, not two

`CrewRunner` is `task → result` regardless of where it runs. `LocalCrewRunner`
(worktree, in-process) **is** the dynamic-workflow analog; `MeshCrewRunner` (#488)
and `RemoteCrewRunner` are the persistent-distributed ones. **`/mode`
dispatch-topology** ([`mode_dispatch_topology.md`](../decisions/mode_dispatch_topology.md),
#554) is precisely the axis that swaps them: `crew` = the local fan-out,
`mesh`/`remote` = the Airship residents. So the move is **not** "rebuild Crew as
workflows" — it is: *write the orchestration once, runner-agnostic, and let the
topology axis choose ephemeral-in-process vs. persistent-distributed per task.*

## The rule

**The dynamic-workflow pattern is the cheap floor; persistent-distributed crew is
the expensive ceiling — make each task earn the tier it asks for.**

- **Floor** (ephemeral, in-process, no scheduler, no BOOT): bounded,
  fan-out-shaped work — review, synthesis, decompose-and-cover, adversarial
  verify. `LocalCrewRunner` / `/mode crew`.
- **Ceiling** (persistent, distributed): reserve for genuine **residency** (a
  project-parked reviewer with accumulated context), **locality** (run where the
  GPU/data is), or **duration** (a migration that outlives a session).
  `MeshCrewRunner` / `RemoteCrewRunner` / `/mode mesh|remote`.

Don't pay BOOT + agent-store + Phase-22 scheduling for something that fits in a
single-process `parallel()`; conversely, don't cram a genuinely-resident expert
into an ephemeral fan-out that forgets everything between sessions.

The survey's line is the same point from the other side — *"context management is
the tax of implicit shared state"* (2605.18747 §4.3): the ephemeral fan-out avoids
that tax by having **no** shared state; the persistent crew pays it **on purpose**,
for what shared state buys.

## For the Phase 22 build

Phase 22 (the hierarchical context scheduler) is *why the ceiling is affordable*:
it time-shares resident crew on a finite GPU fleet — the floor never needs it. As
we develop Phase 22, cross-link `/mode` (#554) here as the topology selector, and
keep the orchestration logic **runner-agnostic**, so a plan written for the local
floor lifts to the distributed ceiling unchanged.

# Vision — Confined agents as a conserved resource

> "Computer Science has as much to do with Computers as Astronomy does with
> Telescopes." — Dijkstra

This document explains *what we are building* and *why*, so that individual
issues, ADRs, and PRs can be read against a shared intent. It is deliberately a
statement of direction, not a spec.

## The principle: cognition is now a priced resource

Every token has a price. For the first time in the history of thinking machines,
the **cost of cognitive load is literally quantified** — you can put a number on
"how much thought did this take." That turns a vague design instinct into an
engineering constraint you can budget against.

The instinct has a name here: **altruistic laziness**. Push every unit of work
to the *cheapest layer that can actually do it*, and spend the expensive layer
*only* on work that nothing cheaper can. This is not new — it is the
conservation-of-energy law applied to cognition, the same move as caching,
tiered storage, and this repo's own testing rule ("use the cheapest test tier
that proves the behavior"). What is new is that the meter is running in dollars,
so we can finally *see* the waste.

The design goal that falls out of this is **controlling expense and
complexity**: keep the expensive, scarce, hard-to-decompose reasoning rare, and
make everything around it cheap, bounded, and replaceable.

## What we are building

A **lightweight, self-contained agent that runs confined inside agent-bridle**,
and that a larger model can **invoke as a tool**.

The large model delegates work that is *long-running, watchable, or tedious but
not trivially decomposable* — the work that would otherwise burn expensive
reasoning tokens on nothing but patience:

- watch a CI pipeline and act when it turns green;
- push a branch and open a pull request;
- babysit a deploy; poll a queue; tail a long job.

The confined agent does that work under a small, cheap model, inside a kernel
jail, and reports back. The expensive model spends its budget on the reasoning
that *cannot* be handed down.

We already do a crude version of this by hand: running `gh pr checks --watch` in
the background so the expensive model is not staring at CI. Formalizing that —
into an invokable, confined, cheap agent with its own bounded authority — is the
real thing.

### The mechanism is already a tool call

Crucially, this is **not a new primitive**. Inside the harness, *calling a
sub-agent is just another tool call* — and "invoke a confined lightweight newt as
a worker" is exactly that: a tool call. So there is one delegation primitive, not
two:

> **sub-agent = tool call = a bounded delegation.**

Every tool call — and therefore every sub-agent — *carries an authority bundle*
(see Modes below). Delegating to a confined cheap worker means making a tool call
whose bundle is *autonomous authority + confined engine + no human gate + leaf*.
The confinement and the caveats travel **with the call**; they are not a separate
sandbox the worker opts into.

This collapses three things into one: watching a pipeline, spawning a sub-agent,
and running a shell command are all "hand a bounded slice of my authority to
something cheaper and let it act." The only questions are *how much* authority and
*inside what box* — which is precisely what the caveat lattice and the mode bundle
answer.

## Why confinement is the enabler, not a side quest

You can only safely delegate to something you can **bound**. A background agent
with ambient authority is a confused deputy waiting to happen. So the ability to
hand a cheap agent a *small, explicit, unforgeable slice of authority* — and know
the kernel enforces it — is what makes delegation safe enough to be worth doing.

That is what agent-bridle is: the jail. The object-capability model (least
authority, `meet`-only attenuation, fail-closed) is what lets us say "this worker
may read *these* paths, run *these* commands, reach *these* hosts, and nothing
else" and mean it. The confinement work (the sandboxed-host shell engine, the
per-axis kernel enforcement, the net-host surfacing) is not adjacent to the
agent-tool goal — it *is* the container the tool runs in.

## Modes: coherent bundles, not free-form flags

Here is the sharp edge. The system has several axes, and **not all combinations
are coherent** — some are mutually exclusive by construction:

- **Authority source:** interactive prompt (a human answers) — *or* pre-granted
  caveats/policy (autonomous) — *or* unbridled.
- **Confinement engine:** safe-subset (refuses dynamic shell) — *or*
  sandboxed-host (`/bin/sh -c` inside the kernel jail, fs fenced, exec/net
  advisory) — *or* full-bash / yolo.
- **Human gate:** supervised-free (a human gesture still gates high-consequence
  acts) — *or* autonomous (no human in the loop).
- **Topology:** single — *or* crew — *or* mesh — *or* remote.

The interactive permission UX (`allow once / session / permanently deny …`) is a
**human-present** posture. A background worker has *no human to prompt* — so that
entire tier is not merely unused for it, it is *incoherent*. The worker must
receive its authority up front as caveats, never as a prompt. That is the first
hard mutual exclusion, and it generalizes:

> A **mode** is a *coherent bundle* of choices across these axes. The background
> worker lives at one specific point — **autonomous authority + confined engine +
> no human gate + it is itself a leaf** — which is a *different bundle* than an
> interactive session. "Toggling modes" means swapping the **whole bundle
> atomically**, and the system's job is to make the *incoherent* combinations
> **unrepresentable**: you cannot be "prompting a human" and "running headless,"
> and you cannot be "unbridled" and "confined."

This is a named-preset / loadout shape, not a pile of independent flags. The
value of naming the modes is that each name is a promise about which axis values
are locked together — and the type system, not vigilance, enforces that the
nonsensical crossings never occur.

## How the current work fits

Everything in flight is a piece of this:

- **The permission tier** (interactive once/session/permanent grants and denies)
  is the *human half* of a two-posture authority system. Its *autonomous half* —
  pre-granted caveats with no prompt — is exactly what the background worker
  needs. The two are the mutually-exclusive endpoints of the "authority source"
  axis.
- **The sandboxed-host shell engine** is the *container* the worker runs its
  commands in: full shell semantics with the filesystem kernel-fenced and
  exec/net honestly advisory-or-refused.
- **The net-host surfacing** (a refused connection now names its host) is what
  lets a confined worker's network boundary be *legible* rather than an opaque
  failure — a precondition for handing it a bounded net grant.
- **wyvern** (the headless, TUI-less, brush-locked flight tier) and the
  **crew / mesh** runners are earlier gestures at the same idea: agents that are
  not the amphibious human-facing session, but confined workers dispatched into
  a bounded box.

`newt` is amphibious on purpose: a human CLI *and* the thing that dispatches
confined workers. The mode system is how one binary honestly inhabits both
without letting the human-present affordances leak into the autonomous ones.

## Formal verification: the proof the vision rests on

If delegation is the whole game, then the one thing that *must* be true is that a
delegation never leaks authority — a tool call, a sub-agent, a mode toggle can
only ever *attenuate*, never amplify. Today that guarantee is smeared across
prose (ADRs, the caveat-lattice paper) and *sampled* by property tests. Sampling
is not proof; and the failure mode we most fear — *locally correct, globally
wrong* — is exactly the one tests miss (each function sound, the composition
across the chain drifting above the floor).

So the security formalisms get a machine-checked source of truth: a `formal/`
engine that extracts our real Rust to **Lean 4** (via Charon → Aeneas) and turns
the invariants into **theorems** (tracked in #902):

- `meet` is a genuine attenuation-only semilattice (`a.meet(b) ⊑ a` — always);
- the enforcement floor holds for *any* grant (`widen(base,g).meet(clamp) ⊑
  clamp`);
- a re-mint is always `⊑` the user root;
- **the confused-deputy bound: a sub-agent's effective caveats are `⊑` the
  caller's** — which, since sub-agent = tool call, is *the same theorem* that
  makes delegation-as-tool-calling safe.

This is not a side quest either. The mode lattice ("incoherent crossings are
unrepresentable") and the caveat lattice are the *same algebra*; Lean is what
turns "unrepresentable" from a code-review hope into a proof obligation. Verify
the lattice once, and tool calls, sub-agents, and mode toggles are all covered by
construction. **The formalism is the load-bearing proof that the whole
delegation-as-conserved-resource design does not silently give authority away.**

## The through-line

Build agents by giving away exactly enough authority, to exactly the cheapest
capable layer, inside a box the kernel enforces — and spend expensive cognition
only where nothing cheaper can stand in. The container is the design; the tool is
the payoff; the priced token is, at last, the ruler we measure it with.

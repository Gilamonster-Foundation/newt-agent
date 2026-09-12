# The illusion of infinite context

**Status:** Intention, stated by the maintainer 2026-09-12. No implementation.
This records the goal, the mechanism, and the three rules that keep the
mechanism honest, so that whoever builds it does not have to re-derive them.

**The intention, in the maintainer's words:** *to create the ILLUSION of
infinite context.* We do not need a million-token window. We need its
**effect**, and we may be able to produce that effect by clever illusion.

**Related:** [`smart-harness.md`](smart-harness.md) and
[`smart-harness-implementation.md`](smart-harness-implementation.md) (the frame
this builds on); [`../notes/2026-06-13-summarization-induced-hallucination.md`](../notes/2026-06-13-summarization-induced-hallucination.md)
(the measurement that constrains §2); `agent-frame/README.md` (the derivation
kernel); `docs/decisions/mesh_integration.md` (the transport in §6).

## 1. What a large window actually buys, and what it costs

A 1M-token window sells **reachability** and charges for **residency**. The
resident set is the expensive half: it is what degrades attention, what fills
a shared KV pool, and what #2268 measured overflowing.

The work does not need residency. A coding turn's true working set is small:
the task, a few files, the recent turns. What it needs is to be able to *get
to* anything, cheaply and verifiably.

So the thesis is not infinite context. It is:

> **Bounded residency, unbounded reach.**

That claim is checkable, which is the point of stating it that way.

## 2. Whose illusion — the one distinction the whole design turns on

An illusion presented to the **operator** is a legitimate engineering goal: the
system behaves as though its context were unbounded.

An illusion imposed on the **model inside the loop** is a defect, and we have
already measured why. [`2026-06-13-summarization-induced-hallucination.md`](../notes/2026-06-13-summarization-induced-hallucination.md)
found that a confident summary is worse than a labelled absence: **absence
routes the model to re-read; a summary suppresses recovery.** A model that
believes it holds everything will not navigate, and will invent the part it
does not hold. That is the false-completion family (see
[`terminal-bench false completions`](../../scripts/eval/harbor/README.md) and
#2268's tail) arriving through a new door.

So the rule is:

> The model is never wrong about what it is holding. It is told **"you have
> this, and you can reach anything,"** never **"you have everything."**

This is not a weakening of the illusion. The operator-facing effect is
identical, and the model-facing honesty is what makes the effect hold up under
load instead of degrading into confident fabrication.

## 3. The mechanism is virtual memory, not a trick

The useful precedent is not a magic trick, it is demand paging.

| virtual memory | here |
|---|---|
| address space | the content-addressed derivation graph (`agent-frame`) |
| resident set | the model's context window |
| page fault | a navigation to a unit not currently projected |
| pager | the context manager (§5) |
| stable address | a `ContentId` — the same bytes always have the same name |

Programs do not believe they have infinite RAM. They address freely and the
fault is handled beneath them. Virtual memory is honest by construction: an
address either resolves or faults, and a fault is **visible and handled**. The
one place it does lie — overcommit followed by an OOM kill — is universally
treated as a defect.

That is the design bar. Any place the model cannot distinguish *held* from
*reachable* is a bug, not a feature. The nearest analogue we have already hit
is the under-enforced window in #2268, where the declared budget was a promise
the harness could not keep and the server received prompts half again as large
as advertised. Overcommit, exactly.

## 4. Relative genesis: what happens at a break

A chain has terminators and a genesis. Today a break is a failure: the budget
cannot shrink further, the model changes, the backend changes, the session
moves to another machine. Under this design a break is a **transition**, not a
death: the material of the previous blocks derives a new block that begins a
new root. The chain becomes a tree.

A block minted this way is a **relative genesis** — a starting point here,
derived from a genesis we may no longer hold locally.

### 4.1 The rule that makes it safe

> **A relative genesis names the parent it cannot produce.**

`unreachable` is not `absent`. The block records the `ContentId` it derives
from and records that the bytes do not resolve locally. Someone else may hold
them; a later reader can tell a rehydrated block from a fabricated one.

Without this the block is a claim with no witness, which is exactly what
`newt-core/tests/first_principle.rs` forbids — the law
`derived_records_name_their_sources`, whose ratchet reached **0** on
2026-09-12 (#1786 Phase C). A relative genesis that starts clean would put it
straight back above zero. This is the same law as
*absent must never be ambiguous*, applied across a host boundary.

### 4.2 Branching needs a selection rule

One set of material can derive many candidate blocks. Something chooses. If the
context manager chooses silently, then the thing being measured is selected by
a model — and our own numbers say what that costs: on eight bundled fixtures
the embedded 0.5B judge admitted **0/8**, the shipped Jaccard classifier scored
**3/8** (1/6 on non-prototype cases), and only the primary model reached 8/8
(#2263). A chooser can choose the flattering history.

So:

- the choice and its reason are **admitted facts** in the frame, not implicit;
- the branch **not** taken stays addressable.

Otherwise "re-derivable" is false: you cannot re-derive what was pruned.

## 5. What the in-loop model may and must know

The harness model does not need to carry that this block is the third attempt,
or came from another model's run, or is a shadow of a session on another
machine. Keeping that out of the prompt is good for focus.

But it must not be **unable to find out**. The distinction:

> Hide it from the prompt, not from the model's reach.

The model should be able to ask whether it is on a relative genesis the way it
can query memory. Not burdening is different from deceiving, and #2273 is the
cautionary case: a model that *could* report it was blocked, inside a harness
that pressured it to fabricate an edit instead.

## 6. The context manager, and the mesh

**Start it as a navigator, not a fact-checker.** Graph queries over the DAG are
deterministic and verifiable. Fact-checking is judgement, and judgement has
been the weak leg in every measurement we have taken (§4.2). Earn the second
role with evidence, and make its verdicts admitted facts subject to the same
scrutiny as any other claim.

**The mesh is transport for signed blocks and their attested CIDs.** Mostly
already true. The one new requirement: a received block's identity is verified
**before** rehydration, and a host that holds a block whose parent it does not
have must be able to say so — §4.1's rule, crossing a wire.

## 7. How we will know it is working

"Does it feel like a million tokens" is not measurable. These are:

| signal | meaning |
|---|---|
| fault rate | how often the model needs a unit not resident |
| fault cost | latency and tokens per navigation |
| **fault when it should** | does it navigate, or invent? |

The third is the one that matters, and it is **not a new metric**: it is the
false-completion rate we already measure on terminal-bench. That convergence is
useful — the instrument that tests whether the harness is honest is the one
already built.

## 8. Open questions

1. What exactly is the resident-set eviction policy, and is it the operator's
   knob, the manager's, or derived from the fault record?
2. Does a relative genesis carry *any* replayable material, or only its parent
   id and an attestation? The first is useful; the second is cheaper and
   harder to get wrong.
3. Does the branching tree ever get garbage collected, and if so what makes a
   branch unreachable without making the history unfalsifiable?
4. Does the model's "am I on a relative genesis" query cost a tool round, or
   ride the projection it already receives?

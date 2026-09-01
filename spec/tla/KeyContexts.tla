---- MODULE KeyContexts ----

\* newt's INPUT-CONTEXT STACK, and whether the operator's escape hatch stays
\* reachable across every state that stack can reach (#2005, BHV-ESC-001..002).
\*
\* Why this is the TLA+ half and the resolver is not. `Ladder::resolve` is a
\* pure function: one trigger, one situation, one verdict. It has no temporal
\* behaviour at all, so a temporal spec beside it would constrain nothing —
\* the decorative artifact ../README.md warns is worse than none. Its algebra
\* is Lean's, in the crate. What IS temporal, and what a code change here can
\* genuinely falsify, is the SEQUENCE of contexts newt mounts and unmounts over
\* a session: the cockpit presenter, a modal, a pager, a panel, and — the next
\* one — PR8's `/settings` shell. Each takes the keyboard while it is on top.
\*
\* The hazard this models is real and already present in the code it abstracts:
\* newt has twelve production `event::read()` loops across nine files, and each
\* one is a context that either has an escape or does not. One ladder covers the
\* presenter; the other eleven are unconverted. A context mounted with no way
\* out strands the operator no matter how correct the ladder underneath it is.
\*
\* Two invariants, and NEITHER implies the other:
\*
\*   EveryActiveContextHasAHatch  no mounted context is hatchless
\*   PreemptionIsTotal            a press is resolved by the TOP context only
\*
\* Together they are the reachability claim: whatever the operator presses is
\* answered by the context on top, and that context has an escape. Drop the
\* first and the top context can be the hatchless one; drop the second and a
\* press can be answered by a frame that is not on top, which is exactly the
\* "panel that also polls the parent loop" defect.

EXTENDS Naturals, Sequences

CONSTANTS
    Contexts,   \* every input context that can be mounted
    Hatched,    \* those that offer the operator a way out; a SUBSET, spelled
                \* that way rather than as a [Contexts -> BOOLEAN] function
                \* because a .cfg cannot carry a function literal — the same
                \* shape PromptControls.tla uses for Displayed
    Base,       \* the context that is always mounted (newt: the cockpit)
    MaxDepth    \* bound on nesting, so the model is finite

HasHatch(c) == c \in Hatched

\* The base context must itself have a hatch: it is the frame the operator ends
\* up in when everything above unmounts, so a hatchless base is not a nesting
\* question but a broken application.
ASSUME /\ Base \in Contexts
       /\ Hatched \subseteq Contexts
       /\ Base \in Hatched
       /\ MaxDepth \in Nat /\ MaxDepth >= 2

VARIABLES
    stack,   \* mounted contexts, innermost first
    owner,   \* which context resolved the last press
    last     \* what just happened, so the invariants can be stated on it

vars == <<stack, owner, last>>

Init == /\ stack = <<Base>>
        /\ owner = Base
        /\ last = "none"

\* Mount a context on top of the stack.
\*
\* `HasHatch(c)` IS THE LOAD-BEARING GUARD, and the only one. Next quantifies
\* over ALL of Contexts — including the `hatchless` decoy the .cfg binds — so
\* deleting it makes Push(hatchless) enabled and TLC produces a real
\* counterexample to EveryActiveContextHasAHatch. Bounding the existential to
\* the hatch-bearing contexts instead would mask exactly that mutation, which
\* is the mistake PromptControls.tla records at its own Next.
Push(c) ==
  /\ HasHatch(c)
  /\ Len(stack) < MaxDepth
  /\ stack' = <<c>> \o stack
  /\ last' = "push"
  /\ UNCHANGED owner

\* Unmount the top context. The base never unmounts — closing it is the
\* session ending, which this model does not describe.
Pop ==
  /\ Len(stack) > 1
  /\ stack' = Tail(stack)
  /\ last' = "pop"
  /\ UNCHANGED owner

\* The operator presses a key, and some context resolves it.
\*
\* `c = Head(stack)` IS THE LOAD-BEARING GUARD here. Next quantifies over every
\* context, so a mutation to `c \in {stack[i] : i \in 1..Len(stack)}` — a panel
\* that keeps polling the parent loop while a child is mounted — becomes
\* enabled and violates PreemptionIsTotal.
Press(c) ==
  /\ c = Head(stack)
  /\ owner' = c
  /\ last' = "press"
  /\ UNCHANGED stack

\* No `Done` stutter, deliberately, and the reason is worth stating because the
\* sibling specs all have one: this model has NO terminal state. Press is
\* enabled in every reachable state, because the stack is never empty. Adding an
\* unconditional stutter action would not fix a deadlock — there is none — it
\* would disable TLC's deadlock check entirely and make the absence of one
\* meaningless.
Next == \E c \in Contexts : Push(c) \/ Press(c) \/ Pop

\* Safety only. No fairness, and no liveness property: Push/Pop cycle forever,
\* so WF_vars(Next) is satisfied by an infinite mount/unmount behaviour in which
\* Pop never fires, and any liveness property here would report a counterexample
\* that says nothing about the code.
Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ stack \in Seq(Contexts)
  /\ Len(stack) >= 1 /\ Len(stack) <= MaxDepth
  /\ owner \in Contexts
  /\ last \in {"none", "push", "pop", "press"}

\* THE ESCAPE-HATCH REACHABILITY PROPERTY. In every reachable state, every
\* mounted context offers the operator a way out — so however deep the session
\* has nested, unwinding is always possible and no frame is a dead end.
EveryActiveContextHasAHatch == \A i \in 1..Len(stack) : HasHatch(stack[i])

\* And the press goes to the top frame, so "the top has a hatch" is the same
\* statement as "this press can escape". Without this, the invariant above would
\* be about a stack nobody consults.
PreemptionIsTotal == last = "press" => owner = Head(stack)

====

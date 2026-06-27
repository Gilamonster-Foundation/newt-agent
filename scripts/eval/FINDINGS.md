# The Loop That Finished Everything and Built Nothing

*A findings essay from the autonomous #548 self-hosting experiment. The numbers,
tables, and charts live in [`results/EXPERIMENT.md`](results/EXPERIMENT.md); the
design and its threats-to-validity in [`METHODOLOGY.md`](METHODOLOGY.md). This is
the part that matters — what we learned.*

---

## What we set out to do

We made one thing the North Star: let newt complete a real GitHub issue,
autonomously, start to finish. Issue #548 — roll up the verbose `/dgx` help into
a single top-level line, keep `/dgx help` as the detail page — small, real, and
checkable. We pointed the `newt plan --one-shot` loop at it and watched.

Then we varied the world around that loop, one thing at a time, five times. Three
runs changed the *codebase* under the agent (could a landed feature help?). Three
runs changed the *model* doing the coding, from a 14-billion-parameter local
coder up through a 27-billion-parameter local generalist to frontier-class
gpt-4.1 (could a smarter model help?). One fixed instrument graded all five by
driving the built binary and looking at the actual `/help` output — not whether
the build was green, but whether the feature *existed*.

Five runs. Zero implementations. And that is the most useful result we could have
gotten.

## The machine works perfectly

This needs saying plainly, because it is genuinely hard and it genuinely works:
the loop is mechanically flawless. Every single run read the issue, grounded
itself in the repository, authored a multi-step plan, was granted scoped
capabilities, executed each leaf in its own worktree, chained the leaves into one
branch, passed its verification gate, and printed **`✓ plan complete`**. Five for
five. No wedges, no crashes, no abandoned runs. As a piece of orchestration, it
is a clean, reliable, self-hosting machine.

And it built nothing.

- **A** wrote a tidy Rust module and never wired it in — an orphan the compiler
  never sees.
- **B** deleted two hundred lines of README and bumped a version number.
- **C** did, precisely, nothing — nine leaves, nine no-ops.
- **D**, the bigger local model, hallucinated *Python* into a Rust repository.
- **E**, the frontier model, produced the most elaborate artifact of all: a
  **five-language** sprawl — C#, Python, Go, C++, and a little Rust — none of it
  in the build graph, none of it touching the function it was supposed to change.

Every one of these passed the gate. Every one reported success.

## The profound part

We expected a smarter model to do better. The opposite happened. As the executor
got more capable — 14B → 27B → frontier — the work did not get closer to correct.
It got more *confident* and more *elaborate*. The least-capable model failed by
doing nothing; the most-capable model failed by writing a polyglot cathedral in
the wrong five languages. **Capability did not correlate with success. If
anything, it correlated with the sophistication of the failure.**

Sit with that, because it inverts the reflex. The reflex, when an agent
underperforms, is "use a better model." Our data says: a better model, dropped
into the same harness, will not save you. It will produce more convincing
evidence of work that isn't there. *A more capable agent behind a dishonest gate
is not safer — it is more dangerous*, because its non-work is harder to spot.

Why did every model fail the same way? The mechanism is the same every time. The
planner — which we never changed — mislocated the target (it insisted
`help_lines` lived in `crew.rs`; it lives in `newt-tui/src/lib.rs`). The per-leaf
worker, handed an abstract instruction and no real grounding, did the natural
thing for a language model staring at a blank slate: it *invented a plausible new
file* instead of *editing the real one*. The isolated worktrees meant nothing
ever had to cohere. And the gate — `just check` — passed, every time, because
nothing the worker invented was ever in the build graph. The gate measured
compilation. The task wanted behavior. Inert files compile beautifully; they just
don't do anything.

## The lesson, stated generally

> **An autonomous agent can complete every step of a task and accomplish none of
> it. The more capable the agent, the more convincing the non-accomplishment. The
> only defense is a gate that measures the thing you actually want — not a proxy
> for it.**

`✓ plan complete` is not `the feature exists`. A green build is not a finished
feature. "The loop ran" is not "the work got done." These sound obvious written
down, and they are exactly the confusions a passing gate will quietly launder
into a sense of progress. We had a gate that said green five times while the
feature never once existed.

There is a Dijkstra line taped to the top of this whole workspace: *computer
science has as much to do with computers as astronomy has to do with telescopes.*
The loop completing is the telescope working. It is not the sky. We spent this
experiment learning, expensively and for real, not to mistake a well-functioning
instrument for the thing it was supposed to let us see. The behavioral grader is
what finally pointed at the sky. It is the one component that told the truth all
five times — and the quiet punchline is that the most valuable thing we built
while trying to evaluate newt was *the evaluator itself*.

## Why this is not a failed experiment

It is the opposite of a failed experiment. A failed experiment teaches you
nothing and leaves you where you started. This one refuted a hypothesis cleanly
("the ceiling is model capability" — no), located the real bottleneck precisely
(the harness: grounding and the honesty of the gate), and left us holding an
instrument that can measure whether any future change actually moves the needle.
We traded five runs of compute for a map of the problem and a ruler to measure
progress against. That is what a good experiment is *for*.

We did not fail to implement #548. The failure to implement #548 **is** the
finding — and it told us exactly where to dig.

## Where to dig

The levers are mechanism-level, not model-level, in priority order:

1. **Put the behavioral gate inside the per-leaf verify.** Promote the grader's
   question — *did the observable behavior change?* — into the crew loop, so a
   leaf that writes inert files cannot report success. Make the gate measure the
   thing we want. This is the highest-leverage change and it makes
   `✓ plan complete` honest.
2. **Ground the worker in the real repository.** Hand it the actual file path and
   language so it *edits the existing seam* instead of inventing a new file in a
   vacuum. Take away the blank slate.
3. **Fix the planner's grounding.** It mislocated the target every single run.
   The right neighborhood is not the right house.

None of these is a bigger model. All of them are the harness. That is the whole
lesson, and we have the ruler to prove it the next time we claim progress.

---

*Five runs, two feature versions, three executor models from local-14B to
frontier. 0/5 implemented #548. The instrument held. The map is drawn.*

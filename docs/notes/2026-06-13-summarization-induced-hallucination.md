# Summarization-induced hallucination: when the harness lies to the model

**Date:** 2026-06-13
**Status:** root-caused, fixed (#319 / PR #321), generalizes
**Tags:** context-engineering, agentic-loops, compression, hallucination, harness-design

> **Thesis.** An agent harness that *summarizes* a coding session's history can
> make the model **hallucinate APIs it had already read** — not because it lost
> information, but because it replaced ground truth with a confident paraphrase.
> A summary that says *"the agent read `api.rs`"* asserts a fact the model
> cannot actually use, and in doing so **suppresses the model's instinct to
> re-read**. The harness manufactured false confidence. The deeper lesson is
> about epistemics, not bytes: **for an agent, a confident summary is worse than
> a labelled absence.**

---

## TL;DR

- A coding agent (running on a local model) confidently **fabricated an entire
  API surface** — types and method signatures that did not exist.
- Naive diagnosis: "the model regressed / hallucinates more now." Real cause:
  **the harness's context-compression pipeline summarized away the verbatim file
  the model had read**, leaving a prose paraphrase in its place.
- The model then generated plausible-but-wrong signatures, because its context
  *asserted* it had read the file (the summary said so) without *containing* the
  file. **Absence would have prompted a re-read; the summary prevented one.**
- Reproduced **deterministically** with no live model — it's a property of the
  compression code, not of any model's mood.
- Fixed by making the harness **honest**: when it summarizes file reads it now
  appends a deterministic *re-read breadcrumb* naming the dropped files with an
  explicit "re-read before relying on contents; do not recall from this summary."
  This converts a silent hallucination into a re-read.

---

## 1. The incident

While running an autonomous coding task, an agent backed by a local model
("nemotron-3") emitted code against a client API — calling constructors and
methods with confident, idiomatic-looking signatures. **None of them existed.**
The agent had, earlier in the same session, actually `read_file`'d the real
module. It had the truth and then wrote fiction.

The operator's first read was a model regression ("it hallucinates more than it
used to"). That intuition pointed somewhere more useful than it first appears:
*if a model that wasn't fabricating this much suddenly is, and the model didn't
change, then **what the model is being shown** changed.* The harness, not the
model, was the variable.

## 2. Background: how newt compresses a session

When a conversation approaches the model's input budget, newt's agentic loop
runs a compression pipeline (`newt-core/src/agentic/compress.rs`, the "18.4
summarize-don't-discard" design). Its shape:

```
[ protected HEAD ]  [ … summarized MIDDLE … ]  [ protected TAIL ]
   system + task         replaced by an              token-budgeted
                         LLM prose summary           (~budget/4) +
                                                      freshest tool group
```

- **Head**: the system prompt and the original task — always verbatim.
- **Tail**: the most recent messages, protected by a *token* budget (~25% of the
  total) plus a guarantee that the **freshest tool-call group** (the last
  assistant `tool_calls` and its results) survives intact.
- **Middle**: everything in between is sent to a summarizer LLM and **replaced by
  a single prose summary** carrying a `[CONTEXT COMPACTION — REFERENCE ONLY]`
  marker.

This design was a deliberate, *measured* improvement. The prior behaviour
amputated the middle into a placeholder — and the baseline benchmark (B6) showed
that under silent truncation the model produced **9/10 silently wrong answers**.
"Summarize, don't discard" was the fix: degrade *visibly*, never silently.

It is the right design for question-answering. It is subtly wrong for coding —
and the reason is the whole point of this note.

## 3. The mechanism

Consider an ordinary coding session:

| Round | Event |
|------:|-------|
| 1 | task: *"add `reconnect()` to `ApiClient` using its `connect()` method"* |
| 2 | `read_file("src/api.rs")` → **the real API surface**, verbatim |
| 3–10 | reads/edits of *other* files (tests, callers, config) |
| 11 | budget reached → **compression fires** |
| 12 | model writes `reconnect()` — and invents `connect()`'s signature |

At round 11 the `api.rs` read from round 2 is:

- **not** in the protected *tail* — eight rounds of other work have pushed it out
  of the ~budget/4 token window;
- **not** the *freshest tool group* — that protection covers round 10, not 2.

So it falls into the **middle**, and the summarizer replaces

```rust
pub fn connect(&self, url: &str, timeout: Duration) -> Result<Session, ConnErr>
```

with prose like *"the agent read `src/api.rs`, which defines an `ApiClient`."*
The exact signature — the one thing the model needs at round 12 — is **gone**,
and what remains *claims the file is known*.

**Relevance is not recency.** Every protection in the pipeline is recency-based
(token-recent tail, freshest group). But the most *relevant* context for round
12 was read at round 2. A purely recency-ranked working set evicts exactly the
thing the task depends on. (`#282`, which makes compression also fire on the
*first* turn under a tight `num_ctx`, widens the window for this: even a turn-1
read can be summarized before the model's first generation.)

## 4. The deeper insight: why a summary causes *hallucination*, not just *gaps*

This is the part worth a conference slide.

A missing piece of context and a *summarized* piece of context are not the same
thing to a language model. Both lack the bytes. But they put the model in
opposite epistemic states:

- **Labelled absence** ("you have not read this file") establishes the premise
  *I don't know this yet* → the policy that follows is **go find out** (re-read,
  ask, search). Uncertainty is productive: it routes to retrieval.
- **A confident summary** ("the agent read `api.rs`, which defines `ApiClient`")
  establishes the premise *this file is known* → the policy that follows is
  **use what you know**. But the knowledge isn't there. The model, conditioned on
  "I have this," does the most probable next thing: it **generates a plausible
  continuation** — a signature that *looks* like what `ApiClient.connect` should
  be. That is hallucination, and the harness *induced* it.

The summary is therefore **worse than the absence it replaced.** Absence is
recoverable — the model's own uncertainty drives the recovery. A summary
overwrites the uncertainty with misplaced confidence and *removes the trigger
for recovery*. The model isn't lying; it's faithfully completing a context whose
premises the harness falsified.

Three corollaries that generalize past this codebase:

1. **Lossy context transforms are not epistemically neutral.** Compressing agent
   context doesn't just shrink it — it changes the model's *belief* about what it
   knows. A harness that summarizes is making a claim on the model's behalf, and
   the model will act on that claim.
2. **A model cannot distinguish "I read X and it said Y" from "my context
   asserts I read X."** It has no privileged access to the provenance of its own
   context. If the context says the file is known, the file is known — to the
   model. Provenance must be carried *in the text*, or it does not exist.
3. **Confidence is a function of the context's framing, not of the underlying
   evidence.** Reframe the same gap from "summary of a file" to "a file you must
   re-read" and the model's behaviour flips from fabricate to fetch — with *zero*
   change in the bytes available. The framing is the lever.

## 5. Deterministic reproduction

This is not a flaky, model-dependent observation. The failure lives in the
compression code and reproduces with no model at all
(`summarized_file_reads_get_a_reread_breadcrumb` in
`newt-core/src/agentic/compress_tests/retained_context.rs`): build a
message list where `src/api.rs` is read at round 2 and used after eight more
rounds, run the real `compress()` with a stub summarizer that returns prose
(as a real summarizer would), and assert on the assembled output. On the
pre-fix code:

```
#319 PROBE: fired=true  action=Summarized  api_signature_survived=false
```

The verbatim signature is provably gone before the message list ever reaches a
model. Anything downstream that "uses" `connect()` is working from prose.

## 6. The fix: an honest harness

The harness cannot always keep verbatim code within budget — that's the whole
reason compression exists. But it can refuse to **present a paraphrase as if it
were the file.** When the compression assembles its summary, it now appends a
**deterministic re-read breadcrumb** (independent of whatever the summarizer LLM
chose to say):

```
Files read or edited in the compacted span — their FULL CONTENTS are NOT
preserved in the summary above. RE-READ any you rely on before using their
exact signatures, types, or line contents; do NOT recall them from this
summary (it is prose, not the file):
- src/api.rs
- src/other_3.rs
```

This restores the labelled-absence epistemic state for exactly the files at
risk: the model is told, by name, that its memory of these files is a summary
and must be refreshed. A confident hallucination becomes a re-read.

The principle, stated generally:

> **Never let the harness assert knowledge the model does not have. When you
> must drop fidelity, label the gap so the model's own uncertainty can recover
> it. Prefer recoverable absence over confident loss.**

### What the fix is *not* (and the follow-up)

The breadcrumb is the honest *floor*, not the ceiling. It tells the model to
re-read; it does not save the re-read round, and a model that ignores the
directive can still err. The stronger follow-up is a **code-aware working set**:
preserve the most-recently-read *file* contents verbatim within budget (a
content-type-aware extension of the freshest-group protection), so re-reads are
rare rather than merely instructed. That is tracked separately; this note
documents the floor because the floor is what makes the harness *honest*, which
is the load-bearing property.

## 7. Generalizable harness-design principles

For anyone building an agent loop that compresses context (which is everyone, at
scale):

1. **Recency ≠ relevance.** Recency-ranked eviction reliably discards
   early-but-load-bearing context. Rank by *task relevance*, or at minimum keep a
   recoverable pointer to what you evict.
2. **Compression is task-typed.** Q&A tolerates lossy summary; coding (and
   anything needing verbatim tokens — code, configs, IDs, quotes) does not. A
   single compression policy across task types will be wrong for some of them.
3. **Summaries manufacture confidence; manage it.** A summary that names an
   artifact without preserving it should *always* carry a staleness/re-read
   marker. Silence about what was lost is the bug.
4. **Provenance must live in the text.** The model can't see your data
   structures. If you want it to know "this is a summary, not the source," the
   words have to say so.
5. **Test compression deterministically.** The failure was a property of the
   pipeline, reproducible without a model. Context-engineering bugs hide behind
   "the model is flaky" — make them assertable.

## 8. Why this matters beyond newt

Every production agent harness compresses context once sessions get long. The
specific failure here — *summarizing away verbatim artifacts a coding model
later needs, and thereby inducing confident hallucination* — is not a newt quirk;
it's a structural hazard of summary-based context management for tool-using
agents. The mitigation (honest staleness labelling; recoverable absence over
confident loss; relevance- and type-aware retention) is portable to any such
system. The educational core — *that a harness's lossy transform silently edits
the model's beliefs, and that confident summaries are epistemically worse than
labelled gaps* — is, we think, the genuinely transferable finding.

## References

- Issue: `#319` (investigation + bisect verdict).
- Fix: PR `#321` (`fix/319-reread-breadcrumb`).
- Originating design: "18.4 summarize-don't-discard" (`#267`); first-turn budget
  (`#282`); the B6 truncation-honesty baseline
  (`docs/testing/results/context-baseline-f0f4f6e.md`).
- Pipeline: `newt-core/src/agentic/compress.rs` (`compute_boundary`,
  `reread_breadcrumb`, `summary_message`).
- Regression guard: `summarized_file_reads_get_a_reread_breadcrumb` in
  `newt-core/src/agentic/compress_tests/retained_context.rs`.

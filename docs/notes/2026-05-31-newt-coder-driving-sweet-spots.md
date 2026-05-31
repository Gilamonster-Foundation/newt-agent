# Driving newt-agent for coding: discovered sweet spots (2026-05-31)

Field notes from piloting `newt worker --coder` headlessly to build a real
utility ([`hartsock/gitxtend`](https://github.com/hartsock/gitxtend) — a
Rust/gitoxide reimplementation of a git "tending" tool, compiled to a Python
wheel via PyO3). The pilot (a Claude Code instance) drove a local model over the
ACP stdio protocol, one task per sortie, captured the patch via `git diff`,
gated it (`cargo clippy -D warnings` + parity tests), and committed with model
provenance. ~9 method/roll-up sorties + ~5 test-authoring sorties.

**Environment:** gnuc, local Ollama. Models exercised: `qwen2.5-coder:32b`
(workhorse), `qwen3-coder:30b`. This is the data that should feed the
**model × newt-agent-size matrix** (see the issue linked at the bottom).

---

## TL;DR — the sweet spot

`newt worker --coder` (newt-coder whole-file emit) reliably produces **one
focused source file, in the project's primary language, per sortie** — e.g. a
single `pub fn` plus its unit tests. Push outside that envelope and it degrades
to **prose** (`emission_shape: prose`, `0 file(s) written`) — it "talks about"
the code instead of emitting a file.

Drive it like a scalpel, not a bulldozer: **N small single-file sorties**, the
harness wiring + git plumbing done by the pilot. Do **not** ask it for a large
multi-case file or a multi-file change in one shot.

---

## What worked: 7/7 focused Rust implementation sorties

Every `src/repo/<method>.rs` (one `pub fn` + `#[cfg(test)]` parity tests against
the real `git` CLI) emitted cleanly and, after pilot fixups, passed CI.

Prompt shape that works (imperative, file-first, single deliverable):

> Create a NEW file `src/repo/X.rs`. Do NOT modify any other file — emit only
> that one new file, complete. Implement exactly one public function: `…`. Use
> gix 0.70: `…`. Add a `#[cfg(test)] mod tests` with parity tests vs the git CLI
> using the shared `fixtures::…` helpers. Tests to include: `…`. Emit the
> complete file.

- One **new** file. **Rust.** Focused on one function.
- Turn time: ~60–220 s per sortie on a 32B local model.

## What failed: 5/5 Python test-authoring sorties → prose-scrub

Asking the worker to author a Python `unittest` E2E test file
(`python/tests/test_*.py`) returned `prose` / `0 files written` **every time**.
The failure was **invariant** across all of these axes:

| Axis varied | Values tried | Result |
|---|---|---|
| Model | `qwen2.5-coder:32b`, `qwen3-coder:30b` | prose both |
| Size | big multi-case suite, focused single-method | prose both |
| Framing | "soul" persona preamble, imperative "emit only the file" | prose both |

So the boundary is **not** prompt size and **not** the soul/persona framing
(both were ruled out by experiment). The remaining candidate causes — **untested,
for the matrix to resolve**:

1. **Language**: Rust source emits; Python does not. The project's primary
   language is Rust; the coder plugin and/or the model may be Rust-biased in how
   whole-file emission is detected/produced.
2. **Task type**: *implementing a function* vs *authoring a test* — the latter
   may pull the model toward explanation.
3. **Path**: `python/tests/` is outside `src/`; possible workspace-scan / write
   scoping effect.

**Practical rule until the matrix says otherwise:** newt-coder authors the
**Rust**; the **pilot authors the Python E2E tests**. (The compiled wheel itself
was verified working out-of-band: an 8/8 live smoke of all shipped methods vs the
`git` CLI.)

---

## The pilot's standing job: fix the library-call specifics

The worker reliably gets the **structure** and the **parity-test design** right
but misses **gix 0.70 API specifics**. The parity test is what catches it; the
pilot makes the one-line correction. Observed in this run:

| Worker wrote | Correct gix 0.70 |
|---|---|
| `Repository::open(path)` | `gix::open(path)` |
| `head.referent_name().shorten().ok()` | `shorten()` returns `&BStr`, not `Result` |
| `id.object().to_string()` | `id.detach().to_string()` (full hex) |
| `config.get(section, sub, key)` | `repo.config_snapshot().string("branch.<n>.remote")` |
| `rev_walk().with_hidden([up])` / `.ancestors_of(…Exclude)` | neither exists → collect reachable sets and diff |

**Mitigation that helped:** embed a short *verified* "gix 0.70 cheatsheet" of the
calls already known to compile in each prompt. It measurably reduced fixups on
later methods.

---

## Operational rules that held up

- **Patch-not-prose.** The worker edits files; the pilot does **all** git
  (commit / push / PR / merge) plus the mechanical wiring (2-line module
  registration + the PyO3 wrapper). Keeping git out of the worker's hands removed
  a whole class of failure.
- **Provenance via trailers.** Author = the human; `Co-Authored-By: <model-id>
  <model@newt.local>` + `Model: <id>` + `Piloted-by: newt-agent`. Squash-merge
  preserves the trailers, so `git log` / GitHub contributor credit shows which
  model wrote each method.
- **Empty/prose diff = a scrub.** Treat `emission_shape: prose` or `0 files
  written` as a crash for that *(model, task)* pair. Don't loop the same model on
  it — switch the approach (or the actor).
- **The parity test is the real gate**, not the model's self-report. `gix` result
  must equal `git` CLI output on the same fixture; clippy `-D warnings` is the
  second gate.
- **Stacked PRs bite.** A method PR based on an unmerged base branch merged into
  the *base*, not `main`, orphaning the commit. Branch every sortie from `main`.

---

## Implication for the model × newt-agent-size matrix (DGX)

The single most useful open question this run surfaced:

> **Does the Python-test prose-scrub persist on larger / DGX-hosted models, or is
> it capability-bound?**

The matrix should cross **(model × size)** against task *shape*, not just task
difficulty:

- `{ Rust impl file, Python test file, multi-file change, large single file }`
  × `{ 7B, 14B, 32B local, 70B+/DGX }` × `{ qwen2.5-coder, qwen3-coder, codestral,
  deepseek-coder-v2, … }`
- Metric per cell: **emit-rate** (did a file come out at all?) *before* you even
  measure correctness. The prose-scrub failure mode is a hard zero, and it is
  invisible to a correctness-only scorecard.

Tracked as **newt-agent#46** (model × size emit-shape matrix) — to be filled from
the bake-off matrix already in flight on another thread.

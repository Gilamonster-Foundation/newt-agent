# Cross-family PyO3 confabulation under context overflow

**Date:** 2026-06-14 · **Status:** first result, reproducible · **Rig:** #75 ·
**Related:** #319/#321 (first incident + fix), #332 (remediation), the knowledge-board
MEMO 2026-06-13 (Beaver, §7.2 harness-vs-model-separation ask)

## Abstract

A coding agent asked to "write one Python example per PyO3 crate in this
repository" against a crate set whose combined API surface **exceeds the model's
effective context window** can fabricate an entire import surface — inventing
module names from the *crate* names (`import newt_core`) instead of the real
Python surface (`newt_agent.core`). The original incident (#319/#332) was on
`nemotron3:33b`. Running the identical task through our ground-truth rig across
three models shows the failure is **model-family-specific, not structural**:
**both** nemotron models fail completely (score 0.0), while `qwen3-coder:30b` —
same harness, same corpus, same prompt — **succeeds** (1.0), discovering and
using the real surface. This refines the prior "any small model will fail"
hypothesis and gives the first concrete evidence that newt's *support harness*
should be tuned per model family.

## Background

The agent harness is compiler/linter/CI/IDE tooling aimed at a model. A human in
an IDE never ships `from newt_core import classify` because the editor underlines
it before they run anything; the model has no such feedback unless the harness
provides it. #319 root-caused the first confabulation to context compression
silently discarding the verbatim API the model had read; #321 added an honest
re-read breadcrumb; #332 added the verify oracle (does each referenced symbol
resolve?) that makes the failure *measurable*. This experiment is the first time
we drive the whole stack against real models and score the result.

## Method

- **Corpus** (the independent variable): newt-agent's own PyO3 crates, packed by
  `pack_pyo3_corpus.sh`. Each crate's `src/pyo3_module.rs` is the binding surface
  the model must read. **8 crates ≈ 20k tokens**, which **overflows the ~14k
  effective window** nemotron3 had in the incident (4 crates ≈ 12k would fit).
- **Prompt** (fixed): *"create an examples folder and write one python script as
  an example for each and every PyO3 crate in this repository."*
- **Drive:** `rig_pyo3_examples.sh live` runs `newt --no-splash code` headless
  against each model (the agentic tool loop — read/write/run over rounds).
- **Score:** `newt-eval score` runs the verify oracle (`python_imports`) over the
  produced `examples/*.py`: a reference resolves if its module — or a dotted
  prefix — is in the real `newt_agent.*` surface; an unresolved module is a
  fabrication. Module-level (the symbol-level tier needs the FFI manifest, #74).
- **Models / endpoints:** `nemotron3:33b` on the DGX (`REDACTED-HOST`);
  `nemotron-3-nano:4b` and `qwen3-coder:30b` on gnuc (`REDACTED-HOST`).

## Results

| model | family | size | score | examples | imports | what it wrote | forensics |
|---|---|---|---|---|---|---|---|
| `nemotron3:33b` | nemotron | 33B | **0.0 FAIL** | 5 | 5/5 fabricated | `import newt_core` (the *crate* name) | 22 tool events; 25,171 in / 6,612 out; **compression fired** ~9.5k→3k |
| `nemotron-3-nano:4b` | nemotron | 4B | **0.0 FAIL** | 2 | 2/2 fabricated | `import newt_data.newt_coder` (doubly-fabricated) | 12 events; 8,506 / 3,876 |
| `qwen3-coder:30b` | qwen-coder | 30B | **1.0 PASS** | 2 | 6/6 resolve | `from newt_agent._newt_agent.acp_worker import Session, TaskReply, …` | 10 events; 7,254 / 1,685 |

The real surface is the umbrella package `newt_agent` with submodules
(`newt_agent.core`/`.data`/`.tools`/`.inference`/`.coder`/`.eval`/`.acp_worker`/
`.mcp`), each bridged from a Rust crate via PyO3. The nemotron models mapped each
crate to a top-level module named after the crate; qwen-coder found the umbrella.

## Interpretation

1. **It is family-specific, not structural.** Beaver's MEMO §7.2 predicted the
   failure was structural — *any* small model crammed with N APIs would
   confabulate. The data refines this: it is **universal within the nemotron
   family** (the 33B and the 4B both fail, so it is not a quirk of one model or
   size) but **not across families** — `qwen3-coder:30b`, given identical
   context, resolves the surface correctly. So the failure is a **nemotron-family
   × harness interaction**, not an inevitability of overflow.
2. **Size doesn't rescue nemotron; it isn't (only) a capacity problem.** The 33B
   fails as completely as the 4B (the 4B is *more* broken — a doubly-fabricated
   `newt_data.newt_coder`). The overflow + the family's prior toward
   crate-name-as-module is what does it.
3. **The mechanism reproduced live.** On the 33B the rig captured compression
   firing mid-run (~9.5k → ~3k tokens) — the working set overflowing the window,
   exactly the #319 mechanism, now observed in a real run rather than a
   deterministic probe.
4. **A blind check would have missed all of it.** Every fabricated file is
   syntactically valid Python; `py_compile` passes them. Only an import-resolving
   oracle catches the fabrication — which is why the verify gate (#332) is the
   load-bearing remediation, not prompt-tuning.

## Implications

This is the first hard evidence for the program's central bet: **the support
harness should be tuned per model family.** nemotron needs the help newt already
builds — decomposition so each subtask's working set fits, curated context so the
real surface stays in view, and the verify gate so fabrications are caught and
re-grounded. qwen-coder largely doesn't, here. The right configuration is
**discovered by measurement, not guessed** — which is what the rig makes possible.

The aspirational end state (the "language packs" idea applied to the model axis):
**model-family packs** — a tuned support profile per family (window budget, tool-
round cap, disclosure mode, decomposition on/off, verify strictness, prompt/soul
shape), discovered by a nightly sweep, stored where per-model tuning already lives
(`model-capabilities.json`, the Phase-20 auto-tuner being the seed), and applied
automatically. A "newt-agent **for** nemotron" as a togglable profile, distinct
from the qwen profile.

## Threats to validity

- **N = 3, one corpus, one prompt, one run each.** A nightly sweep over more
  models × corpus sizes × seeds is the next instrument (it is why we are building
  the sweep wrapper).
- **Module-level scoring only.** The oracle currently checks module existence,
  not symbol existence (`newt_agent.core` *has* `Router`). The FFI manifest (#74)
  upgrades this; until then a model that imports a real module but a fake symbol
  scores as a pass.
- **qwen used the native path** `newt_agent._newt_agent.acp_worker` rather than
  the stitched `newt_agent.acp_worker`. Both resolve and both work at runtime, but
  it is worth noting the "pass" used the lower-level surface.
- **Endpoints differ** (33B on DGX, others on gnuc). Same Ollama, same prompt and
  harness; backend latency differs but the scored artifact does not depend on it.

## Future work

- **Sweep wrapper + nightly CI** — run model-list × corpus-size, diff scorecards
  over time, so a harness change that helps one family and hurts another is
  visible.
- **Model survey system** — load/unload/iterate downloadable models on gnuc and
  the DGX, building a per-family failure profile.
- **Per-family tuning** — once the sweep makes the differences routine, search
  the knobs (window, rounds, decomposition, verify) per family and persist the
  winners; the self-tuning harness.
- **The paper** — *"Newt-agent: an agent for Nemotron"* — the harness as
  family-aware tooling, with this experiment as the seed result.

# The model had the answer: PyO3 fabrication is post-compression sampling variance, not information loss

**Date:** 2026-06-14 · **Status:** forensic, reproduced · **Rig:** #75 · **Repeats:** #350 ·
**Refines:** [`2026-06-14-cross-family-confabulation.md`](2026-06-14-cross-family-confabulation.md) (interp. #3) ·
**Related:** #319/#321 (incident + re-read breadcrumb), #332/#73 (verify gate), #74 (FFI manifest)

## Abstract

The cross-family finding established *that* nemotron fabricates a PyO3 import
surface (`import newt_core`) where qwen-coder does not. This note establishes
*why*, by diffing a **passing** and a **failing** run of the **same model on the
same hardware**. The two runs were given byte-identical inputs (same corpus,
same `python_surface.json`, same 8-crate working set — md5-verified), and the
model **received the full ground truth in context**: it read the
`src/pyo3_module.rs` files that literally declare
`#[pyclass(module = "newt_agent._newt_agent.core")]`. The failing run read *more*
of those files than the passing run, and the harness delivered the complete file
content to the model in both cases (the "(N more lines hidden)" notice is a
display-only elision — `display.rs` comments *"the model always gets the full
content"*). **Context compression fired in both runs**, compressing just as
aggressively in the pass (~24.8k→5.7k tokens) as in the fail (~24.7k→4.3k). The
divergence is therefore **not** information availability and **not** overflow per
se — it is what the model does *after* compression evicts the verbatim surface:
**re-ground (re-read, recover the real path) or fall back to a strong, wrong
prior (one snake-cased package per Rust crate)**. For nemotron3 that choice is
**sampling-stochastic**. This reframes the fix: more context or a better re-read
nudge cannot fix a model that already had — and ignored — the answer. The
load-bearing remedies are (1) make the surface a compression-surviving
*structured fact*, and (2) *verify-and-revert-retry* to exploit the stochasticity.

## Method

Two runs of `nemotron3:33b` on the DGX (`REDACTED-HOST`), same prompt
(*"create an examples folder and write one python script as an example for each
and every PyO3 crate in this repository"*), driven by the #75 rig:

- **FAIL:** `/tmp/rig-dgx` — score 0.0, 5/5 fabricated.
- **PASS:** `/tmp/survey-dgx/run-nemotron3_33b` — score 1.0, 15/15 resolve.

Input parity was verified by md5, not assumed:

| artifact | FAIL md5 | PASS md5 | identical |
|---|---|---|---|
| `python_surface.json` | `b4dd6dc1…` | `b4dd6dc1…` | ✅ |
| `newt-core/src/pyo3_module.rs` | `dc2a6bd5…` | `dc2a6bd5…` | ✅ |
| working-set crate count | 8 | 8 | ✅ |

## What diverged — and what didn't

**Didn't diverge — the reads.** Both runs read the binding sources; the FAIL read
*more* of them (8 `pyo3_module.rs` vs 6):

```
FAIL reads: acp-worker, coder, core, data, eval, inference, mcp-server, tools (+3 re-reads)
PASS reads: acp-worker, coder, core, data, eval, inference            (+3 re-reads)
```

**Didn't diverge — the ground truth in context.** `newt-core/src/pyo3_module.rs`,
read by both, contains the authoritative import path explicitly:

```rust
create_exception!(_newt_agent, PyNewtError, PyException);          // the extension module
#[pyclass(name = "Router", module = "newt_agent._newt_agent.core")] // the import path, verbatim
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) {     // a *sub*module under a parent
    let m = PyModule::new(py, "core")?;                             // …named "core", not "newt_core"
```

plus a doc comment naming the umbrella crate that aggregates the bindings. The
harness delivered this in full to both runs (display elision ≠ context
truncation, confirmed in `newt-core/src/agentic/display.rs`).

**Didn't diverge — the compression.** Both runs overflowed and compressed:

| | compression events | example reduction |
|---|---|---|
| FAIL | 3 | ~24,663 → ~4,293 tokens |
| PASS | 2 | ~24,835 → ~5,738 tokens |

**Did diverge — the post-compression move.** After compression evicted the
verbatim bindings (~round 9 in both), the PASS re-read and re-grounded (its peak
output round wrote the grounded examples *after* recovering the surface); the
FAIL stayed in low-output churn and emitted the fabricated imports. The two
models wrote:

| run | what it wrote | resolves? |
|---|---|---|
| PASS | `import newt_agent._newt_agent.data`, `from newt_agent._newt_agent import …` | 15/15 ✅ |
| FAIL | `import newt_core`, `import newt_coder`, `import newt_data`, … | 0/5 ❌ |

The fabrication is the **Rust crate name, snake-cased, as a top-level package** —
a plausible maturin-per-crate convention, wrong for this single-extension
workspace. The same signature appears verbatim in `nemotron-3-nano:latest`
(`import newt_core`, `newt_coder`, …), so it is a **family prior**, not one
model's quirk.

## Quantifying the stochasticity (p)

The cross-family doc flagged that single-run cells mislabel borderline models.
The repeats tooling (#350) measures it directly — **K=5** runs of each model on
the DGX against the same `corpus-stress` overflow corpus:

| model | clean pass | failure shape | other outcomes |
|---|---|---|---|
| `nemotron3:33b` | **1/5** (20%) | 3/5 **partial** (score 0.58–0.67 — grounds most imports, fabricates a few) | 1/5 no-output |
| `nemotron-3-nano:30b` | **2/5** (40%) | 2/5 **total** (score 0.0 — fabricates the whole surface) | 1/5 vacuous (wrote files, no imports) |

Run sequences: 33B = `fail · pass · fail · no-output · fail`; nano =
`fail · pass · pass · fail · vacuous`. Three things this sharpens:

1. **Borderline, confirmed.** A clean-pass rate of 20–40% over byte-identical
   inputs is exactly the stochasticity single-run cells hid. Neither model "always
   fabricates"; each fabricates *at a rate*.
2. **The failure SHAPE differs within the family.** The 33B degrades *gracefully*
   — its failures are partial (most imports ground, a handful are fabricated); the
   30B nano fails *catastrophically* (0.0, the whole surface invented). Same
   family, same crate-name prior, but size changes how the failure *lands*, not
   whether it happens. Verify-strictness and retry granularity are therefore
   per-model knobs, not one family setting.
3. **Failure isn't binary.** Two of ten runs were neither pass nor fabrication —
   one wrote no files (no-output), one wrote import-less files (vacuous). The
   four-outcome rubric earns its keep; a binary pass/fail mislabels both (note the
   33B's no-output run scored a *vacuous* 1.0 on imports — zero imports trivially
   "resolve" — which the rubric correctly demotes rather than counting as a pass).

**The partial-failure shape changes the retry economics (refines R2).** Because
the 33B grounds most imports on every run (per-import success ≈ 0.8 even on a
*failing* run), verify-gated revert-and-retry should revert only the **fabricated
files** the oracle pinpoints (`Verdict::Fabricated` by `file:line`), not the whole
task. A file-scoped retry compounds the high per-import rate and converges far
faster than the task-scoped `p^N` (full-task `p ≈ 0.2` for the 33B). The nano's
*total* failures argue the opposite for it — a 0.0 run has nothing worth keeping,
so a task-scoped revert fits. **The right retry granularity is itself a per-model
profile knob.**

## Why this reframes the fix

The incident chain assumed an **information-loss** mechanism: #319 root-caused
compression silently discarding the API the model had read; #321 added a re-read
breadcrumb so the model is told to pull it back. That mechanism is real and
necessary — but this run pair shows it is **not sufficient to explain the
failure**, because the model that fabricated **had the full surface, read it, and
overrode it with a prior** on that sampling roll. A model that ignores an answer
it already has is not helped by being given the answer again.

Concretely, three "obvious" fixes are **disconfirmed** by this evidence:

- **Bigger context window / better retrieval** — the info was present and complete.
- **The re-read breadcrumb alone (#321)** — the FAIL had it available and still
  did not re-ground on that roll. Necessary nudge, not a guarantee.
- **Progressive-disclosure memory (Workstream A) alone** — the content *was*
  disclosed; disclosure isn't the gap.

## Recommendations (by leverage)

**R1 — FFI-introspection manifest (#74): make the surface a compression-surviving
structured fact.** Extract `{crate → import_path}` once from the bindings
(`#[pymodule]` name, `#[pyclass(module = …)]`, the `register(parent)` submodule
names) and pin a tiny authoritative block in the working set:
`newt-core → newt_agent._newt_agent.core`. It is small and high-value, so it
survives prune+summary where 20k of verbatim source cannot — removing the
post-compression ambiguity the prior fills. *This is the single highest-leverage
item: the answer existed in the source and compression evicted it; an 8-line
manifest is the form of the answer that survives.*

**R2 — verify-gated revert-and-retry (#73, S1): exploit the stochasticity.** The
verify oracle (`newt-core::symbols`, `Verdict::Fabricated`) already detects the
fabrication; wire it as a gate that, on a fabricated import, **reverts the file
and retries** rather than asking the model to fix-in-place. Revert is correct
*because* the failure is sampling-driven — a fresh roll has the pass-rate's
chance of grounding correctly (the PASS proves the model *can*), whereas
fix-in-place keeps it anchored on the same wrong prior. With measured `p`, N
attempts → ≈ `p^N` failure.

**R3 — compression preserves symbol facts, not prose (Workstream B,
`on_pre_compress`).** The compression that fired in both runs reduced via
prune+summary, discarding the verbatim `module = …` attributes. Have
`on_pre_compress` extract and pin import-path / signature facts so they survive
as **exact data**, not lossy summary. This is R1 generalized into the
compression path — the principled version of the #319 fix.

**R4 — the breadcrumb should carry the fact, not a chore (#321 refinement).**
Today the re-read breadcrumb says *"re-read the file."* Make it carry the evicted
fact: *"you read newt-core's binding; its import path is
`newt_agent._newt_agent.core`."* Merging R1's manifest line into the breadcrumb
turns a request the model can decline into a fact it would have to actively
contradict.

**R5 — per-family tuning is a post-compression-recovery knob.** The family
signature is specific and the lever is specific: for nemotron, raise the
pressure to re-ground after compression (inject the manifest, lower the re-read
bar, cap working-set per subtask). This is the concrete content of a
"newt-agent **for** nemotron" profile — discovered by the sweep, not guessed.

## Threats to validity

- **One pass/one fail pair** for the qualitative diff; the K=5 repeats (above) are
  the N>1 backing (20–40% clean-pass, the rest partial/total fabrication). The md5
  parity makes the single pair load-bearing for the *input-identity* claim
  regardless of N. The repeats ran on a current-`main` 0.6.8 binary (no harness
  change vs. the published baseline); a larger K and a seed log would tighten the
  rate.
- **Module-level scoring** (the FFI manifest, #74 / R1, upgrades to symbol-level).
- **"Re-grounded after compression" is inferred** from the round-by-round token
  probes (PASS shows a high-output write round after a re-read; FAIL does not),
  not from a labelled trace. A future instrument: tag each emitted import with the
  round and whether the binding was in-context at emission.

## Bottom line

The #319 fix family treated fabrication as *the model lost the API*. This run
pair shows it is *the model had the API and, on some rolls, trusted a prior over
it.* The harness cannot make a stochastic model deterministic — but it can (R1)
keep the answer in a form compression won't drop, and (R2) check the output and
take another roll when it's wrong. Those two are the load-bearing 0.6.9 work; the
rest is tuning.

# Methodology — the model survey

How we measure what the newt-agent harness does to a model. This is the
reproducible procedure behind everything in [`docs/test-results/`](../test-results/)
and the findings in [`docs/findings/`](../findings/).

## Premise

The agent harness is **compiler / linter / CI / IDE tooling aimed at a model
instead of a human.** A human in an IDE never ships `from newt_core import
classify` because the editor underlines it before they run anything; a model has
no such feedback unless the harness supplies it. So the harness's effect on a
model is **measurable, not assumed** — and different model families respond to
the same harness differently. The survey measures that, per model, per hardware.

**There is no failed experiment.** Every outcome is a data point on the
capability frontier: a model that *passes* tells us the harness suffices for that
family; one that *fabricates* tells us where the support harness must do more;
one that produces *no output* tells us it can't drive the loop at all; a
*timeout* tells us it's outside the harness's time budget. We record all four.

## The instrument (rig)

```
pack_pyo3_corpus.sh   → assemble N PyO3 crates into a working-set corpus
rig_pyo3_examples.sh  → drive newt headless against a model on a fixed prompt
newt-eval score       → score the produced Python with the verify oracle
survey_models.sh      → sweep a model list on one endpoint → results matrix
```
All under `docs/testing/results/scripts/`.

### Corpus — the independent variable

`pack_pyo3_corpus.sh --crates N` packs N of newt-agent's own PyO3 crates (each
crate's `src/pyo3_module.rs` is the binding surface the model must read). The
combined size is the **working-set knob**: a ~14k-token effective window (what
nemotron3 had in the first incident) overflows around 5 crates.

- `--crates 4` ≈ 12k tokens → **fits-window** (control)
- `--crates 8` ≈ 20k tokens → **overflow** (the incident regime; the survey default)

It also emits `python_surface.json` — the **real** importable surface (the
umbrella `newt_agent` + its eight submodules), which the oracle scores against.

### Task — fixed prompt

> *create an examples folder and write one python script as an example for each
> and every PyO3 crate in this repository.*

### Drive

`rig_pyo3_examples.sh live` runs `newt --no-splash code` headless (the real
agentic tool loop — read/write/run over rounds, up to `max_tool_rounds`) in a
throwaway sandbox HOME, against the target model/endpoint.

### Score — the verify oracle (the honest judge)

`newt-eval score` runs the `python_imports` evaluator (`newt-core::symbols`) over
the produced `examples/*.py`: an import resolves iff its module — or a dotted
prefix — is in the real surface; an unresolved module is a **fabrication**.
Module-level today (symbol-level needs the FFI manifest, #74). This catches the
class a blind `python -m py_compile` cannot (a fabricated import is valid Python).

### Forensics

Read from the run's sandboxed `~/.newt/conversations.db`: max-turn tool-event
count, tokens in/out, and a cap-hit inference (`events ≥ max_tool_rounds`, since
`end_reason` isn't persisted yet, #75).

## Outcomes (every cell is data)

| symbol | meaning |
|---|---|
| ✅ **PASS** | wrote examples; **all** imports resolve to the real surface |
| ❌ **FAIL** | wrote examples; one or more **fabricated** imports |
| ∅ **no-output** | wrote no `.py` files — did not complete the task (a vacuous import score of 1.0 is *not* a pass) |
| ⚠ **timeout / error** | exceeded the per-model time budget, or the run errored |

## Hardware

| label | device | memory |
|---|---|---|
| `gnuc 4060 Ti` | NVIDIA GeForce RTX 4060 Ti | 16 GB |
| `DGX Spark` | NVIDIA DGX Spark (GB10) | 128 GB unified |

Inference is served by Ollama (`gnuc-ollama.home.lab`, `dgx-ollama.home.lab`).
Hardware bounds which models fit and how fast they run; the *scored artifact*
does not depend on it, but no-output/timeout outcomes often do.

## Reproduce

```bash
# build the 0.6.8 binaries under test
CARGO_TARGET_DIR=~/.cache/newt-target cargo build --release -p newt-agent -p newt-eval

# pack the overflow corpus
docs/testing/results/scripts/pack_pyo3_corpus.sh --repo . --out /tmp/corpus8 --crates 8

# sweep a model list on one endpoint
NEWT_BIN=~/.cache/newt-target/release/newt \
NEWT_EVAL_BIN=~/.cache/newt-target/release/newt-eval \
docs/testing/results/scripts/survey_models.sh \
  --endpoint https://gnuc-ollama.home.lab --hardware "gnuc 4060 Ti" \
  --corpus /tmp/corpus8/corpus --surface /tmp/corpus8/python_surface.json \
  --out /tmp/survey-gnuc --models "nemotron-3-nano:4b qwen3-coder:30b ..." --timeout 900
```

## Caveats / threats to validity

- **One run per cell.** No seed averaging yet; a future nightly sweep adds repeats.
- **Module-level scoring.** A real module with a fabricated *symbol* scores as a
  pass until the FFI manifest (#74) lands.
- **Timeouts are hardware-bound.** A model that times out on the 4060 Ti may pass
  on the DGX Spark — the matrix is per (model, hardware).
- **The rig drives `newt code` over stdin**, not the (not-yet-built) clean `newt
  run` headless CLI; behavior is equivalent but the entry point will change.

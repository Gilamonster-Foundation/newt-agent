# Findings — empirical harness research

This directory is the **durable record of what we learn by running the agent
harness against real models.** It is the publication track: each finding is a
dated, self-contained write-up grounded in a reproducible experiment, and the
collection is meant to be built on over time (re-run as models change, extended
to new model families and corpora).

## Why this exists

newt-agent is the **drake-swarm training ground**: a local-first coding agent
whose PR gates are honest CI, and whose harness we deliberately stress to learn
how LLMs actually behave under it. The premise (see
`docs/notes/2026-06-13-summarization-induced-hallucination.md`) is that **the
agent harness is a variation of compiler / linter / CI / IDE tooling aimed at a
model instead of a human** — and like any tool, its effect on the model is
measurable, not assumed. These findings are those measurements.

## The measurement instrument

The **ground-truth stress rig** (`docs/testing/results/scripts/`,
[#75](https://github.com/Gilamonster-Foundation/newt-agent/issues/75)) is the
fitness function:

```
pack_pyo3_corpus.sh   → assemble N PyO3 crates into a working-set corpus (the size knob)
rig_pyo3_examples.sh  → drive newt headless against a model on a fixed prompt
newt-eval score       → score the output with the verify oracle (does each import resolve?)
                      → scorecard.json {fabricated imports, tokens, tool events, cap-hit}
```

The verify oracle (`newt-core::symbols`, the `python_imports` evaluator) is the
honest judge: a model's output is "usable" to the degree its references resolve
against the real symbol surface — the failure class a blind `py_compile` cannot
see.

## Index

| date | finding | TL;DR |
|---|---|---|
| 2026-06-14 | [Cross-family PyO3 confabulation](2026-06-14-cross-family-confabulation.md) | The "fabricate an entire API surface under context overflow" failure is **model-family-specific, not structural**: both nemotron models fail (score 0.0), qwen3-coder:30b passes (1.0) the identical task. → the support harness should be tuned per model family. |

## How to add a finding

1. Run an experiment with the rig (record the exact corpus, prompt, model,
   endpoint, and the scorecard JSON).
2. Write `docs/findings/YYYY-MM-DD-short-name.md`: abstract, method, results
   table, interpretation, threats to validity, future work.
3. Add a row to the index above.
4. Keep claims tied to the artifact they came from — the same discipline the
   harness enforces on the model.

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
| 2026-06-14 | [Fabrication is sampling variance, not info loss](2026-06-14-fabrication-is-sampling-not-information-loss.md) | Same model, same byte-identical inputs: one run grounds 15/15, another fabricates 5/5. The model **had** the surface and overrode it with a crate-name prior; compression fired in both. → fix with a compression-surviving fact (R1) + verify-gated revert-retry (R2), not "re-read". |
| 2026-06-15 | [Verify-gated retry: grounding vs. gate-gaming](2026-06-15-retry-and-the-honest-gate.md) | A revert-retry loop reads 3/3 → 1.0 naively, but honestly: 1 real grounding, 1 no-output, 1 **gate-evasion**. Under retry, nemotron games the gate's blind spots (prefix-breadth, hyphen, wildcard). → verify-gated retry is a **composable technique** whose worth is bounded by its gate's adversarial completeness. |
| 2026-06-16 | [Harness in-loop on live Nemotron](2026-06-16-nemotron-in-loop-validation.md) | First end-to-end run of the model-support-kit (`knowledge_base`+`verify_gate`+`retry`) against the live model the incident came from: baseline **0.0** → profile **1.0**, fabrication eliminated. → the harness techniques compose and work in-loop; grounding is model-bound. |
| 2026-07-29 | [DGX Spark on Terminal-Bench — capability survey](2026-07-29-dgx-spark-terminal-bench-survey.md) | A one-day tb-30 survey of a DGX Spark across local + hosted models via newt, with the integrity log. **A dated snapshot** (numbers move as models/harness change); the live table is the README scoreboard. → re-runs land as new dated files, see below. |

## Re-running a moving survey

Some findings are **experiments frozen at a date** (the four above). A **survey**
(the DGX-Spark tb-30 doc) is different: the same instrument is re-run over time, so
its numbers **move**. Version them by *dating whole snapshots*, never by editing a
score in place:

1. Re-run the suite; the always-current numbers land in the README scoreboard
   (auto-generated from `scripts/eval/bench-results.jsonl`).
2. Write the narrative as a **new** dated file
   `docs/findings/YYYY-MM-DD-dgx-spark-terminal-bench-survey.md` — copy the prior
   snapshot's skeleton, record the run's newt version + model digests + integrity
   log, and state what moved vs. the last snapshot.
3. Add an index row above; leave every prior snapshot **unedited** as the record of
   what was true on its date. Point the top-level `README.md` link at the newest.

This keeps the findings a *build-on-over-time* series (per the intro) instead of a
single doc that silently rewrites its own history.

## How to add a finding

1. Run an experiment with the rig (record the exact corpus, prompt, model,
   endpoint, and the scorecard JSON).
2. Write `docs/findings/YYYY-MM-DD-short-name.md`: abstract, method, results
   table, interpretation, threats to validity, future work.
3. Add a row to the index above.
4. Keep claims tied to the artifact they came from — the same discipline the
   harness enforces on the model.

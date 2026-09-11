# Field notes and studies

The durable output of this experiment is what building it teaches about how
LLMs behave inside a harness. Dated notes live in this directory; studies
that live elsewhere are indexed here too.

- **[Summarization-induced hallucination](./2026-06-13-summarization-induced-hallucination.md)** — a confident summary is worse than a labelled absence: absence routes the model to re-read, a summary suppresses recovery.
- **[Truncation honesty](../testing/results/context-baseline-f0f4f6e.md)** — silent context truncation yields *silently wrong* answers; every fix moves the failure, it doesn't always remove it.
- **[Coder-driving sweet spots](./2026-05-31-newt-coder-driving-sweet-spots.md)** — where small local models are and aren't reliable at agentic coding.
- **[Hermes learnings](../design/context-memory-hermes-learnings.md)** — take the algorithms, refuse the architecture.

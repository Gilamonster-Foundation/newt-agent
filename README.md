# Newt-Agent

<p align="center">
  <img src="docs/logos/newt-agent-logo_source.png" alt="Newt-Agent logo" width="256" />
</p>

> **Experimental agentic coder.**

Written in Rust. The default build ships no cloud provider — remote models are
opt-in subprocess plugins. The scoreboard below is the claim, measured on
Terminal-Bench, confined and unconfined.

## Terminal-Bench

Measured on [Terminal-Bench](https://github.com/harbor-framework/terminal-bench)
via `newt solve` (headless) plus the Harbor adapter. The release gate is a
**per-model monotonic ratchet** — a model's score never goes down across
releases; establish a starting number, then keep beating it. Both lanes are
published, because confined (**OCAP on**) versus unconfined (**OCAP off**) is
the claim worth making: security you can afford to leave switched on.

<!-- BENCH-SCOREBOARD:START -->
_Per-model Terminal-Bench champions, **OCAP off vs on**. Each lane is a monotonic ratchet (a score never goes down). 0.7.6 establishes the honesty-classified, digest-pinned confined (OCAP-on) baseline; OCAP-on within reach of OCAP-off (parity) is pursued forward via pre-granted permissions, not gated here. Auto-generated; do not edit by hand._

| Model | OCAP off | OCAP on |
|-------|----------|---------|
| `nemotron-3-super`<br><sub>nemotron · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 36.7% (11/30) | 26.7% (8/30) |
| `ornith-1.0-35b-q8`<br><sub>ornith · tb-30 · ctx 65536 · v0.7.6 · 2026-07-29</sub> | _pending_ | 36.7% (11/30) |
| `qwen3.6_35b`<br><sub>qwen · tb-30 · ctx 65536 · v0.7.6 · 2026-07-29</sub> | 20.0% (6/30) | 26.7% (8/30) |
| `o4-mini`<br><sub>openai · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 13.3% (4/30) | 16.7% (5/30) |
| `qwen3-coder_30b`<br><sub>qwen · tb-30 · ctx 65536 · v0.7.6 · 2026-07-29</sub> | 10.0% (3/30) | 13.3% (4/30) |
| `gpt-oss_120b`<br><sub>openai · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 10.0% (3/30) | 10.0% (3/30) |
| `kimi-linear_48b`<br><sub>kimi · tb-30 · ctx 65536 · v0.7.6 · 2026-07-31</sub> | _pending_ | 10.0% (3/30) |
| `nemotron-3-nano_30b`<br><sub>nemotron · tb-30 · ctx 65536 · v0.7.5 · 2026-07-29</sub> | 6.7% (2/30) | _pending_ |
| `glm-4.7-flash`<br><sub>glm · tb-30 · ctx 65536 · v0.7.6 · 2026-07-31</sub> | _pending_ | 3.3% (1/30) |
| `gpt-4.1-mini`<br><sub>openai · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 0.0% (0/30) | 3.3% (1/30) |
| `deepseek-coder-v2_16b`<br><sub>deepseek · queued</sub> | _queued_ | _queued_ |
| `deepseek-r1_32b`<br><sub>deepseek · queued</sub> | _queued_ | _queued_ |
| `gemma4_31b`<br><sub>gemma · queued</sub> | _queued_ | _queued_ |
| `kimi-dev_72b`<br><sub>kimi · queued</sub> | _queued_ | _queued_ |
| `nemotron-3-super_120b`<br><sub>nemotron · queued</sub> | _queued_ | _queued_ |
| `nemotron-mini_4b`<br><sub>nemotron · queued</sub> | _queued_ | _queued_ |
| `nemotron_70b-instruct-q8_0`<br><sub>nemotron · queued</sub> | _queued_ | _queued_ |
| `ornith-1.0-397b-iq1_m`<br><sub>ornith · queued</sub> | _queued_ | _queued_ |
| `qwen2.5-coder_32b`<br><sub>qwen · queued</sub> | _queued_ | _queued_ |
| `qwen3-coder-next_latest`<br><sub>qwen · queued</sub> | _queued_ | _queued_ |

<!-- BENCH-SCOREBOARD:END -->

**Full results** — every model including those still queued, per-run provenance,
and the harness methodology — are published by
[gilamonster-bench](https://github.com/Gilamonster-Foundation/gilamonster-bench),
a separate instrument that has no dependency on newt. If the ruler shipped with
the thing it measures, one commit could move both at once. For how these
particular numbers were kept honest — including the runs thrown out — see the
[DGX Spark capability survey](./docs/findings/dgx-spark-terminal-bench-survey.md).

## Quick start

```bash
git clone https://github.com/Gilamonster-Foundation/newt-agent
cd newt-agent
just install                       # → ~/bin/newt, ~/bin/newt-mcp-server
newt setup inference.example.net   # probe discovery ports, select a model
newt code                          # TUI coder in the current directory
```

Authenticated endpoints, discovery ports, and where backends are stored:
[the setup guide](./docs/guide/setup.md). Inside the TUI, `/mode` picks a working
style and `/posture` is the separate authority control — a posture floor can only
ever narrow authority
([decision record](./docs/decisions/operating_modes_and_permission_postures.md)).
Tool output renders through a bounded, tail-biased spill that `/spill` tunes
([newt-tui](./newt-tui/README.md)). Run `newt --help` for every mode (worker, MCP
server, doctor, config, …) — the binary is the authority on its own surface, this
file is not. Python bindings live in [`newt-agent-py/`](./newt-agent-py/).

## Why a bridle, not just a harness

An agent *harness* helps the model do work; a **bridle** lets the operator
*steer* — and prove, after the fact, exactly where the horse went. Newt is an
experiment in making Object Capability (OCAP) security — long considered
theoretically correct but practically unimplementable — pragmatic inside an agent
loop, as a reusable concept
([`agent-bridle`](https://github.com/Gilamonster-Foundation/agent-bridle))
intended to be pluggable into other harnesses, not just this one.

Because OCAP is an algebraic construction, some questions are answered
*structurally* rather than by audit-log archaeology: who acted on what and when,
who granted the authority for it, and whether **only** what was permitted
actually happened. For anyone whose work lives on provenance, authority,
integrity, and data sovereignty — lawyers, clinicians, data scientists — those
answers have to be properties of the system, not promises in a policy document.
The long form is [`docs/vision.md`](./docs/vision.md).

If it doesn't find its day in the sun, it was fun anyway.

## Design laws

The invariants. Each links to the decision record that argues it.

- **Local-first inference.** The default binary speaks only to local
  backends. Cloud providers are opt-in subprocess plugins speaking the
  JSON-RPC schema in [`plugins-protocol/`](./plugins-protocol/) — the opt-in
  is enforced at the **build** level, not a runtime flag.
- **Fail-closed OCAP.** Authority is a caveat lattice, not a denylist; a
  fixed safety floor no mode or grant can unlock. See
  [`docs/decisions/agentic_object_capability_security.md`](./docs/decisions/agentic_object_capability_security.md)
  and [`docs/decisions/ocap_confinement_model.md`](./docs/decisions/ocap_confinement_model.md).
- **Small crates, zero warnings, coverage-gated.** `just check` mirrors CI;
  the pre-push hook runs it. One operator's leverage *is* this discipline.
- **Patch, not prose.** Delegated work is verified by the harness (real
  diffs, real test runs — [`newt-eval/`](./newt-eval/)), never by trusting a
  model's summary of itself. The bench ratchet above is the same law at
  release scale: verify by artifact, never by self-report.
- **Skills are on-demand context.** The prompt carries an index; bodies load
  when used. See [`docs/decisions/agent-skills.md`](./docs/decisions/agent-skills.md)
  and the bundled skills in [`.newt/bundled-skills/`](./.newt/bundled-skills/).
- **Issues are ground truth.** [`ROADMAP.md`](./ROADMAP.md) sequences
  delivery, but GitHub issue state is authoritative — the document is only
  the map.
- **Causal ordering, not wall-clock.** Timestamps are display *claims*; the
  conversation store orders on signed per-writer ticks + content hashes. See
  [`docs/decisions/conversation_context_architecture.md`](./docs/decisions/conversation_context_architecture.md).

## Field notes

The durable output of this experiment is what building it teaches about how LLMs
behave inside a harness.

- **[Summarization-induced hallucination](./docs/notes/2026-06-13-summarization-induced-hallucination.md)** — a confident summary is worse than a labelled absence: absence routes the model to re-read, a summary suppresses recovery.
- **[Truncation honesty](./docs/testing/results/context-baseline-f0f4f6e.md)** — silent context truncation yields *silently wrong* answers; every fix moves the failure, it doesn't always remove it.
- **[Coder-driving sweet spots](./docs/notes/2026-05-31-newt-coder-driving-sweet-spots.md)** — where small local models are and aren't reliable at agentic coding.
- **[Hermes learnings](./docs/design/context-memory-hermes-learnings.md)** — take the algorithms, refuse the architecture.

## Where things live

| What | Where |
|---|---|
| Setup beyond the quick start | [`docs/guide/setup.md`](./docs/guide/setup.md) |
| Benchmark results & methodology | [gilamonster-bench](https://github.com/Gilamonster-Foundation/gilamonster-bench) |
| Forward plan | [`ROADMAP.md`](./ROADMAP.md) (issue numbers are the live state) |
| Release history | [`CHANGELOG.md`](./CHANGELOG.md) |
| Design docs & studies | [`docs/design/`](./docs/design/) |
| Decision records | [`docs/decisions/`](./docs/decisions/) |
| Field notes | [`docs/notes/`](./docs/notes/) |
| Terminal UI | [`newt-tui/README.md`](./newt-tui/README.md) |
| Evaluation harness | [`newt-eval/README.md`](./newt-eval/README.md) |
| Local gate | `just check` (see [`justfile`](./justfile)) |

## License

Apache-2.0. See [LICENSE](./LICENSE).

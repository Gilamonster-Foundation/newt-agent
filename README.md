# Newt-Agent

<p align="center">
  <img src="docs/logos/newt-agent-logo_source.png" alt="Newt-Agent logo" width="256" />
</p>

> **Experimental agentic coder**, written in Rust. Local-first: the default
> build ships no cloud provider. The scoreboard below is the claim.

## Terminal-Bench

Measured on [Terminal-Bench](https://github.com/harbor-framework/terminal-bench)
via `newt solve` and the Harbor adapter, confined (**OCAP on**) and unconfined
(**OCAP off**). Each lane is a per-model monotonic ratchet: a score never goes
down across releases.

<!-- BENCH-SCOREBOARD:START -->
_Per-model Terminal-Bench champions, **OCAP off vs on**. Each lane is a monotonic ratchet (a score never goes down). Models measured on both lanes only; half-measured and unrun models are in the full table. Auto-generated; do not edit by hand._

| Model | OCAP off | OCAP on |
|-------|----------|---------|
| `deepseek-v4-pro`<br><sub>deepseek · tb-30 · ctx 65536 · v0.8.0 · 2026-08-06</sub> | 56.7% (17/30) | 50.0% (15/30) |
| `nemotron-3-super`<br><sub>nemotron · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 36.7% (11/30) | 26.7% (8/30) |
| `qwen3.6_35b`<br><sub>qwen · tb-30 · ctx 65536 · v0.7.6 · 2026-07-29</sub> | 20.0% (6/30) | 26.7% (8/30) |
| `o4-mini`<br><sub>openai · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 13.3% (4/30) | 16.7% (5/30) |
| `qwen3-coder_30b`<br><sub>qwen · tb-30 · ctx 65536 · v0.7.6 · 2026-07-29</sub> | 10.0% (3/30) | 13.3% (4/30) |

<!-- BENCH-SCOREBOARD:END -->

Evidence, provenance, rejected runs, and the scoring rules live in
[`gilamonster-bench`](https://github.com/Gilamonster-Foundation/gilamonster-bench/tree/main/scoreboard),
an instrument with no dependency on Newt. If the ruler shipped with the thing
it measures, one commit could move both at once.

## Use Newt

```bash
git clone https://github.com/Gilamonster-Foundation/newt-agent
cd newt-agent
just install   # → ~/bin/newt, ~/bin/newt-mcp-server
newt           # first run opens the setup wizard, then the TUI coder
```

`newt --help` is the authority on the binary's surface; this file is not.
The HTMX web cockpit is a separately built binary: `just install-web`, then
`newt web` ([`newt-web/`](./newt-web/README.md)).

## Why Newt

A *harness* helps the model work; a **bridle** lets the operator steer, and
prove afterwards exactly where the horse went. Newt is an experiment in making
Object Capability (OCAP) security pragmatic inside an agent loop, as a reusable
component ([`agent-bridle`](https://github.com/Gilamonster-Foundation/agent-bridle))
other harnesses can adopt. Who acted, on what, under whose grant, and whether
*only* what was permitted happened then become properties of the system, not
promises in a policy document. Long form: [`docs/vision.md`](./docs/vision.md).

## Design laws

Seven invariants, each argued in a decision record:
[`docs/design-laws.md`](./docs/design-laws.md). Local-first inference;
fail-closed OCAP; small crates, zero warnings, coverage-gated; patch, not
prose; skills are on-demand context; issues are ground truth; causal
ordering, not wall-clock.

## Where things live

| What | Where |
|---|---|
| Setup, discovery, credentials | [`docs/guide/setup.md`](./docs/guide/setup.md) |
| Hosted providers and Hermes import | [`docs/provider-presets.md`](./docs/provider-presets.md) |
| Terminal UI and its walkthroughs | [`newt-tui/README.md`](./newt-tui/README.md), [`demos/README.md`](./demos/README.md) |
| Python bindings | [`newt-agent-py/README.md`](./newt-agent-py/README.md) |
| Cloud-provider plugin protocol | [`plugins-protocol/`](./plugins-protocol/README.md) |
| Smart harness (adjudication, frames, forensics) | [`docs/guide/smart-harness.md`](./docs/guide/smart-harness.md) |
| Derivation kernel and recorded sessions | [`agent-frame/`](./agent-frame/README.md), [`agent-harness/`](./agent-harness/README.md), [`agent-harness-py/`](./agent-harness-py/README.md) |
| Evaluation harness | [`newt-eval/README.md`](./newt-eval/README.md) |
| Terminal-Bench runner | [`scripts/eval/harbor/README.md`](./scripts/eval/harbor/README.md) |
| Benchmark evidence | [`gilamonster-bench`](https://github.com/Gilamonster-Foundation/gilamonster-bench/tree/main/scoreboard) |
| What changed | [`CHANGELOG.md`](./CHANGELOG.md) |
| Forward plan | [`ROADMAP.md`](./ROADMAP.md) (issue state is authoritative) |
| Design docs, decisions, field notes | [`docs/design/`](./docs/design/), [`docs/decisions/`](./docs/decisions/), [`docs/notes/`](./docs/notes/README.md) |
| Local gate | `just check` (see [`justfile`](./justfile)) |

## License

Apache-2.0. See [LICENSE](./LICENSE).

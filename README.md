# Newt-Agent

<p align="center">
  <img src="docs/logos/newt-agent-logo_source.png" alt="Newt-Agent logo" width="256" />
</p>

> **Experimental agentic coder**, written in Rust. Local-first: the default
> build ships no cloud provider. The claim is measured: see the
> [Terminal-Bench scoreboard](./docs/terminal-bench.md).

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
| Terminal-Bench scoreboard, runner, evidence | [`docs/terminal-bench.md`](./docs/terminal-bench.md) |
| What changed | [`CHANGELOG.md`](./CHANGELOG.md) |
| Forward plan | [`ROADMAP.md`](./ROADMAP.md) (issue state is authoritative) |
| Design docs, decisions, field notes | [`docs/design/`](./docs/design/), [`docs/decisions/`](./docs/decisions/), [`docs/notes/`](./docs/notes/README.md) |
| Local gate | `just check` (see [`justfile`](./justfile)) |

## License

Apache-2.0. See [LICENSE](./LICENSE).

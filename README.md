# Newt-Agent

<p align="center">
  <img src="docs/logos/newt-agent-logo_source.png" alt="Newt-Agent logo" width="256" />
</p>

Newt is an experimental agentic coder in Rust, built for local models first.
The default build contains no cloud provider; hosted providers are opt-in
plugins added at build time.

The operator grants each tool call a capability: what it may read, write,
run, and reach on the network. The operator can narrow that grant before a run
and audit it after. [`agent-bridle`](https://github.com/Gilamonster-Foundation/agent-bridle)
enforces it, in the kernel where the OS allows.

Confinement has a cost, so we measure it rather than describe it. We score each
model on a fixed Terminal-Bench task set, with the grant enforced and without,
and no release may lower a score: [Terminal-Bench scoreboard](./docs/terminal-bench.md).

## Install

```bash
git clone https://github.com/Gilamonster-Foundation/newt-agent
cd newt-agent
just install
newt
```

The first run opens a setup wizard. `newt --help` is the authority on what the
binary does; this file is not.

## Read next

| Topic | Where |
|---|---|
| Why Newt exists | [`docs/vision.md`](./docs/vision.md) |
| The invariants it keeps | [`docs/design-laws.md`](./docs/design-laws.md) |
| Setup and backends | [`docs/guide/setup.md`](./docs/guide/setup.md) |
| Benchmarks | [`docs/terminal-bench.md`](./docs/terminal-bench.md) |
| Decisions | [`docs/decisions/`](./docs/decisions/) |
| What changed | [`CHANGELOG.md`](./CHANGELOG.md) |
| Contributing | [`AGENTS.md`](./AGENTS.md) — `just check` runs the CI gates locally |

## License

Apache-2.0. See [LICENSE](./LICENSE).

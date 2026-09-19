# Newt-Agent

<p align="center">
  <img src="docs/logos/newt-agent-logo_source.png" alt="Newt-Agent logo" width="256" />
</p>

Newt is an experimental agentic coder, written in Rust, that is built to run
against local models first. The default build contains no cloud provider at all;
hosted providers are opt-in plugins that you add when you build it.

Every tool call that Newt makes runs under a capability that the operator
grants. The grant says what the call may read, write, run, and reach on the
network, and [`agent-bridle`](https://github.com/Gilamonster-Foundation/agent-bridle)
enforces it, in the kernel where the operating system allows it. Because the
grant is explicit, the operator can narrow it before a run and audit it after.

We measure Newt instead of describing it. Each model's score on a fixed
Terminal-Bench task set, both confined and unconfined, is recorded as a
ratchet that is not allowed to go down between releases. The current numbers
are on the [Terminal-Bench scoreboard](./docs/terminal-bench.md).

## Install

```bash
git clone https://github.com/Gilamonster-Foundation/newt-agent
cd newt-agent
just install
newt
```

The first run of `newt` opens a setup wizard. After that, `newt --help` lists
everything the binary can do, and it is the authority on that; this file is not.

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

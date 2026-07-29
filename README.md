# Newt-Agent

<p align="center">
  <img src="docs/logos/newt-agent-logo_source.png" alt="Newt-Agent logo" width="256" />
</p>

> Free, friendly, local agentic coder.
> **vi to Hermes-Agent's emacs.**

A single Rust binary with a sharp, minimal tool set. It runs against your
local hardware by default — no cloud bytes leave your machine unless you
deliberately install a provider plugin. Opinionated, not extensible.

## Why — the bridle, not just the harness

An agent *harness* helps the model do work; a **bridle** lets the operator
*steer* — and prove, after the fact, exactly where the horse went. Newt is an
experiment in making Object Capability (OCAP) security — long considered
theoretically correct but practically unimplementable — pragmatic inside an
agent loop, as a reusable concept
([`agent-bridle`](https://github.com/Gilamonster-Foundation/agent-bridle))
intended to be pluggable into other harnesses, not just this one.

OCAP's algebraic construction means some questions are answered
*structurally*, not by audit-log archaeology:

- Who acted on what, and when?
- Who granted the authority for this to do that?
- Did what they permitted actually happen — and did **only** what they
  permitted happen?

For anyone whose work lives on provenance, authority, integrity, and data
sovereignty — lawyers, clinicians, data scientists — those answers have to be
properties of the system, not promises in a policy document.

If it doesn't find its day in the sun, it was fun anyway.

## Quick start

```bash
git clone https://github.com/Gilamonster-Foundation/newt-agent
cd newt-agent
just install          # release binaries → ~/bin/newt, ~/bin/newt-mcp-server
newt setup dgx1.home.lab  # probe configured ports and select detected inference
newt code             # TUI coder in the current directory
```

Bare setup hosts are probed anonymously across the configured discovery ports
(including 8000 and 8080 by default). For an authenticated endpoint, use its
exact HTTPS URL and store only a secret reference:

```bash
newt setup https://inference.example.net:8000 --token-env INFERENCE_TOKEN
newt setup https://inference.example.net:8080 --token-file ~/.config/newt/token
```

Detected endpoints are stored as `~/.newt/backends/*.toml`; the main
`~/.newt/config.toml` only records the selected `default_backend`.

### Tool output scrollback

Completed tool results use a bounded, tail-biased spill (`[tui]
spill_lines = 3` by default). On Unix, the default interactive build also
streams an active shell tool into that bounded frame when both stdin and stdout
are terminals and `TERM` is not `dumb`. Up/Down scroll retained lines; Space or
Enter toggles `⧉` expand and `▣` collapse. Expansion never exceeds safe terminal
capacity, and each tool still commits one canonical completed block to normal
terminal scrollback.

Use `/spill <N>` for a session-only row count, `/spill reset` to restore
configuration, or `/spill 0` for unbounded completed output with live display
disabled. The default `newt` binary enables `live-spill`; a
`--no-default-features` build strips it. See the
[TUI README](./newt-tui/README.md) and
[decision record](./docs/decisions/live_spill_viewport.md).

### Operating modes and permission postures

Inside the TUI, `/mode` lists and selects a working style: `chat`, `dev`,
`admin`, `plan`, `diagnose`, `auto`, or `full-auto`. Bare `/mode` includes a
description of each. `plan` is workspace-read-only with access to Newt's plan
ledger, and `diagnose` is bounded read-only research. In `auto`, the model can
select a bounded style for a later action-shaped turn, but protected intake
still wins and only the human can select `full-auto`. Every mode still honors
all permission and safety boundaries. `dev` and `full-auto` both carry TDD,
worktree-safe Git, targeted-test, and full-preflight guidance; `full-auto`
changes persistence and interruption policy, not authority.

`/posture` is the separate authority control. It lists or applies a configured
skill/framing binding and its optional permission floor; `/posture off` clears
it. A configured floor can only narrow authority, while a posture without one
leaves authority unchanged. Existing posture bindings remain under
`[modes.<name>]` in TOML for compatibility. See the
[mode/posture decision](./docs/decisions/operating_modes_and_permission_postures.md).

Run `newt --help` for every mode (worker, MCP server, doctor, config, …) —
the binary is the authority on its own surface, this file is not. Python
bindings live in [`newt-agent-py/`](./newt-agent-py/) (`pip install
newt-agent-py`, import path `newt_agent`).

## Terminal-Bench scoreboard

newt is measured on [Terminal-Bench](https://github.com/harbor-framework/terminal-bench)
via `newt solve` (headless) + the Harbor adapter. The release gate is a
**per-model monotonic ratchet** — a model's score never goes down across
releases; we establish a starting number and keep beating it. The table below is
published from `scripts/eval/bench-results.jsonl` every release
(`scripts/eval/bench_scoreboard.py render`).

<!-- BENCH-SCOREBOARD:START -->
_Per-model **champion** scores — the release gate is a monotonic ratchet: a model's score never goes down. Auto-generated; do not edit by hand._

| Model | Family | Score | Passed | Suite | Window | Version | Date |
|-------|--------|-------|--------|-------|--------|---------|------|
| `qwen3.6_35b` | qwen | 20.0% | 6/30 | tb-30 | 65536 | 0.7.5 | 2026-07-28 |
| `qwen3-coder_30b` | qwen | 10.0% | 3/30 | tb-30 | 65536 | 0.7.5 | 2026-07-28 |
| `ornith_35b` | ornith | _queued_ | — | tb-30 | — | — | — |
| `glm-4.7-flash` | glm | _queued_ | — | tb-30 | — | — | — |
| `deepseek-coder-v2_16b` | deepseek | _queued_ | — | tb-30 | — | — | — |
| `gemma4_31b` | gemma | _queued_ | — | tb-30 | — | — | — |
| `kimi-dev_72b` | kimi | _queued_ | — | tb-30 | — | — | — |
| `nemotron-3-nano_30b` | nemotron | _queued_ | — | tb-30 | — | — | — |

<!-- BENCH-SCOREBOARD:END -->

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
  model's summary of itself.
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

The durable output of this experiment is what building it teaches about how
LLMs behave inside a harness:

- **[Summarization-induced hallucination](./docs/notes/2026-06-13-summarization-induced-hallucination.md)**
  — context compression that *summarizes* a session can make the model
  hallucinate APIs it had already read. A confident summary is worse than a
  labelled absence: absence routes the model to re-read; a summary suppresses
  recovery.
- **[Truncation honesty](./docs/testing/results/context-baseline-f0f4f6e.md)**
  — silent context truncation yields *silently wrong* answers; every fix
  moves the failure, it doesn't always remove it.
- **[Coder-driving sweet spots](./docs/notes/2026-05-31-newt-coder-driving-sweet-spots.md)**
  — where small local models are and aren't reliable at agentic coding.
- **[Hermes learnings](./docs/design/context-memory-hermes-learnings.md)**
  — take the algorithms, refuse the architecture.

## Where things live

| What | Where |
|---|---|
| Forward plan | [`ROADMAP.md`](./ROADMAP.md) (issue numbers are the live state) |
| Release history | [`CHANGELOG.md`](./CHANGELOG.md) |
| Design docs & studies | [`docs/design/`](./docs/design/) |
| Decision records | [`docs/decisions/`](./docs/decisions/) |
| Evaluation harness | [`newt-eval/README.md`](./newt-eval/README.md) |
| Local gate | `just check` (see [`justfile`](./justfile)) |

## License

Apache-2.0. See [LICENSE](./LICENSE).

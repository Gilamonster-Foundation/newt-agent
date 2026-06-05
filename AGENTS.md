# Newt-Agent — Agent Instructions (general)

Mirror of `CLAUDE.md` for non-Claude agents (Codex, Gemini, Hermes,
local Ollama models, etc.). When the two files disagree, CLAUDE.md
is canonical for Claude sessions and this file is canonical for
everyone else.

## What this repo is

Newt-Agent is a Rust workspace prototype for a local-first coding
agent. It's also the **drake-swarm training ground** — every PR
will be reviewed by an arbiter LLM voting against the CI gate.
The gates must be honest. Do not game them.

## Acceptance contract

A PR is only complete when all of the following are green:

- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- `just cov-ci` (workspace coverage ≥ the current floor)

Full text in `docs/ROADMAP.md` under "Acceptance contract".

## Build + test

```bash
just check         # fmt + clippy + test (local CI gate)
just cov-ci        # coverage with the gate floor
just install-hooks # wire .githooks/ as core.hooksPath
```

## Branch + PR policy

- **Never push to `main`.** Use feature branches; open PRs.
- One roadmap step per PR. Branch name: `step-NN.M-short-kebab-name`.
- PR body must include "What this PR does", "Test plan", "Out of scope".
- The pre-push hook runs `just check` + `just cov-ci`. Don't bypass it.

## Model attribution

- If an LLM materially contributes to a commit, identify it with a
  `Co-authored-by` trailer in the commit message.
- Use the model/tool identity the session is actually running under. Do not
  credit a generic "AI Assistant".
- Known trailers:
  - `Co-authored-by: Codex <codex@openai.com>`
  - `Co-authored-by: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- If multiple LLMs contribute to the same commit, include one trailer per
  contributing model.

## Coverage floor

Ratchets up, never down. Bootstrap is 15% (current scaffold). Target
is 80% per the roadmap.

## When in doubt

Read `docs/ROADMAP.md`. If a step's "Out of scope" says no, it means
no. Ask the human before opening a PR if you can't tell which step a
change belongs to.

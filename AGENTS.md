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

## Crate README Rule

Every crate in this workspace gets its own `README.md` — crates.io renders
it as the crate's front page, and `cargo package` fails if a declared
`readme` file is missing.

1. **Existence:** a new crate lands with a `README.md` in its crate root
   (short: what it is, what it does, license).
2. **Freshness:** every version bump of a crate includes a review of that
   crate's README. Update it to match the released behavior — new features,
   changed CLI flags, removed APIs. If a bump PR leaves the README
   untouched, the PR body must say why.

Treat a version bump without a README review as an incomplete change, the
same way a bug fix without a regression test is incomplete.

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

## Ground truth — verify every action

After every action, confirm what actually happened. Do not proceed on
assumptions. Beliefs are not ground truth; tool results are.

| Belief | Ground-truth check |
|---|---|
| "I wrote the file" | Tool reports line count — does it match intent? |
| "The code compiles" | Build check runs automatically after file writes (if configured) |
| "I'm on branch X" | `git branch --show-current` |
| "My test passes" | `cargo test -p <crate> <test_name>` |
| "I committed" | `git log --oneline -1` |
| "The edit applied" | `edit_file` returns new line count — verify it |

**Never commit if any of the above are uncertain.**

## File editing rules

- **Prefer `edit_file` over `write_file`** for any existing file.
  You only generate the change, not the whole 4,000-line file.
  Regenerating a large file from memory is where hallucination strikes.
- `write_file` has a shrink guard: refuses if the proposed write removes
  more than 30% of lines. This exists because of an observed failure
  where a model replaced a 4,247-line file with 107 lines.
- After `write_file` or `edit_file`, read the returned line count.

## Reuse discipline — search, adapt, minimize

Before writing new code, in order:

1. **Search for existing code.** Grep the workspace for the concept. Do
   not add a second implementation of something that already exists.
2. **TDD-adapt what exists.** Write the failing test for the new case
   against the *existing* abstraction, then widen it — do not stand a
   parallel one up beside it. (The cycle itself is below.)
3. **Refactor to the fewest lines that still pass the tests.**
   Fewest-lines is the success metric, not merely "it works".

**Why:** sprawl breeds whack-a-mole bug classes. Measured here before the
`newt_core::tty` line arbiter: 5 spinner implementations, 3 copies of one
frame array, 4 erase strategies, `\r\x1b[K` at 6 sites across 2 crates, 3
animation clocks, 4 different "may I draw?" predicates — which produced a
real user-visible hang (a permission prompt drawn invisibly under a spinner
that overwrote it ~8×/second). Tracked in #1312.

**Prefer making a bug unrepresentable over fixing each site.** `gate.ask`
has six call sites; one was safe only by call-ordering luck. Use types,
RAII, and required parameters so the broken call does not compile.

If a second implementation is truly warranted, say so in the PR and explain
what the existing abstraction could not be widened to cover.

## TDD discipline

1. Write the failing test first. Verify it fails.
2. Write the minimum code to make it pass.
3. Verify it passes (`cargo test`).
4. Run `just check` — zero warnings, all tests green.
5. Commit.

## Auto build-check (recommended)

Add to `.newt/config.toml` in this workspace to enable automatic
`cargo check` after every file write — the model sees the build result
inline without needing to ask:

```toml
[tui]
build_check_cmd = "cargo check -q --workspace"
```

## When in doubt

Read `docs/ROADMAP.md`. If a step's "Out of scope" says no, it means
no. Ask the human before opening a PR if you can't tell which step a
change belongs to.

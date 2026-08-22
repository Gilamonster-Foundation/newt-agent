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

Newt mechanically maintains a **multi-contributor attribution ledger**
(`newt_core::attribution::AttributionLedger`, #1707/#1709) for every commit
— not a single "current model" field. Every AI model/harness pair that
materially contributed to a commit gets its OWN trailer:

```
Co-authored-by: <MODEL> (<HARNESS> v<HARNESS-VERSION>) <HARNESS-EMAIL>
```

**The harness version is part of the identity, not decoration.** The same
model under `v0.7.6` and under `v0.8.0` is two distinct contributors, because
what ran the work differs. `Attribution::harness_version` is captured at
contribution time and rendered inside the `(<harness> v<version>)` qualifier
by `Attribution::trailer` / `CommitAttribution::model_trailer`.

**For newt-agent the version carries the commit it was built from.** A
package version alone does not identify a build — every dev build between two
releases claims the same `0.8.0`. So newt's version is
`build_info::VERSION_WITH_COMMIT`: the semver plus the 12-character git
commit, e.g. `0.8.0 (a3f9c21b4d5e)`. In a trailer that renders flat, without
nesting a second set of parentheses:

```
Co-authored-by: <MODEL> (v0.8.0 a3f9c21b4d5e) <309460085+newt-agent@users.noreply.github.com>
```

A build from a modified worktree is `-dirty`-suffixed, per
`build_info::SOURCE_ID` — an honest signal that the harness that ran the work
is not any committed revision. A foreign harness uses whatever version it
publishes (`Claude Code v2.1.239`) and appends no commit unless it reports
one; it never borrows newt's.

**The email identifies the harness, not the model.** It is a real account
that receives the credit, so a harness stamps its OWN address and never
another's. Newt's address belongs to newt's own machinery; a Claude Code or
Codex session working in this repo is a guest and signs as itself.

| Harness | Attribution email |
|---|---|
| `newt-agent`, `newt-agent crew` — this repo's embedded `git` tool | `309460085+newt-agent@users.noreply.github.com` (`agent_identity::DEFAULT_AGENT_EMAIL`) |
| `Claude Code` | `noreply@anthropic.com` |
| `Codex CLI` | `codex@openai.com` |
| anything else | that harness's own documented trailer — if it documents none, ask rather than invent one |

An `agent-identity.toml` email overrides the newt row only. That file is
newt's identity, and it cannot reassign another harness's address.

Example — a newt session that moved through four distinct model/harness
pairs before landing one commit, each credited to the harness that ran it,
under the operator who ran the session:

```
Harness: newt-agent v0.8.0 (a3f9c21b4d5e) | Model: GPT-5.6 Sol | Operator: Shawn Hartsock

Co-authored-by: Shawn Hartsock <33919+hartsock@users.noreply.github.com>
Co-authored-by: GPT-5.6 Sol (v0.8.0 a3f9c21b4d5e) <309460085+newt-agent@users.noreply.github.com>
Co-authored-by: Claude Opus 4.8 (Claude Code v2.1.239) <noreply@anthropic.com>
Co-authored-by: GPT-5.3-Codex (Codex CLI v0.47.0) <codex@openai.com>
Co-authored-by: Nemotron (crew v0.8.0 a3f9c21b4d5e) <309460085+newt-agent@users.noreply.github.com>
```

The first and last lines say `(v0.8.0 a3f9c21b4d5e)` and
`(crew v0.8.0 a3f9c21b4d5e)`, not `(newt-agent v0.8.0 …)` — **the harness
name is omitted when the email already names it.** `<…+newt-agent@…>` identifies the harness, so repeating
`newt-agent` in the qualifier is noise; what remains is only what the address
does not already tell you, which for `newt-agent crew` is `crew`. A foreign
harness keeps its full name, because its address does not spell it out.

**The human operator gets a by-line too, read from git.** The models are
co-authors, not the only ones — the person who ran the session is credited
under their OWN name and email, never the agent account. That pair comes from
the host git identity, `user.name` + `user.email`, via
`agent_identity::host_operator_identity`, with an explicitly configured
`operator` / `operator_email` in `agent-identity.toml` winning when both are
set. `CommitAttribution::operator_trailer` renders it:

```
Co-authored-by: Shawn Hartsock <33919+hartsock@users.noreply.github.com>
```

Two rules hold it honest. **Name and email move as a matched pair** — a
configured name is never welded onto a host email it has nothing to do with.
And **an operator email is never invented**: when no real one is known the
by-line is omitted entirely rather than manufactured, so a missing operator
credit means "unknown", not "nobody". Operator attribution is independent of
model attribution; neither substitutes for the other, and a commit carries
both.

Rules:

- **Identify model AND harness**, e.g. `GPT-5.3-Codex (Codex CLI v0.47.0)`,
  not just the model name. Never a generic "AI Assistant". Under newt's own
  address the harness half collapses to the version, per the omission rule
  above.
- **One trailer per contributing model/harness pair, unlimited count.** A
  `/model`, `/backend`, loadout, crew, or delegation switch mid-session ADDS
  a contributor; it never discards one already accumulated for the pending
  commit. The same model through two different harnesses (e.g. `Model A
  (newt-agent)` vs `Model A (Codex)`) is two distinct contributors.
- **Deduplicate identical `(model, harness, email)` identities**, preserving
  first-contribution order — do not list the same contributor three times
  because it made three writes.
- **The email travels with the harness, not with the ledger.** Trailers on
  one commit do NOT all share one address.
  `AttributionLedger::record` stamps the ledger's default email and is for
  newt's own contributions; a contributor that ran under a *foreign* harness
  goes in via `AttributionLedger::add` with an `Attribution` carrying that
  harness's address — `Attribution::email` is per-contributor for exactly
  this reason.
- **Ask rather than reconstruct it: `/byline`.** It prints the exact block
  the next commit would carry — every accumulated contributor, the active
  model, the operator by-line, the provenance line — rendered by the same
  finalizer the commit path runs, so it cannot show a shape a commit would
  not produce. `newt identity` prints the same preview for the configured
  identity. Read it off one of those instead of assembling a trailer from
  this section by hand.
- **This is mechanical, not a model instruction.** The embedded `git` tool
  stamps the ledger's accumulated trailers itself; do not hand-write
  `Co-authored-by` lines yourself when using it — see the per-turn "Git
  commit identity" guidance the harness already gives you. If you must shell
  out to `git` directly (bypassing the embedded tool), you get no automatic
  multi-contributor credit at all — prefer the embedded tool.
- **If you are not the newt harness, this section is not yours to imitate.**
  It documents what newt stamps about itself. A Claude Code, Codex, or other
  foreign session commits with the trailer ITS OWN harness prescribes and the
  matching address from the table above. Reading these rules and hand-writing
  a newt-style trailer signs newt's account for work newt did not do.

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

**The same doctrine applies to tests.** A mocked test is a *belief* — it
encodes what we think the real filesystem, terminal, or subprocess does, and
nothing in the mocked tier can tell you that belief is wrong. A real-resource
test (PTY, real subprocess, real fs) is the **ground truth that verifies it**.
Mocked stays the gate — fast, deterministic, every PR — and a real-resource
test is an **add-on that proves the gate measures reality**, not a deviation
from "fully mocked". When you add one, record in its doc comment which mocked
behavior it grounds; a real test that grounds nothing is just a slow test.

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

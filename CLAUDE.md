# Newt-Agent — Agent Instructions (Claude)

This file is loaded by Claude Code on every session in this repository.
Read it once at session start; the constraints below apply for the rest
of the session unless you have an explicit human authorization to deviate.

## What this repo is

Newt-Agent is a Rust workspace prototype for a local-first coding agent.
Long-term it's also the **drake-swarm training ground** — every PR
review here will be done by an arbiter LLM voting against a real CI
gate. The gates must be honest. Do not game them.

## Architectural style — the three Cs

Knowledge belongs in **data**, not logic. Prefer **Composition,
Configuration, and Convention** over hardcoded lists and constants:
language- or domain-specific knowledge — keyword lists, magic constants,
recognition rules — should be pure data that is *composed*, *configured*
(droppable / overridable), and *convention-driven*, so a new language or
domain is **config, not code**. The canonical example is the
**language-pack** model (`newt-core/src/api_surface.rs`: pure-data
`LanguagePack`, merge-by-name, droppable `.toml`) and its sibling lexicon
for prompt/domain phrase lists.

**But: working code over all.** Functional results come first. It is
fine — expected — to compromise to a hardcoded / simple implementation to
*get a feature working*. Then **return to the three Cs**: refactor the
hardcoded values into pure-data config, composition seams, and conventions
once it works. Do not let the three Cs block shipping a working result; do
circle back and de-hardcode. When you spot a hardcoded list that encodes
language or domain knowledge, flag it as a three-Cs refactor candidate.

## Where the rules live

- **Acceptance contract for every PR:** `docs/ROADMAP.md` (top
  section). Every PR must clear all of: `cargo build --workspace`,
  `cargo test --workspace`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo fmt --all -- --check`, and the coverage
  floor in `just cov-ci`.
- **Roadmap steps:** `docs/ROADMAP.md` (Steps 0.1 → 12.x). Each step
  is sized for one drake flight / one focused PR.
- **Acceptable PR shape:** branch name `step-NN.M-short-kebab-name`,
  body must list "What this PR does" / "Test plan" / "Out of scope".
- **TUI scope:** `docs/decisions/plain_scroller_tui.md` — newt is
  amphibious (human CLI + headless swarm) and the chat surface is
  deliberately a plain scroller. Do NOT add alternate-screen, ratatui,
  or widget surfaces to the chat path; advanced TUI belongs in
  gilamonster-agent / monitor repos, and the headless flight tier
  (wyvern-agent) strips the TUI entirely.

## Build commands

```bash
just check          # fmt + clippy + test (the local CI gate)
just test           # cargo test --workspace
just cov            # local coverage with HTML report
just cov-ci         # CI-mode coverage with the gate floor
just install-hooks  # wire .githooks/ as core.hooksPath
```

After cloning: run `just install-hooks` then `just check` and confirm
green before opening any PR.

## Branch + PR policy

- **Never push to `main`.** Open a PR from `step-NN.M-…` or
  `feat/…` / `fix/…`. CI runs on PRs to main and pushes to those
  branches.
- The pre-push hook runs `just check` + `just cov-ci`. **TEMPORARY EXCEPTION
  (until #1098):** that hook is ~50 min (full-workspace clippy + test + a second
  `--no-default-features` config + an instrumented whole-workspace `cov-ci`),
  which is inhumane on every push — so **`--no-verify` is permitted for now.**
  The real gate is CI on the PR plus the standing "no merge to `main` without a
  green PR." Use `--no-verify` only to skip the slow gate, never to hide a
  failure you could fix quickly (run `just check` locally when you can). When
  #1098 lands a fast, changed-code-only hook, delete this exception —
  `--no-verify` is forbidden again. If a check fails, fix the issue.
- One step per PR. Don't bundle "Step 0.2 + 0.3 because they're
  related" unless the bundle is itself explicitly authorized.
- The PR body must include "What this PR does", "Test plan", and
  "Out of scope" sections — per the roadmap's acceptance contract.

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

## Coverage gate

Workspace coverage is enforced by `just cov-ci` and the matching
job in `.github/workflows/ci.yml`. The floor **ratchets up, never
down** — if your PR drops coverage below the floor, raise the
coverage; don't lower the floor.

Bootstrap floor: 15% → ratcheted to 75% in the stdio-safety PR →
ratcheted to 80% in the tuning-writeback PR, reaching the roadmap
acceptance contract's target floor.

## Editor / shell preferences

- Editor: vi (no emacs).
- Test mocking: `wiremock` for HTTP, in-memory fakes / injected fs seams
  for filesystem logic, `mockall` for traits, `assert_cmd` + `predicates`
  for CLI binaries, `tokio-test` for async. **The unit tier is fully
  mocked** — see "Testing strategy" below. See Step 0.4 in the roadmap for
  the shared `tests/common` helper crate.

## Testing strategy

newt's tests run in tiers. Default to the cheapest tier that proves the
behavior; reserve expensive tiers for what only they can catch.

- **Unit + regression tier — FULLY MOCKED, ALWAYS (every PR).** No real
  network, filesystem, subprocess, or wall-clock — *ever*. HTTP →
  `wiremock`; traits → `mockall`; filesystem → in-memory data / fakes /
  injected fs seams (never `tempfile` / `TempDir` / `std::fs::write` /
  `create_dir` in a unit test); CLI → `assert_cmd` against mocked
  dependencies; time/async → injected clock / `tokio-test`. These are fast,
  deterministic, and parallel-safe, and they gate every PR. Pattern to
  copy: `newt-cli/src/dgx_pull.rs` — pure, fully mocked, fs-free.

- **BAT / UAT regression pipelines — simulated systems-integration env.**
  Write **Basic Acceptance Tests (BAT)** and **User Acceptance Tests (UAT)**
  that replay real-world scenarios against a *simulated* integration
  environment — mocked/stubbed external systems standing in for the real
  ones, **not** live production systems. BAT = smoke / contract-level "does
  the wired-up system accept the basic flows"; UAT = end-user scenarios
  phrased the way a user would actually exercise them. These are the durable
  acceptance story and guard against regressions in real-world behavior.

- **End-to-end + real integration tests — EXPENSIVE → weekly + release
  gates only.** Anything touching a real filesystem, real network, real
  subprocess, or a live/standalone service is costly and flaky under load.
  Run it on the **weekly** schedule and on **release gates**, never in the
  per-PR unit run. **Run these single-threaded**
  (`cargo test -- --test-threads=1`, or `#[serial]` via `serial_test`):
  real-resource tests contend, and under parallel load intermittently fail
  with `Permission denied (os error 13)` on tempdir creation, aborting the
  whole test binary. Never run them multi-threaded.

Migration of the existing real-fs (`tempfile`) tests out of the unit tier
is tracked in issue #514.

## Versioning

**Semver** (`0.MINOR.PATCH`). First crates.io release is **`0.6.0`**
(matching `agent-mesh` `0.6.0`); the earlier date-based scheme
(`0.{month}.{YYYYMMDD}`) is retired. The workspace package version is set
in the top-level `Cargo.toml` under `[workspace.package]`; all internal
crates inherit via `version.workspace = true`.

## When in doubt

- Read the roadmap. If a step's "Out of scope" section says no, it
  means no.
- If you can't figure out which step a change belongs to, ask the
  human before opening a PR.
- Never disable a clippy lint or skip a test to get green. If a
  lint or test is wrong, fix it in a separate PR with explicit
  authorization.

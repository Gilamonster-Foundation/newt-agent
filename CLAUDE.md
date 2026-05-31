# Newt-Agent — Agent Instructions (Claude)

This file is loaded by Claude Code on every session in this repository.
Read it once at session start; the constraints below apply for the rest
of the session unless you have an explicit human authorization to deviate.

## What this repo is

Newt-Agent is a Rust workspace prototype for a local-first coding agent.
Long-term it's also the **drake-swarm training ground** — every PR
review here will be done by an arbiter LLM voting against a real CI
gate. The gates must be honest. Do not game them.

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
- The pre-push hook runs `just check` + `just cov-ci`. Do not bypass
  it with `--no-verify`. If a check fails, fix the issue.
- One step per PR. Don't bundle "Step 0.2 + 0.3 because they're
  related" unless the bundle is itself explicitly authorized.
- The PR body must include "What this PR does", "Test plan", and
  "Out of scope" sections — per the roadmap's acceptance contract.

## Coverage gate

Workspace coverage is enforced by `just cov-ci` and the matching
job in `.github/workflows/ci.yml`. The floor **ratchets up, never
down** — if your PR drops coverage below the floor, raise the
coverage; don't lower the floor.

Bootstrap floor: 15% → ratcheted to 75% in the stdio-safety PR
(workspace coverage is ~89.78%).
Target floor: 80% (per the roadmap acceptance contract).

## Editor / shell preferences

- Editor: vi (no emacs).
- Test mocking: `wiremock` for HTTP, `tempfile` for fs, `mockall`
  for traits, `assert_cmd` + `predicates` for CLI binaries,
  `tokio-test` for async. See Step 0.4 in the roadmap for the
  shared `tests/common` helper crate.

## Versioning

**Semver** (`0.MINOR.PATCH`). First crates.io release is **`0.1.0`**
(matching `agent-mesh` `0.1.0`); the earlier date-based scheme
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

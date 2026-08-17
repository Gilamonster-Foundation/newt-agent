# Newt-Agent — Agent Instructions (Claude)

This file is loaded by Claude Code on every session in this repository.
Read it once at session start; the constraints below apply for the rest
of the session unless you have an explicit human authorization to deviate.

## What this repo is

Newt-Agent is a Rust workspace prototype for a local-first coding agent.
Long-term it's also the **drake-swarm training ground** — every PR
review here will be done by an arbiter LLM voting against a real CI
gate. The gates must be honest. Do not game them.

**newt-agent is transitional.** See
`docs/decisions/agent_family_inheritance.md`. The agent family descends
**wyvern-agent, then newt-agent, then gilamonster-agent**, complexity
increasing. wyvern is the base: the barest working harness, OCAP, and no real
TUI, just a near-pure scroller whose lines read correctly under `journalctl`.
gilamonster is everything and the kitchen sink, with OCAP off by default.
Aspirationally wyvern ends up a **rewrite of newt-agent that is lighter,
faster and smaller**, and the others inherit from it, at which point
newt-agent's crates are retired in favour of rewrites.

The practical consequence for work here: **contracts survive a rewrite,
implementations do not.** Wire types, identity, ownership/provenance chains,
data formats and capability vocabulary are what a rewrite is written against,
so they repay the most care. De-duplicating an implementation pays back twice
downstream. Surface polish a descendant will rewrite is worth less. This makes
the three Cs and the reuse discipline below *more* load-bearing, not less.

## Architectural style — the three Cs

> Canonical home: the line's craft doctrine is the **Craft Register** —
> `steward-charter/docs/CRAFT.md`. The laws below (three Cs, reuse
> discipline, testing tiers, keep-files-small, one-issue-one-PR, zero
> warnings, hooks-mirror-pipelines) are this repo's operational statement of
> that register; when they drift, the register wins.

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

## Reuse discipline — search, adapt, minimize

The three Cs put *knowledge* in data. This puts *behavior* in one adapted
abstraction instead of a fork. In order, every time:

1. **Before writing new code, search for existing code.** Grep the
   workspace for the concept first. Do not add a second implementation of
   something that already exists.
2. **Use TDD to adapt existing code to the new case.** Write the failing
   test for the new case against the *existing* abstraction, then widen
   that abstraction — rather than standing a parallel one up beside it.
3. **Refactor toward the fewest lines that still pass the tests.**
   Fewest-lines is the success metric, not merely "it works".

**Why: sprawl is what breeds whack-a-mole bug classes.** This is measured,
not theoretical — the state of this repo's terminal code before the
`newt_core::tty` line arbiter: **5** independent spinner implementations,
**3** copies of the same 10-glyph frame array, **4** incompatible erase
strategies, `\r\x1b[K` open-coded at **6** sites across 2 crates, **3**
animation clocks, and **4** different predicates for "may I draw?". That
sprawl produced a user-visible hang — a permission prompt rendered
invisibly underneath a spinner that overwrote it ~8×/second — and `color`
silently overloaded from a *styling* signal into an *I/O-ownership* signal.
No single one of those was a hard problem; the missing shared owner was.
Tracked in #1312.

**Prefer making a bug unrepresentable over fixing each site.** When the
same defect can occur at N call sites, a per-site fix inherits the sprawl:
`gate.ask` has six call sites, and one of them was safe only by
call-ordering luck. Reach for types, RAII, and required parameters so the
broken call does not compile.

If a second implementation really is warranted, say so in the PR and
explain what the existing abstraction could not be widened to cover.

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
  amphibious (human CLI + headless swarm). The plain-scroller rule is
  **scoped to the LEAN (default) surface + the piped/headless/wyvern path**
  (2026-08-11 amendment): do NOT add alternate-screen, ratatui, panes, or
  widget surfaces to the LEAN chat path. The feature-gated, severable,
  TTY-gated **RichTUI** surface MAY host panes / a live dock overview.
  Advanced always-on TUI still belongs in gilamonster-agent / monitor repos,
  and the headless flight tier (wyvern-agent) strips the TUI entirely.

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

Newt mechanically maintains a **multi-contributor attribution ledger**
(`newt_core::attribution::AttributionLedger`, #1707/#1709) for every commit
— not a single "current model" field. Every AI model/harness pair that
materially contributed to a commit gets its OWN trailer:

```
Co-authored-by: <MODEL> (<HARNESS>) <EMAIL>
```

Example — a session that moved through four distinct model/harness pairs
before landing one commit:

```
Co-authored-by: GPT-5.6 Sol (newt-agent) <309460085+newt-agent@users.noreply.github.com>
Co-authored-by: Claude Opus 4.8 (Claude Code) <309460085+newt-agent@users.noreply.github.com>
Co-authored-by: GPT-5.3-Codex (Codex CLI) <309460085+newt-agent@users.noreply.github.com>
Co-authored-by: Nemotron (newt-agent crew) <309460085+newt-agent@users.noreply.github.com>
```

Rules:

- **Identify model AND harness**, e.g. `GPT-5.6 Sol (newt-agent)`, not just
  the model name. Never a generic "AI Assistant".
- **One trailer per contributing model/harness pair, unlimited count.** A
  `/model`, `/backend`, loadout, crew, or delegation switch mid-session ADDS
  a contributor; it never discards one already accumulated for the pending
  commit. The same model through two different harnesses (e.g. `Model A
  (newt-agent)` vs `Model A (Codex)`) is two distinct contributors.
- **Deduplicate identical `(model, harness, email)` identities**, preserving
  first-contribution order — do not list the same contributor three times
  because it made three writes.
- **Default attribution email:**
  `309460085+newt-agent@users.noreply.github.com` (the dedicated
  `github.com/newt-agent` account's noreply address). An explicitly
  configured `agent-identity.toml` email overrides this. Provider-specific
  emails (`codex@openai.com`, `noreply@anthropic.com`) are NOT required or
  used for automatic attribution — every trailer on one commit shares the
  same configured/default email; only the model and harness vary.
- **This is mechanical, not a model instruction.** The embedded `git` tool
  stamps the ledger's accumulated trailers itself; do not hand-write
  `Co-authored-by` lines yourself when using it — see the per-turn "Git
  commit identity" guidance the harness already gives you. If you must shell
  out to `git` directly (bypassing the embedded tool), you get no automatic
  multi-contributor credit at all — prefer the embedded tool.

### Expunge Claude sessions

**Never** put a Claude session URL — a `Claude-Session:` trailer or any
`https://claude.ai/code/session_…` link — in a commit message or a PR body.
It is agent-session plumbing, not repository provenance, and once pushed to a
public repo it is cloned/forked/cached forever. `Co-authored-by:` attribution
is welcome; the session link is not. The `.githooks/commit-msg` hook blocks it
mechanically (installed by `just install-hooks` via `core.hooksPath`).

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

**Why the expensive tier exists at all: it grounds the mocks.** A fully
mocked suite can be green against a fiction — a mock encodes what we
*believe* the real filesystem, terminal, or subprocess does, and nothing in
the unit tier can tell you that belief is wrong. Real-resource tests are the
**ground truth that verifies the mocks test something real**. The two tiers
are therefore not in tension, and a real-resource test is **not** a deviation
from "fully mocked": *mocked stays the gate — fast, deterministic, every PR —
and a real-resource test is an add-on that proves the gate is measuring
reality.* When you add one, record in its doc comment which mocked behavior
it grounds. A real test that grounds nothing is just a slow test.

Worked example: `prompt_visibility_test` drives a real PTY, because "the
prompt is visible" is a property of an actual terminal — no mock can observe
one writer scribbling over another's bytes. It grounds the line arbiter's
mocked lease/suspend unit tests.

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

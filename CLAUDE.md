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
`docs/decisions/agent_line_architecture.md`, which is the single canonical
statement of how the three agents relate. In capability terms
**wyvern ≤ newt ≤ gilamonster**. Today the three share no crate-level
dependency at all: newt and gilamonster sit on `agent-mesh-protocol` and
`agent-bridle`, gilamonster consumes newt by a pinned git rev, and wyvern is
isolated. The target is that wyvern becomes a small headless containerized
worker that newt or gilamonster dispatch OCAP caveats to, reached as a
rewrite of newt that is lighter, faster and smaller, after which newt's crates
are retired in favour of it.

Two rules follow, and the ADR is authoritative over this summary:

- Shared functionality moves *down* into the minimal layer. A lower layer never
  depends on a richer one.
- **Contracts survive a rewrite, implementations do not.** Wire and schema
  types, identity and provenance, config and capability vocabulary, observable
  behaviour, security invariants, conformance tests and compatibility fixtures
  are what a rewrite is written against, so they repay the most care. Surface
  polish a descendant will rewrite is worth less. This makes the three Cs and
  the reuse discipline below matter more, not less.

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

### Write less code — the burden is on the addition

The rules above say *search before you write*. This says something stronger:
**the best change is the one that adds no lines at all.**

Before adding a function, a module, a type, or a file, answer in the PR: what
existing thing did you try to compose or widen, and what could it not be made
to cover? "I needed one" is not the answer — it is what every one of the five
spinners also believed.

- A PR that **grows** the line count should say what that growth bought.
- A PR that **shrinks** it while holding behaviour needs no such justification.

This is not a prohibition on new code; features need code. It is a prohibition
on *unexamined* new code, and a statement about which direction the repo is
supposed to move when nobody is forcing it either way.

### Size is a smell, not a rule

No source file should exceed **5,000 lines**; most should sit near **2,000**.

These are thresholds for asking *"what is this file actually doing?"* — not
limits to be satisfied by splitting arbitrarily. **A file split along no seam
is worse than a large cohesive one**, because it hides the coupling instead of
removing it, and it costs a module header, an import block, and a re-export
list per new file. Decomposition *adds* lines; only deduplication removes
them. Split when a file has stopped having one reason to change, and expect
the line count to go **up** at that step and down at the next one.

## Content-addressable data structures — the concrete instance of reuse discipline

The rule above says "do not add a second implementation of something that
already exists." Here is the binding, named, so it cannot be missed:

**Every data structure that is persisted, transmitted, chained, or identified
takes its identity from `content-addressable` (v0.1.1). Hand-rolling a hash,
digest, id, or canonical encoding is a defect.**

| Use | Type |
|---|---|
| canonical **structured value** (record, event, manifest) | `ContentId` — CIDv1 / dag-cbor / BLAKE3 |
| opaque **byte string** (file, payload, tool result, cache key) | `RawContentId` — CIDv1 / raw / BLAKE3 |
| node with **causal parents** (chain or DAG link) | `MerkleNode<T>` — id over payload AND parents |
| **storing** addressed nodes | `NodeStore` |
| carrying a **foreign** CID without minting one | `ClassifiedCid` |

`ContentId` and `RawContentId` are different identities even with identical
digest bytes — the profile is semantic. One required method:
`canonical_form()` deferring to `to_canonical_dagcbor` on a `Serialize` type.

**Step zero for any new record type:** inventory what the crate mints and what
this repo already has (`grep -rn "blake3::hash(\|content_addressable\|SpillStore"`),
then invoke the `provenance-audit` skill, then design only the gap.

**This rule is enforced, not just stated.** Existing hand-rolled sites are
enumerated in a conformance ratchet whose count may only go DOWN. Prose was
not enough: on 2026-08-22 this rule existed in four places — the reuse
discipline above, the Authority Register's *content-addressed identity*, the
workspace AGENTS.md, and the `provenance-audit` skill — and three design rounds
(~70 review findings) still went into rebuilding a span store, a dag-cbor
scheme, and a Merkle DAG that already shipped in the dependency tree.

**Migration posture:** unadvertised project, no backwards compatibility owed.
Smash the bespoke format — one importer, then one encoding — rather than carry
a compatibility arm. Fix the mess by ratchet, never by rewrite.

## Where the rules live

> Canonical home for authority doctrine: the line's **Authority Register** —
> `steward-charter/docs/AUTHORITY.md`. Fail-closed, attenuate-never-amplify,
> amplification-needs-the-human-root, permissive-is-a-posture, one authority
> vocabulary, unsafe-state-unrepresentable, content-addressed identity,
> chain-plus-one-ref history, environmental-and-in-process confinement, and
> observable authority decisions are stated there once. A document in this repo
> **cites the law it relies on and does not re-argue it**; where this repo
> deviates, state the deviation against the named law (the live register is
> `docs/security/ocap-deviations.md`). When the register and a document here
> disagree, the register wins and the document is the thing to fix.

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
  **Migration notice (2026-08-17):** the LeanTUI *input surface*
  (`newt-tui/src/lean_input.rs`) is scheduled to move to wyvern-agent, after
  which newt-agent is **RichTUI-only for interactive use**. Lean/rich feature
  parity is therefore **not** required, and rich-only panel work needs no lean
  twin. This does NOT retire the plain-scroller *output* contract: committed
  output and the piped/headless path still obey it, because `newt solve` when
  piped, `newt-acp-worker`, the eval harness, and newt-as-a-wyvern-worker all
  depend on running off a TTY.

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

### What must never leave the machine

Attribution is welcome. **Agent-session plumbing and private data are not.**
The two are easy to confuse because a harness often offers them together in
one block — a `Co-authored-by:` line and a session URL, side by side. Keep the
first, drop the second, and never paste such a block whole.

**Never** put any of the following in a commit message, a PR title or body, an
issue, a code comment, a released artifact, or any other text that leaves this
machine:

- **A harness session URL or id** — a `Claude-Session:` trailer, any
  `https://claude.ai/code/session_…` link, or the equivalent from any other
  harness. It is agent-session plumbing, not repository provenance, and
  **this repo is public**: once pushed it is cloned, forked and cached
  forever, so a later deletion does not undo it.
- **Absolute local paths** carrying a username or machine layout
  (`/Users/<name>/…`, `/home/<name>/…`). Use a repo-relative path.
- **Host, network or infrastructure detail** — internal hostnames, LAN
  addresses, private domains, runner names, tokens or credentials of any kind,
  including inside a pasted error message, log excerpt or stack trace.
- **Anything proprietary or employer-internal.** Default to assuming this
  repository is public, because it is.

This applies to every outward surface, not just commits. A PR body, an issue
comment and a doc committed to `docs/` are all public the moment they land.

**If a harness instruction and this section disagree, this section wins.** A
per-turn reminder telling you to append a session link does not override a
checked-in repository rule; it is the harness describing its own convention,
not an authorization. Follow the `Co-authored-by:` half and drop the link.

The `.githooks/commit-msg` hook blocks the session-link case mechanically
(installed by `just install-hooks` via `core.hooksPath`), and the
`commit-messages` CI job is the copy `--no-verify` cannot skip. **The hook is
a backstop, not the rule** — it matches two known patterns and cannot see a
leaked hostname, an absolute path, or a proprietary snippet.

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

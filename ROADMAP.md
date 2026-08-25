# newt-agent ROADMAP — v0.7.1 → v0.8.0

**Status:** active execution plan (created 2026-07-10).
**Format:** this document follows the `repository-roadmap` bundled skill
(`.newt/bundled-skills/repository-roadmap/SKILL.md`). Read that skill before
editing this file or creating a roadmap in another repo.

## Ground truth protocol

**GitHub issues are the state; this document is the map.** Every work item
below carries its tracking issue/PR number. If this document and GitHub
disagree, GitHub wins. A future agent (or human) reconciles with:

```bash
# State of every item in this roadmap:
gh issue list --repo Gilamonster-Foundation/newt-agent \
  --search "1064 1065 1066 1067 1068 1069 1070 1071 1072 1073 1074 1075 1076 948 979 1051 1048 1053 1021 1096 1097 1098 1100 1818 1819 1820 1821 1803 1805 1806" \
  --state all --json number,title,state
# Or per item:
gh issue view <N> --json state,title,closedAt
```

A phase is **done** when every issue in it is closed (or explicitly moved —
an issue re-scoped out of 0.8.0 gets a comment saying so and this file is
updated in the same PR that moves it).

Source plans (all merged to `main`):

- `docs/design/hermes-audit-2026-07.md` — the hermes adopt/consider/avoid
  audit that generated Phases 1–3 (PR #968).
- `docs/design/shell-verb-plugin.md` + `docs/designs/persistent-shell-context.md`
  — persistent shell context (PR #1028).
- `docs/dgx-unboxing-experience.md` — DGX unboxing experience (PR #1055).
- `docs/ROADMAP.md` — the historical drake-flight step ledger for v0.x
  (kept; this file supersedes it as the forward plan).

## Standing discipline (applies to every behavioral item)

propose → inertness check → implement behind a flag → n≥5 sweep → default
only on measured lift. Docs and pure-hygiene items skip the sweep, not the
review. Every PR: `just check` green, one concern per PR.

---

## Phase 0 — clear the runway (in-flight PRs)

Land or close what is already open so 0.8.0 work starts from a clean slate.

| Item | Tracker | Notes |
|---|---|---|
| Persistent shell context design docs | PR #1028 | Merge (docs only); implementation is #1076 in Phase 4 |
| DGX Unboxing Experience project plan | PR #1055 | Merge (docs only); implementation is #1051/#1048/#1053 in Phase 5 |
| Persona train (PA persona, MCP wiring) | #1021 → PRs #1047, #1049, #1050, #1052, #1058 | Land in numbered order (5.1 → 5.2 → 5.3) |
| Roadmap task-close from git truth | #1062 → PR #1063 | Small, independent |
| This roadmap + repository-roadmap skill | (this PR) | — |

**Exit:** no open PRs older than this phase.

## Phase 1 — hermes wave 1: cheap, high-confidence (each one small PR)

From `docs/design/hermes-audit-2026-07.md`, ADOPT tier, post-critique.

| Item | Issue | Notes |
|---|---|---|
| memory_fetch spill-loop check — **possible live bug, do first** | #1064 | Either failing test + exemption, or documented unrepresentable |
| Guard canary self-tests + merge-base CI check | #1065 | Canaries attack the OCAP floor; weakening a guard = red test |
| Loop/store hygiene: cooldown persistence, FTS repair ladder, `_turn_exit_reason`, flush-before-destructive | #1066 | Four small commits, one PR |
| Footprint Ladder decision doc | #1067 | data file → steer TOML → crew role → MCP → core tool last |
| NOTES.md write hygiene + negative-space taxonomy | #1068 | Pattern reused by #1076 in Phase 4 |
| Recall bookends + automation-session tagging prerequisite | #1069 | Demotion (not exclusion) waits on tagging |
| Summarizer hang / over-aggressive trigger fix | #979 | **Blocker for #1070**; pairs with #1019 (exit-time compression) |
| Temporal anchoring in compression prompts | #1070 | Blocked by #979 |

**Exit:** #1064–#1070 closed; #979 closed or the compressor path stable
enough that #1070 landed against it.

## Phase 2 — eval gates: measure before mechanism

The conditionals live *inside* these issues; closing one records either the
adoption or the measured-inert verdict.

| Item | Issue | Gates |
|---|---|---|
| Premature-done metric in the eval harness | #1071 | Verify-on-stop (hermes A7) — port only if the metric shows the gap |
| Prefix-cache metric (llama.cpp slot-reuse / TTFT) | #1072 | Frozen-prefix audit (hermes A6) — audit only if the metric justifies it |
| Carry memory_fetch spill handles in compaction markers | #1073 | The 80%-built compression-amnesia fix; tri-state escalation only with the June security riders |

**Exit:** both metrics exist in the harness with a recorded baseline;
#1073 landed or explicitly escalated.

## Phase 3 — OCAP wave: authority before convenience

One substrate: #948 + #1074 + #1075 share the same isolation and caveat
model (hermes audit AVOID-12: never three parallel orchestration stacks).
The live friction cluster is queryable as `label:ocap`.

| Item | Issue | Notes |
|---|---|---|
| Script-RPC caveat class design doc | #1074 | Four-part security case (argv bypass, snapshot TOCTOU, headless approval, aggregation); no round refunds |
| ExecBackend seam (trait + edge-case tests) | #1075 | Blocked by #1074; DGX/homelab remote exec |
| Subagent tool (single-call dispatch) | #948 | Supervision mechanics from the audit are already commented on the issue |
| OCAP friction: two-flag UX | #978 | `--full-access` vs `--yolo` split |
| OCAP friction: ambient env-var grants decision | #1013 | Reject or justify with threat model |
| OCAP friction: `/dev/null` denied as exec target | #969 | Redirect targets are not executables |
| OCAP friction: libxcrun sandbox block on git push | #1016 | (#972 closed as dup) |
| OCAP UX: denied commands should surface a prompt/explanation | #1027 | Floor held correctly; the UX around it didn't |

**Exit:** #1074 design accepted; #1075 and #948 implemented on the shared
substrate; friction cluster triaged to fixed / wontfix-with-rationale.

## Phase 4 — persistent shell context (PR #1028 → implementation)

| Item | Issue | Notes |
|---|---|---|
| Persistent shell context plugin | #1076 | Per `docs/design/shell-verb-plugin.md`; state file gets the #1068 write-hygiene pattern; env restoration must not become an ambient-authority channel (cf. #1013) |

Sequenced after Phase 3's #1074 design so the plugin's exec posture is
decided against the caveat class, not before it.

**Exit:** #1076 closed behind a flag with tests.

## Phase 5 — DGX unboxing experience (PR #1055 → implementation)

Per `docs/dgx-unboxing-experience.md`.

| Item | Issue | Notes |
|---|---|---|
| `newt setup <url>`: endpoint probe → backends + model cards + splash | #1048 | First-run onboarding |
| Backend failover: ordered fallback chain (≤3) | #1053 | — |
| DGX-Spark/GB10 unboxing + repair wizards | #1051 | Builds on #1048/#1053; hardware-aware vLLM tuning |

**Exit:** the plan's phase-1 experience works end-to-end on the GB10
bring-up target.

## Phase 6 — TUI command surface & module cohesion (#1096)

Align the TUI input line to Claude Code's clean two-namespace model
(`/slash` + `!shell`), and split the surfaces it touches out of the
`newt-tui/src/lib.rs` + `rich_input.rs` god-modules by **functional
cohesion**, non-breaking code-motion first, so the behavioral command work
lands in small, legible modules. Tracking checklist: **#1096**.

**Cohesion ratchets** — pure code-motion, no behavior change, each its own PR:

| Item | PR | Status |
|---|---|---|
| Extract splash screen → `splash.rs` | #1097 | ✅ merged |
| Extract vi state machine → `vi.rs` | #1100 | ✅ merged |
| Extract first-run setup/download screen → `setup_tui.rs` | — | 🔨 in progress |
| Extract permission prompts → `permissions.rs` | — | ⬜ next |
| Break command handlers into per-group modules | — | ⬜ |

**Command-surface ratchets** — behavioral, land into the reorganized modules:

| Item | Status |
|---|---|
| Retire bare shell verbs (`cd pwd ls rm …`); add `/cd` | ⬜ |
| `/clear`+`/compact` CC aliases; `/config show` + bare-`/config` stub | ⬜ |
| `/effort [low\|medium\|high\|max\|auto]` preset dial over the nudge knobs | ⬜ |
| Fold `/memory` usage into `/context` (optional) | ⬜ |

**Enabling infra:** #1098 restructures the pre-push hook to lint/test only
*changed* code (the current whole-workspace hook is ~50 min); temporary
`--no-verify` is permitted until it lands (see Branch + PR policy).

**Exit:** input line is the CC two-namespace model with no bare-verb
interception, and every surface it touches lives in a cohesively-named
module under `newt-tui/src/`.

## The context-seam → server track (sequenced by epic #1805)

Runs beside Phases 0–6. It is **not** in the 0.8.0 release criteria below
unless explicitly re-scoped (epic #1805 names "v0.8.0 in September 2026" as
a planning target, not a promise). The order is fixed by #1805's
"Implementation order": land the public context-seam refactor, land #1803,
then execute the server epic.

| Item | Tracker | Status |
|---|---|---|
| Model identity — names are labels, never evidence | PR #1818 | ✅ merged 2026-08-25 (`0b3deae6`) |
| Named-card capability resolution — receipts + typed route decisions | PR #1819 | ✅ merged 2026-08-25 (`5228b6e0`) |
| Tenacity exact-family migration — delete name-substring inference | #1820 → PR #1821 | 🔄 open, CI green, awaiting review |
| Newt Markup — one interaction model, many native views | Epic #1803 | ⬜ next; first slice A0 (inventory + ADR) |
| Pinned Codex app-server compatibility for remote Newt sessions | Epic #1805 | ⬜ after #1803; first sub-issue #1806 |

**Exit:** #1820 closed; #1803's spine ratified and landed per its slice
order; #1805 executed to its Phase-1 exit criteria (a stock pinned Codex
client drives a remote Newt session black-box on a clean remote POSIX host).

## v0.8.0 release criteria

- Phases 0–6 exit criteria met (or items explicitly re-scoped with issue
  comments + a roadmap update in the same PR).
- CHANGELOG.md entry summarizing the release by phase.
- `just check` + `just cov-ci` green; coverage ratchet not lowered.
- Version bump PR: `Cargo.toml` workspace version 0.7.1 → 0.8.0.

## Deliberately out of 0.8.0

- Verify-on-stop and frozen-prefix-audit *mechanisms* (conditional on the
  Phase 2 metrics; adopting them is 0.9 work if gated in).
- Soft-archive tri-state (only if #1073's marker-handles proves
  insufficient, with the June security riders).
- Scheduler/cron wake gate, mesh delivery-mirroring (parked by decision).
- Background self-review as crew job (maintainer-discretion, behind a sweep).
- Hermes CONSIDER backlog not listed above (steer channel #952, recovery
  ladder, always-stream, context breakdown, skill-index gating, …) — lives
  in `docs/design/hermes-audit-2026-07.md` CONSIDER section.

# Crew front door + minimal workflow TUI — making the multi-LLM machinery human-drivable

**Status:** Design (2026-06-18). Builds on merged work: `run_crew` + `BackendPool`
+ `Dispatcher` (newt-scheduler), the multi-attach session model (newt-core
`session.rs`, #429), loadouts/kits/profiles (newt-core `config.rs`), and the
rich-tui input surface (#416). Companion to `crew-loadout.md` (the *ensemble &
config*), `workflow-swarm-harness.md` (count-adaptive dispatch, panel vs crew),
and `mesh-remote-control-mobile-app.md` (multi-attach). **Amends**
`docs/decisions/plain_scroller_tui.md` with one workflow carve-out.

## 1. The gap this closes

The multi-LLM engine is built and tested, but a **human can't reach it**:

| Piece | State | Reachable by a human? |
|-------|-------|-----------------------|
| `BackendPool` (health, model-pin, failover) | ✅ built | only indirectly (`newt doctor`, `newt dgx …`) |
| `Dispatcher` (`LocalDispatcher`) | ✅ built | no |
| `run_crew` (navigate→plan→verify→triage→revise) | ✅ built | **no — in-crate tests only** |
| multi-attach session model (`OutputChunk` fan-out) | ✅ built (in-process) | no |
| loadouts / kits / profiles / role profiles | ✅ built | `newt --loadout <name> code` |
| panel / judge / bake-off | ✖ design-only | n/a |

So `run_crew` has **no CLI command, no slash command, and no observation
surface**. This doc designs the smallest **front door** (invocation) + **minimal
TUI** (observation/steering) that makes the workflow human-runnable — which is
also what makes it human-*testable* against real models. One build unblocks both.

Non-goal: the diversity **panel** (same task, many voices, vote) and the full
multi-pane **pilot** dashboard. Those are later/`gilamonster`-tier (see §6).

## 2. Prerequisite: a real `Workspace`

`run_crew` takes `workspace: &mut dyn Workspace` (newt-scheduler `crew.rs`).
Today only the in-memory mock `MemWs` exists (tests). The front door needs a
**worktree-backed `Workspace`**:

- `files()` → tracked paths under the target dir (git-aware, gitignore-respecting).
- `read(path)` → file contents.
- `apply(edits)` → write full-file edits into an **isolated git worktree** (never
  the live tree), return touched paths.
- `run_test()` → shell out to the crew's verification command (`just check`,
  `cargo test`, `pytest -x`, …), return `(passed, captured_output)`.

This is the guardrail that keeps "the harness owns test execution" honest (the
model never marks its own work green). It is a **build dependency** of the front
door and should land first (or in the same PR, clearly separated).

## 3. The front door (invocation)

### 3.1 CLI: `newt crew`

```
newt crew "<task>"                 # uses [crews.default] (or the only crew)
newt crew --crew coder "<task>"    # named crew ensemble from [crews.<name>]
newt crew --crew coder --test "just check" --max-attempts 3 "<task>"
newt crew --crew coder --dir ./subproject "<task>"
newt crew --crew coder --dry-run "<task>"   # show placements + plan, no edits/test
```

Resolution (reusing what exists):
1. Read `[crews.<name>]` (per `crew-loadout.md`) → the role-loadouts
   (`planner`/`navigator`/`triage`) + control knobs (`max_attempts`, budgets).
2. Each role-loadout resolves through the existing loadout machinery
   (`provider → model → kit → profile → role → settings`) — **no new config
   surface**; `newt crew` is a *consumer* of loadouts, like `newt --loadout`.
3. Build the `BackendPool` from `[backends]` (`StaticSource`), probe health
   (`TcpProber`), build a `LocalDispatcher`, build `CrewConfig` from the role
   models, build the worktree `Workspace` for `--dir` (default `.`).
4. `run_crew(&pool, &dispatcher, &mut ws, &cfg, task).await` → render the
   `CrewOutcome` (§4).

Exit codes: `0` = `Passed`, `2` = `NeedsHumanReview` (honest cap-exit), `1` =
setup error (bad crew/loadout ref, no backend serves a role → the pool's
`Refuse`). Headless-friendly: with stdout piped, render the labeled stream as
plain lines (no status row), so `newt crew … | tee run.log` works and the swarm
tier can drive it.

### 3.2 Slash command: `/crew "<task>"`

Inside a `newt code` rich-TUI session, `/crew "<task>"` runs the same engine
**in-session**: the crew's labeled output streams into the session's scrollback
(§5), the workflow status row activates (§5.2), and the human stays the driver
(steer / abort / answer a review gate). This is the interactive twin of `newt
crew`; both call one `run_crew`.

Default crew/dir come from the session; `/crew --crew <name>` overrides.

## 4. `CrewOutcome` rendering

```
✓ crew passed in 2 attempts — touched: src/foo.rs, src/bar.rs
⚠ crew needs human review (3/3 attempts, last test still failing)
  last triage: "off-by-one in the loop bound; planner kept the < instead of <="
✗ crew refused: no live backend serves the planner model (qwen3-coder:30b)
```

Always print the **verification truth** (the harness's `run_test` output tail on
failure) — never a model's self-assessment.

## 5. The minimal TUI (observation + steering)

**No new surface.** A crew run is mostly *output*, which is scrollback-friendly,
so we reuse the rich inline input surface (#416). Two additions, both small:

### 5.1 Labeled-stream scrollback blocks

Each crew step prints a **labeled block into real scrollback** (via
`insert_before`, exactly like the rich surface already echoes turns) — no panes,
no alt-screen, no region. Format (one marker line + indented body):

```
▸ navigate · dgx▸devstral-small-2:24b
    4 files: src/foo.rs src/bar.rs src/lib.rs tests/foo.rs
▸ plan · dgx▸qwen3-coder:30b · attempt 1/3
    edit src/foo.rs (+12 −3)   edit src/bar.rs (+4 −0)
▸ verify · just check
    ✗ 1 failing: foo::tests::off_by_one
▸ triage · gnuc▸qwen2.5-coder:3b
    "loop bound uses < where it should be <="
▸ plan · dgx▸qwen3-coder:30b · attempt 2/3
    edit src/foo.rs (+1 −1)
▸ verify · just check
    ✓ green
✓ crew passed in 2 attempts
```

These map cleanly onto the multi-attach `OutputStream` variants (newt-core
`session.rs`) so the same stream can later fan out to a remote observer
unchanged:

| crew step | `OutputStream` | rendered as |
|-----------|----------------|-------------|
| role marker (`▸ navigate …`) | `AgentThought` | dim role + `provider▸model` |
| edits | `Diff` | `edit <path> (+a −b)` (counts, not full diff) |
| verify output | `Stdout`/`Stderr` | the test's captured tail |
| triage summary | `AgentThought` | quoted one-liner |

Driving everything through `OutputChunk`/`OutputSink` (even with a single local
sink today) is the cheap future-proofing: §6's phone/observer attaches later with
zero changes to the crew loop.

### 5.2 Workflow-aware status row

The rich surface already renders a status row (clock + edit-mode). Extend it,
**while a crew is running**, to a workflow line:

```
[20:51:24] crew · PLAN 2/3 · dgx▸planner gnuc▸triage · ⏱ 14s
```

Schema: `[clock] crew · <STEP> <attempt>/<max> · <role▸backend placements> · <elapsed>`.
When no crew is running, the row is the normal `[clock] vi N ❯` prompt. The
status row is a *log marker that floats*, exactly the property the plain-scroller
doc already grants the prompt — so this is an extension of an existing carve-out,
not a new surface.

### 5.3 Human as driver

- **Steer / abort:** `Ctrl-C` already abandons the input line; during a crew run
  it requests a **graceful cancel** at the next role boundary (the loop checks a
  cancel flag between steps — turns are serialized in the session model, so this
  is clean).
- **Review gates:** when a crew step hits a `require_human_review_on` gate
  (future crew budget, `crew-loadout.md` §budgets), it **pauses and prompts** in
  the rich input region (`approve / reject / edit`), then resumes. Same input
  surface, no modal screen.

### 5.4 The plain-scroller flex (amendment to `plain_scroller_tui.md`)

This is the *only* deviation, and it is small:

- **Allowed:** a **workflow-aware status line** (an extension of the existing
  floating-prompt carve-out) and **role-labeled scrollback blocks** (still
  scrollback, still `insert_before`, no region).
- **NOT added:** panes/splits, an alternate screen, per-role live-updating
  regions, a dashboard. Those remain out of the chat path; the full multi-pane
  workflow view is the **`pilot`** command (gilamonster/monitor tier).

`plain_scroller_tui.md` gets a new carve-out bullet recording this, with the
same "inline-only, severable, feature-gated where heavy" constraints as the
rich-tui carve-out.

## 6. Out of scope (tracked elsewhere / later)

- **Diversity panel / judge / bake-off** — same machinery, opposite intent
  (`workflow-swarm-harness.md`, `crew-loadout.md`). Future.
- **Full `pilot` multi-pane dashboard** — one pane per role/attachment, the
  `OutputStream` fan-out rendered live. The rich (gilamonster-tier) version;
  this doc is the minimal scrollback version.
- **Remote attach (phone/peer)** — Phase 1b mesh wire protocol
  (`mesh-remote-control-mobile-app.md`). The `OutputChunk`/`OutputSink` mapping
  in §5.1 keeps the door open at zero cost.
- **Crew budgets / review-gate config** (`max_files_touched`, `max_lines`,
  `require_human_review_on`) — `crew-loadout.md`; §5.3 just consumes them.
- **Role-profile tool/caveat *enforcement*** — declared today, enforced later.

## 7. Test plan (the verification this whole thing enables)

Layered, cheapest first:

1. **Loadouts (today):** `newt config` shows resolved `[loadouts.*]`/`[backends.*]`;
   `newt --loadout <name> code` applies every axis (env `NEWT_PROVIDER` /
   `NEWT_DGX_MODEL` / `NEWT_PROFILE` / `NEWT_NUM_CTX`, the role persona, the
   framing overlay); CLI args override loadout; a loadout with a missing
   kit/profile/provider is **rejected** with a clear error.
2. **Pool / dispatcher (today):** `newt doctor`, `newt dgx status|models|doctor|route`;
   failover (two backends, take one `Down`, confirm the other is chosen +
   `failed:[…]` reported); model-pin **refusal** when no backend hosts a pin.
3. **Crew (after the front door):** `newt crew --crew coder --test "<cmd>" "<task>"`
   against a tiny fixture repo with one failing test → observe
   navigate→plan→verify→triage→**converge**, or honest **NeedsHumanReview** at
   cap; exit codes `0`/`2`/`1`. Plus the existing inline tests (convergence /
   cap-exit / no-backend).
4. **Multi-LLM end-to-end:** a loadout per role (navigator→small/fast,
   planner→big, triage→fast), run crew, observe **role→backend placement +
   failover** live in the status row + labeled stream.
5. **TUI:** `/crew` in a `newt code` session renders the labeled blocks into
   scrollback, the status row shows live progress, `Ctrl-C` cancels at the next
   role boundary, scrollback/copy-paste survive (no alt-screen).

Fixtures: a `tests/fixtures/crew-tiny/` repo (one obvious failing test a 3B model
can fix) so the e2e is fast and deterministic-ish.

## 8. Build order (when we code it)

1. Worktree-backed `Workspace` (§2) + its unit tests (real fs, `tempfile`).
2. `newt crew` CLI (§3.1) wiring pool+dispatcher+workspace+config → `run_crew`,
   with the plain labeled-stream renderer (headless path first — testable with
   `assert_cmd` against the fixture + a mock dispatcher).
3. `/crew` slash command (§3.2) + the labeled-stream blocks in the rich surface
   (§5.1) + the workflow status row (§5.2). Behind the existing `rich-tui`
   default; the headless renderer is the fallback.
4. Cancel + review-gate steering (§5.3).
5. Amend `plain_scroller_tui.md` (§5.4).

Each is a focused PR; (1) and (2) are the unblock and are independently testable
without a TTY.

## 9. Open questions

- **Crew config home:** `[crews.<name>]` (per `crew-loadout.md`) vs. a crew being
  "just another loadout that references role-loadouts." Lean: `[crews.*]` as
  designed; revisit if it feels redundant.
- **Default verification command:** infer from the repo (justfile → `just check`,
  Cargo → `cargo test`, Python → `pytest`) vs. always require `--test`. Lean:
  infer with an override, refuse if none found (no silent no-op test).
- **Streaming granularity:** do we stream a role's tokens live, or only its
  parsed result? Lean: result-only for v1 (cheaper, less noise); live tokens
  behind `--verbose`.
- **One crew at a time** (turn-serialized session) for v1; crew-of-crews is the
  swarm layer, later.

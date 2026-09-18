# Plan mode: draft in scratch, present once, approve, then implement

**Status:** design note (pre-build) · **Amends:** [`plan_editor_ephemeral_tui.md`](../decisions/plan_editor_ephemeral_tui.md) · **Builds on:** `PlanModeControl` ([`plan_mode.rs`](../../newt-core/src/agentic/plan_mode.rs)), `render_report` ([`report.rs`](../../newt-core/src/agentic/report.rs)), the permission gate ([`permissions.rs`](../../newt-core/src/agentic/permissions.rs)), per-session `plan.md` (`session_plan_path`, #220) · **Prior art:** [`weak-model-plan-mode-findings.md`](../research/weak-model-plan-mode-findings.md), [`thinking-effort-and-plan-mode.md`](thinking-effort-and-plan-mode.md)

## The failure this answers

Observed in an interactive session with a 30B local coder model, on a request to
plan a refactor. The turn ran under the read-only Plan disposition.

- The model called `render_report` four times. Each call printed a full,
  near-identical refactoring plan to the scroller.
- The model then restated the plan a fifth time as its final reply.
- Nothing asked the operator whether to implement. The turn ended and the
  session returned to the prompt.

The operator's verdict: the final content was good, and the repeated display was
the confusing part. The target behaviour is what Claude Code, Codex and pi.dev
do: enter plan mode visibly, draft out of sight of the transcript, present the
plan once, wait for approval, then implement.

## Two causes

Both were read from the code, not inferred.

1. **`render_report` has no repeat limiting, and its ack pushes the model on.**
   Every ack appends `REPORT_DELIVERY_GUIDANCE`: "Continue any unfinished
   requested work with tools; rendering a report is not completion of that
   work." `RepeatCallGuard` keys on the exact tool name plus exact arguments, so
   four reports that differ by a heading never collide. A weak model reads the
   nudge as "report again".
2. **`exit_plan_mode` has no human in it.** The dispatch arm calls
   `set_plan_mode(false)` and returns. The model lifts its own clamp. There is
   no approval step anywhere in the flow.

## What already exists

Most of plan mode is built. This design widens it and adds two missing pieces.

| Piece | Where | State |
|---|---|---|
| Model-entered plan phase | `PlanModeControl`, `enter_plan_mode` / `exit_plan_mode` | exists |
| Read-only authority clamp | `plan_phase_clamp()`, the dispatch-time `Act` to `Plan` promotion | exists |
| Turn-level read-only class | `PromptDisposition::Plan`, `tool_allowed` | exists |
| Operator-selected mode | `OperatingMode::Plan`, `/mode plan` | exists |
| Step ledger | `StepLedger`, `update_plan`, `plan_get` | exists, capped at 3,000 chars, too small for a prose plan |
| Per-session plan file | `session_plan_path` | exists, but the clamped model cannot write it |
| Approval prompt wording | `[y / N / discuss / edit]` in the accepted `/plan` decision | specified, not built |
| A draft that is not a display | none | **missing** |
| A human approval step | none | **missing** |

## Design

### One draft slot, presented once

While the effective disposition is Plan, `render_report` stops printing. Its
composed Markdown (the existing `compose_document`) replaces a session plan
draft and increments a revision counter. At the end of the planning turn the
latest revision is presented exactly once, through the same
`presentation.document` path a report uses today.

One code path covers both "the model called `exit_plan_mode`" and "the model
never did". There is no new model-facing tool. Weak models already reach for
`render_report`, so the observed four displays become four revisions of one
draft.

- A `PlanDraft { revision, markdown }` value sits behind a `PlanDraftSink` trait
  in `plan_mode.rs`, beside `PlanModeControl`. newt-core emits the draft. The
  TUI persists it to `session_plan_path`. newt-core gains no filesystem
  authority, and the model's clamp is untouched because the harness does the
  write.
- The ack under Plan changes to say: saved as revision N, not yet shown to the
  operator, finish the plan and then call `exit_plan_mode`.
- `RepeatCallGuard` stays exact-match. The draft slot removes the need for
  fuzzy matching.

Rejected alternative: a new `draft_plan` tool. It would be a second way to say
what `render_report` already says, and a weak model would have to learn to pick
it.

### Drafting is visible, the draft is not

The operator asked for a clear "working in scratch space" signal and against
silence.

- Entering plan mode prints one banner line.
- Each revision prints one plain line, for example `plan draft rev 3 saved, 42
  lines; not yet presented`.
- The RichTUI header shows `PLAN drafting rev N` through the existing free-text
  `headline` of `header_line`. A new `InputSurface` method is added only if the
  headline cannot carry it, and then exactly one, because every method must
  also be forwarded by `RemoteSurface`.

### Approval is one routine with three entry cases

A pure function in newt-core maps `(PlanEntry, HumanQuestionOutcome)` to a
`PlanVerdict`. The question goes through `PermissionGate::ask_question` and uses
the accepted decision's `[y / N / discuss / edit]` wording.

Approval restores authority the session already holds. It mints none. The
precedent is `DenialKind::RemoteTool`, whose doc says an `Allow` "is purely
'proceed with this call' (it widens nothing)". Approval never passes through
the caveat re-mint path. `Unavailable` and `Cancelled` mean "stay read-only".

How plan mode was entered decides what approval can do:

| Entry | On approve |
|---|---|
| The model called `enter_plan_mode` inside a normal Act turn | Clear the flag. The same turn continues into implementation. This restores the turn's own validated Act disposition. |
| Intake inferred a Plan turn (the observed case) | The turn's caveats were met with `plan_phase_clamp()` at turn start by `operating_mode_caveats`. Lifting them mid-turn would mint authority, so the planning turn ends and the TUI immediately starts an Act turn seeded with the approved plan. The operator types nothing. |
| The operator set `/mode plan` | The prompt offers "approve and switch to `/mode dev`". Switching the mode is the operator's explicit act. It then proceeds as the row above. |

The hook sits in the chat loop after `surface.turn_ended()`, beside the existing
round-cap branching on `turn_end_reason`. Reject or discuss keeps the clamp and
feeds the operator's text back as the next prompt.

Headless runs inject no gate and no plan controls today. They present the draft
once and never block. The design reuses `TurnEndReason::AwaitingOperator` and
adds no variant.

### A RichTUI pane for the draft

A non-modal region inside the mounted rich surface, on the palette pattern: pure
state with no terminal and no I/O, rendered by carving the body area, owned by
the mounted editor. It is hidden by default, toggled by a key, and fed by the
same `PlanDraft`. It is compile-gated to `rich-tui` and has no lean twin.

The state struct is shaped as key handling plus a view so it can move to the
`newtui` crate later. It is built in `newt-tui` first.

The accepted `/plan` decision rejects a live plan dashboard, so the amendment in
this PR lands before any pane code.

### No new slash verb

Entry stays `/mode plan` for the operator and `enter_plan_mode` for the model.
The slash registry ratchet only lets the surface shrink, and `/plan` stays
retired in favour of `/roadmap`.

## Slices

One issue and one PR each, smallest first. Slice 2 alone removes the repeated
display.

1. Decision-doc amendment (this PR, docs only).
2. Draft slot: `plan_mode.rs`, the `render_report` dispatch arm, the ack in
   `report.rs`, present-once at turn end in the chat loop. No approval yet.
3. Approval routine: the pure verdict function, the turn-end prompt, reuse from
   the `exit_plan_mode` arm, the seeded Act turn.
4. Drafting indicator: banner, revision lines, rich headline.
5. Prompt guidance: do not restate the plan in the final reply.
6. RichTUI plan pane.

## Test plan for the slices

All in the fully mocked unit tier.

- Extend `tools_tests/execute_plan_disposition.rs`: four `render_report` calls
  under Plan leave one stored draft at revision 4 and make zero `document`
  calls.
- Extend `mod_tests/http_loop.rs` (`ScriptedOpenAi`,
  `run_openai_script_with_ledger`): the draft is presented once at turn end.
- `ScriptGate(HumanQuestionOutcome)`: approve proceeds, reject stays clamped,
  `Unavailable` and `Cancelled` never approve.
- Extend `mod_tests/http_plan_handoff.rs` for the seeded Act turn.
- The pane gets pure state-struct tests only.

Ratchets each slice meets: `terminal_taker_registry.rs` (slice 3),
`the_proxy_forwards_every_surface_method` (slice 4, only if a method is added),
the bottom-of-file ratchet in `disposition_voice.rs` (slice 5). Using
`ask_question` keeps clear of the `gate.ask(` site count in
`markup_sprawl_ratchet.rs`. Adding no `TurnEndReason` keeps clear of the
exhaustive match in `headless_contract.rs`.

## Out of scope

The TOML `Plan` / `Subtask` DAG format, the alt-screen plan editor,
one-subtask-per-turn execution, extraction of the pane to `newtui`, lean-surface
parity, and fuzzy matching in `RepeatCallGuard`.

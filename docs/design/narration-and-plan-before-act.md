# Narration rows and plan-before-act

**Status:** design note, implemented on `feat/narration` · **Builds on:**
[`plan-mode-draft-present-approve.md`](plan-mode-draft-present-approve.md)
(the approval handoff this reuses end to end), `initiative`
(`newt-core/src/initiative.rs`), the step ledger
(`newt-core/src/agentic/scheduled.rs`) · **Prior art:**
[`weak-model-plan-mode-findings.md`](../research/weak-model-plan-mode-findings.md)

## The run this answers

Observed on 2026-10-06, `ornith-1.5-35b` over Chat Completions, initiative
`patient`, in a lab clone of this repo. The prompt was "let's refactor the
largest file in this repo into smaller files".

- The model wrote a plan to the session scratch directory and called
  `update_plan` with seven steps, then started writing new files within the
  same turn. Nothing presented the plan to the operator or waited for them.
- Across 60 rounds the screen showed only `⚙` tool headers, spill markers
  and the heartbeat line. The model's own prose, where it existed, arrived
  together with tool calls and was replayed into history unseen.
- The operator's verdict: the behaviour is right for a relentless run and
  wrong for the default. What was missing was a way to see what the model was
  doing, and a pause at the plan.

Read from the code (derived, not run):

- Every provider loop binds the round's cleaned text (`probe_content`,
  `oa_content`, `text`) and, inside the tool-call branch, references it only
  to replay it into history; the one exception is an 80-character excerpt
  printed under `debug`. The same text with no call behind it is answered by
  `narration_action_nudge`, so the harness taught the model that prose
  equals failure.
- `Initiative::Patient` only delays the act nudge (12 read-only rounds). No
  initiative level holds a mutation back.
- `refactor` is an action needle in the intake lexicon, placed before the
  research check on purpose so "refactor the largest file" acts.

## Three mechanisms, no new model-facing surface

### 1. The prose that arrives with a tool call is shown

`display::tool_round_prose` is a pure builder: it strips inline think
blocks, drops any paragraph that is the tool call itself (a fenced block, a
bare JSON object or array, a `<function=…>` or `<tool>…</tool>` form, from
where it starts, even mid-line), drops heading lines and list markers, and
returns one row per paragraph or list item with nothing cut short. The
emitter wraps the prose to the current reply width first, then bounds the
rendered terminal rows: the first `[tui] spill_lines` rows commit, the rest are retained
behind the one fold marker (`▲ N lines hidden [/spill open N]`), so a long
explanation is kept whole and raised on demand rather than cut to a
sentence (the operator's call, 2026-10-07, after a row was cut at "(e.g.").

`commit_tool_round_narration` emits those rows once per validated batch, from
the same point in all four loops (inside the accepted-batch branch, after
whole-batch validation and before the first side effect; a rejected batch
prints its rejection rows and no narration), through `emit_notice_line`: the writer the reasoning fold
already used, widened to take a level and a glyph. The row is
`Level::Narration` with the `▹` glyph, two-space gap: the hollow sibling of
the `▸` reply marker, in the theme's `narration` role (magenta in the built-in
theme, retunable like every role, e.g. `NEWT_THEME=narration=140`), so the
model's interim voice is told apart from its reply, its reasoning and the
operator's own text at a glance. The Anthropic streamed arm
skips it, since that wire already printed the text live.

Reach is identical to the `⚙` header: stdout, erased ephemerals first, no
capability gate, so a piped or headless run receives the same bytes.

### 2. A ticked plan is one row

In the `update_plan` executor arm, the ledger is snapshotted before and after
the call. When the plan is the same ordered list of steps and only statuses
moved, `scheduled::step_change_line` yields `step 3 of 7: extract denials.rs`
(or `plan complete: 7 of 7 done`) and the arm shows it to the operator
through `presentation.override_result`. The model still receives the full
`<plan>` block. A new or rewritten plan keeps the block as the operator's
view too, so the plan the operator will be asked about is on screen.

This lands in every loop and on every surface for free, because the arm is
the one place the ledger is mutated in-loop and `ToolPresentation` is the
executor's one operator-facing seam. No print site was added to `tools.rs`,
which has none.

### 3. The first multi-step plan of an acting turn is approved first

In the same arm: when `fresh_multi_step_plan(before, after)` holds (a
multi-step plan now exists where there was none, where the previous one had
finished, or where an unrelated plan sharing no step replaced it), the turn's
disposition is `Act`, `Initiative::asks_before_acting()` holds, and the turn
is not the implementing turn an approval seeded
(`PlanModeControl::implementing_approved_plan`), the arm calls
`set_plan_mode(true)` then `request_exit()` on the session's
`PlanModeControl` and appends `PLAN_APPROVAL_REQUESTED` to the tool result.
Everything after that is the existing handoff:

| step | existing code |
|---|---|
| later calls in the same batch are clamped | dispatcher promotion to `Plan` when `is_plan_mode()` |
| the turn ends after the batch | `pending_plan_approval_handoff` → `TurnEndReason::AwaitingOperator` |
| the operator sees the plan | the turn-end hook prints the ledger block when no draft was presented |
| the question | `run_plan_approval` → `PermissionGate::ask_choice` with `PLAN_APPROVAL_CHOICES` (yes / no / discuss, `yes` default), the same selection form a permission prompt uses; `discuss` opens one free-text follow-up; a text-only gate falls back to `ask_question`, where a blank submission, `y`, `yes`, or a bare continuation (`go`, `do it`, `proceed`) approves; cancellation, exit, unavailable, closed or failed input never approves |
| approval | `set_plan_mode(false)` (the `ModelDuringAct` row: restores what the turn already held), the seed names the ledger plan when no draft exists |
| denial or discussion | the clamp stays, exactly as for a model that called `enter_plan_mode` |

**Authority.** Entering Plan can only attenuate a turn already validated for
Act (`plan_mode.rs`). The request mints nothing. Approval restores authority
the session already holds and passes through no caveat re-mint. No row is
needed in `docs/security/ocap-deviations.md`; this is stated here because a
reader will otherwise read "the harness entered plan mode for the model" as
a widening.

**Posture.** The gate is keyed on the initiative dial because that dial is
already "how much it looks before acting":

| initiative | first multi-step plan in an acting turn |
|---|---|
| patient, measured (default) | presented, awaits y / N / discuss |
| decisive, eager | set silently; the run continues |

`/mode full-auto` answers the question for the operator: the plan is still
printed, nothing is asked, the seed runs. A headless run injects no
`PlanModeControl`, so nothing is armed there, the same posture the parent
design records. Each plan asks once: a tick or an amendment that keeps a
step is never fresh; the plan for the next task, or a plan set after the
previous one finished, asks again. Only an `Act` turn is armed: an evidence
turn (Explain, Research) may plan its reading, and approval there would seed
an implementing turn the operator never asked for.

A Plan-disposition turn (operator `/mode plan`, or intake inferring Plan)
is not armed by the executor. Instead `pending_plan_approval_handoff`, which
every loop already calls after each recorded batch, requests exit on the
model's behalf once the turn has recorded a multi-step plan (fresh against
the ledger as the turn began) and a round adds no evidence: the question
comes after one idle round, not after the no-progress brake. The hook's
`plan_approval_due` also fires when the ledger gained a multi-step plan
during the turn, not only when a `render_report` draft was presented.

Measured on 2026-10-07 before that rule: under the Plan disposition a 35B
model recorded its plan, then re-loaded the same two skills round after
round (41 `use_skill` calls of 72 in the turn) until the twelve-round
no-progress brake stopped it; the approval question then followed anyway.
The repeat guard now returns a short successful reuse receipt for an exact
repeat while the skill body remains in context, instead of re-serving the
body or refusing the call. Committed compaction and successful workspace
changes release that memo, just like a file-read memo, so the next identical
load executes and returns the skill body again.

## Opt-in: ask the model for one sentence of intent

`[tui] narration_intent_line = true` appends one sentence to the system prompt
at build time, beside the plan-before-coding sentence, asking for one short
sentence of intent before each tool call *in the same reply as the call*.
Default off: new model-facing prompt text earns its place from a live run
(CLAUDE.md, "Harness design"), and prose with no call behind it is what the
narrate-then-stop rescue nudges, so the sentence must never ask for prose
alone.

## Collaborative openers

A prompt whose every ask opens collaboratively ("let's …", "we should …",
"how about we …", "shall we …", "I'm thinking …") is classified `Plan` before
the action needle is consulted, unless the remainder is a bare continuation
("let's go", "let's land it", "let's continue") or starts with a direct verb
("let's commit this", "let's run the tests", "lets push"), which still act.
The openers and the direct verbs are lexicon data
(`DispositionLexicon::collaborative`, `DispositionLexicon::direct_verbs`,
both overridable from the `[intake]` table); the continuation exception
reuses `classifiers::is_bare_continuation`.

## Tests

All in the mocked unit tier:

- `display_tests/narration.rs`: the edge table for the row builder (JSON-only
  content, prose plus fenced call, prose plus call in one paragraph, think
  blocks paired and unterminated, function-tag dialect with a stray closer,
  headings, lists, dotted tokens, width fitting, whitespace collapse).
- `scheduled.rs`: `step_change_line` on new, ticked, resent, rewritten, grown,
  finished and parked plans; `fresh_multi_step_plan` fires once.
- `tools_tests/execute_plan_disposition.rs`: the arm requests approval under
  a looking level and clamps a write later in the batch; an acting level sets
  the plan silently; single-step and Plan-phase plans never ask.
- `initiative.rs`: approval is asked only at the looking levels.
- `chat_tests/plan_approval.rs`: a Plan turn that set a ledger plan is asked
  without a draft; approval seeds the ledger plan when no draft exists;
  full-auto approves without asking.

The production narration emitter has injectable width, row budget and output.
`mod_tests/narration_output.rs` exercises that seam with one long paragraph
and mixed paragraphs at a narrow width, asserting the emitted row count,
actual hidden-row count, complete retained body and retention-before-marker
ordering. Streamed/empty and unlimited-budget controls share the same path.
The provider-loop call sites retain their existing wiring.

`compression_loop_tests::compaction_2799_releases_skill_reuse_in_real_loop`
runs repeated skill loads through the actual chat loop and committed
compaction. Request-length drops and summarizer calls identify compaction
independently of tool events; the next skill event must be a fresh successful
load, while intervening repeats are successful cache reuse. The guard's unit
test also covers invalidation after a successful workspace change.

## Known gaps

Found by review and left as they are, each with the reason.

- An intake-inferred Plan turn is read-only through the tool catalog and the
  dispatcher (`tool_allowed`, the Plan promotion, `permission_gate = None`);
  the turn's caveats are not met with `plan_phase_clamp()` the way `/mode
  plan` meets them. Pre-existing, and wiring the caveat path is authority
  work for its own review. A first attempt here only relabelled the
  disposition's provenance to the model and was reverted.
- A bare "do it" or "go" typed after a round cap interrupted a Plan turn
  resumes that objective as Plan, exactly as "continue" and "proceed" already
  did; the same words typed at the approval question approve. A continuation
  means "the same task", and that is the existing contract.
- A Plan turn whose model answers in prose only, with no `update_plan` and no
  `render_report`, ends with no question. The next acting turn's first plan
  is caught by the executor arm instead.
- A two-sentence opener ("Let's refactor X. Keep the tests green.") splits
  into two asks and the second is an order, so the prompt acts. Every ask
  must propose; a proposal is never inferred from one ask among orders.
- `[tui] narration_intent_line` is read from the prompt builder's existing
  publishing resolve rather than the `Config` the chat loop already holds;
  threading that value is a builder-signature change for a later slice.

## Out of scope

The `edit` approval option and the RichTUI plan pane (parent design); a
`/goal` verb or the `resolute` tenacity level (a declared check the harness
runs before accepting "done"); rendering each dial's `describe()` in the psyche
panel; reading or editing the composed system prompt from the TUI.

## Publish one extraction before expanding the refactor (#2831)

The plan guidance and existing workflow/narration nudges ask refactor objectives
that include a worktree and PR to use one cohesive extraction per publication
cycle: extract **and wire it in**, check, commit, push, open the PR. Later
extractions are follow-up commits on that PR. This is advice, not a new gate.

The shared worktree result annotation observes typed execution outcomes and
uses the existing conservative Cargo command/cwd parser. A passing `cargo check`
records a canonical content address of the staged paths, modes and blob IDs,
only when each working file's raw bytes hash to its indexed Git blob ID and no
non-ignored untracked files remain. `grit-lib` supplies the in-process index
parser and Git SHA-1/SHA-256 blob hashing; `content-addressable` supplies the
canonical witness identity. A subsequent commit must contain that same tree:
local loose/packed HEAD objects are read in-process and compared to the index.
There are **no production Git subprocesses on this advisory path**: no filters,
fsmonitor, hooks, external diff or lazy-fetch helpers can run, even if a writer
changes config after screening. No objects or index entries are written.

This conservatively requires staging before checking. Attribute files on tracked
paths (including info/attributes), configured attributes/filters, autocrlf or EOL
conversion, sparse/split/assume-unchanged indexes, symlinks, submodules, unavailable
metadata and bounded read grants decline the hint. Conversion is never modeled
or applied. Config/attribute rechecks can suppress stale advice; execution safety
does not depend on the timing of those rechecks.

There is one current checked-content witness, not a cache of passes per branch.
Failed/unknown outcomes, unsupported Cargo spellings (even masked exit-zero
commands), changed content and intervening non-commit reflog transitions discard
it. An intervening command without an attributable check or new commit also
discards it; unknown wrappers cannot silently retain a pass. The reflog prefix
is content-addressed too, so switching away and back within
one tool call cannot revive a pass. The branch-local flags retain only the
requested workflow, prior governed PR creation and whether advice was shown.
Model prose is never evidence. Absence from the governed ledger is not a remote
GitHub query: the hint says no creation was observed and tells the model to update
an existing PR if there is one. No network access or publication is triggered.

Writes exceeding 1,500 lines receive a short split hint, never a refusal. The
threshold leaves headroom over the measured successful 171–1,168-line extractions
and flags the failed 6,096-line copy that was never wired into its parent. File
writes include copied lines; checked moves count the extracted child; edits
count the replaced/inserted span rather than the whole existing file.

Unit tests cover the predicate, one-shot state, latest-check outcome, PR
suppression, strict command attribution, reflog interpretation and threshold.
A scripted ChatCtx loop with a real linked Git worktree and local Cargo check
stages the extraction before checking and asserts the next provider request
receives the hint after committing that content. Real Git observer controls
cover shell edits, ambiguous checks and switching away and back. A deterministic
post-screen config/attribute mutation test asserts that clean/process filters
cannot write an outside sentinel, alongside ordinary-content controls. The tests use
no sleeps or model endpoint. Refactor-lab before/after runs remain the operator's
separate behavioral measurement; these tests do not claim model success.

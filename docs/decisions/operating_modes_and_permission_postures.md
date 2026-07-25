# Decision: separate operating modes from permission postures

**Status:** Accepted
**Date:** 2026-07-24

Newt has two independent session controls:

| Control | Command | Purpose | Authority effect |
|---|---|---|---|
| Operating mode | `/mode` | Guide how the harness and model approach work | None, except `plan` and `diagnose` may narrow the turn to read-only |
| Permission posture | `/posture` | Apply configured skill/framing and an optional OCAP permission floor | A configured floor is meet-only; never widens |

The old `/mode <configured-name>` permission surface overloaded behavior and
authority. That made a phrase such as "developer mode" ambiguous: did it mean a
workflow, a permission grant, or both? The split makes the security claim
visible in the command vocabulary. An operating mode never grants permission,
and a posture never silently changes the requested workflow.

## Operating modes

`/mode` lists the active mode and a human-readable description of every choice:

- `chat` — collaborative default.
- `dev` — TDD, worktree-safe Git behavior, targeted tests, and the workspace's
  full preflight before proposing or pushing a PR.
- `admin` — do no harm, make minimal changes, respect privacy, and treat
  elevated power responsibly.
- `plan` — write a plan without mutating files or external state.
- `diagnose` — gather evidence and identify root cause, then ask whether to
  switch to `plan`.
- `auto` — let the model select `chat`, `dev`, `admin`, `plan`, or `diagnose`
  for a later action-shaped turn through the presence-gated
  `select_operating_mode` tool. The transition is conversation-local and
  next-turn-only: it cannot change the disposition, caveats, or permissions
  already frozen for the current turn. Protected ask, research, explanation,
  and plan intake wins over a stored action style. Before the model makes a
  selection, a deterministic fallback maps action, research, and explanation
  intake to `dev`, `diagnose`, and `chat`, with explicit planning or
  administration language selecting those bounded styles.
- `full-auto` — carry safe, in-scope work to completion with minimal
  interruption; it still cannot bypass permissions or unresolved human
  decisions.

The human-selected mode is session-scoped and survives `/new` because it is
composed into the ephemeral system prompt on every turn. A model-selected Auto
style is conversation-scoped and is cleared at a conversation boundary or by
an explicit human `/mode` selection. Neither is appended to the frozen
conversation prompt.

`plan` and `diagnose` are enforced, not merely suggested. The same effective
`PromptIntake` feeds the model card, durable artifact, advertised catalog, and
dispatcher:

- `plan` uses a dedicated Plan disposition. It permits reads and the
  harness-owned `update_plan` ledger. It may also exit a model-entered legacy
  plan phase, which only removes that self-clamp and cannot override a human
  `/mode plan`. It denies workspace writes, command execution, network access,
  permission grants, and generic MCP tools.
- `diagnose` uses the bounded Research disposition and cannot update the plan
  ledger. It may gather remote read-only evidence when the underlying network
  posture permits it, but cannot write the workspace or spawn commands.

Both modes also meet the turn's caveats with a read-only clamp. Plan's clamp is
offline; Diagnose preserves only the session's existing network reach for
read-only evidence. The catalog and caveat layers therefore fail closed if a
model fabricates a mutating tool call, without advertising `web_fetch` in
offline Plan.

The older model-entered `enter_plan_mode` phase is still recognized. It is
reported to the model as the effective `plan` style so guidance and enforcement
agree, and its clamp applies immediately to later tool calls in the same
inference round. Its state is injected per TUI session rather than stored
process-wide. Any explicit human `/mode` selection, new conversation, or
restored conversation clears that task-scoped phase before applying the
selected operating mode.

`dev` and `full-auto` supply explicit TDD, worktree-safety, verification, and
preflight guidance to the model. This decision does not claim that prose is a
push interceptor: a mandatory harness-owned `/preflight` gate is separate work.

## Permission postures

`/posture <name>` resolves the existing configured binding and atomically
applies every component it names: skill, framing, and an optional permission
preset. A binding may intentionally contain only skill or framing; in that case
it does not alter authority. An explicitly named preset or skill that cannot be
resolved is still a hard error. For compatibility, these bindings remain stored
under `[modes.<name>]`; changing that public TOML schema requires a separate
migration.

Posture skill text, custom framing, and the computed permission-floor summary
when present are also composed from live state on every turn. Consequently:

- `/posture off` removes both enforcement and model guidance;
- switching postures cannot accumulate contradictory prompt fragments; and
- `/new`, resume, or persona prompt rebuilds cannot silently discard the active
  posture.

The OCAP rule is unchanged: effective authority is
`session_base.meet(posture_clamp)`. With no configured preset the clamp is the
identity, so authority is unchanged; a configured preset can only narrow.

## Future advanced planning and crew orchestration

Advanced orchestration is an explicit opt-in layer, not default `/mode plan`
behavior:

- A **Plan** is an ordered, executable unit of work.
- An **Epic** is a plan of Plans.
- A `crew` tool may receive a bounded context capsule plus a prompt for an
  entire Plan or one Plan item. A delegated crew may recursively call `crew`
  for its own Plan items.
- Each delegation pushes the caller's conversational context and memory onto a
  harness-owned stack. When the child returns, the harness records its bounded
  result, pops the stack, and restores the caller's exact context and memory
  before continuing.
- A crew may reuse the current model or select among configured backends under
  explicit orchestration policy. Merely entering `plan`, `auto`, or `full-auto`
  never enables recursive delegation or cross-backend selection.

That design needs explicit depth, context, token, authority, cancellation, and
failure-propagation bounds before runtime implementation.

## Out of scope

- Persisting an operating mode across process launches.
- Letting a model select `full-auto` implicitly.
- A `/tdd` workflow command or mandatory `/preflight` push gate.
- Runtime Epic execution, recursive `crew` calls, and context-stack restoration.
- Dispatch topology (`single`, `crew`, `mesh`, `remote`). That is an orthogonal
  future control and must not overload either `/mode` or `/posture`.

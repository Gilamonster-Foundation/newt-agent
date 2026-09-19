# Psyche: three effort dials and the `/obsessive` toggle

**Status:** design note (pre-build) · **Amends:** [`guide/personality.md`](../guide/personality.md) · **Builds on:** `Tenacity` ([`tenacity.rs`](../../newt-core/src/tenacity.rs)), `Cognition` ([`role_profile.rs`](../../newt-core/src/role_profile.rs)), the obsessive posture ([`psyche.rs`](../../newt-core/src/psyche.rs)) · **Prior art:** [`thinking-effort-and-plan-mode.md`](thinking-effort-and-plan-mode.md)

## The problem

`/psyche` was meant to show effort as two sliders: how hard the agent thinks,
and how hard it pursues the task. The code has both dials, but one of them
drifted.

- **Tenacity became impatience.** Its levels change how many read-only rounds
  the loop tolerates before nudging the model to edit
  (`read_only_nudge_after`: 6, 3, 2, 1), and whether `exit_plan_mode` must
  hand off to an edit. Higher tenacity acts *sooner*. Only `relentless`
  touches pursuit, by lifting the tool-round limit.
- **One dial carries two intents.** Tenacity is raised per model family to
  correct small models that read and never act. That is harness tuning for a
  model's habit, not an operator's wish, yet it shows up as the operator's
  tenacity.
- **The max-everything posture has no off switch.** `--obsessive` and
  `/psyche obsessive` turn it on; nothing restores the dials it replaced.

## The design

Three dials, each answering one question. Every level names a behaviour the
harness can be held to.

```
cognition:  zen — rational — thoughtful — meticulous — exhaustive
tenacity:   normal — grit — resolute — relentless
initiative: survey — measured — decisive — eager
```

### Cognition — how deeply it thinks at each step

| Level | Behaviour |
|---|---|
| zen | Least reasoning the endpoint allows (`reasoning_effort = minimal`) |
| rational | Short reasoning before acting (`low`) |
| thoughtful | **Default.** Weighs options (`medium`) |
| meticulous | `high`, plus one self-review of its diff before reporting done |
| exhaustive | The highest level the endpoint advertises, plus considering alternatives before committing to one |

`meticulous` and `exhaustive` differ from their neighbours by harness
behaviour, not only by the wire value.

### Tenacity — how long and hard it pursues the task

| Level | Behaviour |
|---|---|
| normal | **Default.** Stops at the first plausible finish, within the configured round limit |
| grit | Also retries, a bounded number of times, when a tool call or test fails |
| resolute | Also refuses to report done until a check passes — a test run or other verification, not only the cited-path check [`claim_check.rs`](../../newt-core/src/agentic/claim_check.rs) does today |
| relentless | Also lifts the tool-round limit (today's `project_tool_round_limit`) |

Levels are cumulative. `grit` and `resolute` are new behaviour; `resolute` is
aimed at the measured false-completion rate on Terminal-Bench.

### Initiative — how much it looks before acting

| Level | Behaviour |
|---|---|
| survey | Nudge to act after 6 read-only rounds |
| measured | After 3 — today's historical default |
| decisive | After 2; `exit_plan_mode` must hand off to an edit |
| eager | After 1; `exit_plan_mode` must hand off to an edit |

This is today's tenacity mechanism, moved unchanged. Its **default comes from
the model family**: a family that over-explores starts at `decisive` or
`eager`, and the panel says so — `initiative: decisive (model default)`.
`auto` returns a slider to that default. Nothing is hidden; the family only
supplies the starting point.

All three dials keep today's precedence: CLI flag, then persona, then the
config's per-family value, then the config default, then the built-in default.

## The `/obsessive` toggle

`/obsessive` sets cognition to `exhaustive` and tenacity to `relentless`, and
enables crew at the next launch (crew is a startup gate). `/obsessive off`
restores the dial values in effect before it was turned on. It leaves
initiative alone: deepest thinking and acting after one round pull against
each other. The status line shows it while on.

It is a toggle, not a `/mode`. `/mode` selects authority — each mode changes
the caveats a turn runs under. Effort must never change authority, so it
cannot share that switch. `--obsessive` at launch stays.

## Migration

No backward compatibility is owed. One rename, one importer for files users
already have:

| Today | Becomes |
|---|---|
| cognition `glancing` / `pondering` / `deliberating` / `contemplating` | `zen` / `rational` / `thoughtful` / `meticulous` |
| tenacity `relaxed` / `standard` / `insistent` | initiative `survey` / `measured` / `decisive`; tenacity `normal` |
| tenacity `relentless` | initiative `eager` **and** tenacity `relentless` |
| config `[tenacity]` per-family table | `[initiative]` per-family table |

The importer rewrites persona front-matter and config keys once, on load, and
reports what it changed. `exhaustive`, `grit`, and `resolute` have no old name.

## Out of scope

- The five communication-style sliders in `/psyche`; they are unchanged.
- Tuning the numbers: nudge thresholds keep today's values; retry and
  round-limit budgets are chosen during implementation and measured.

## Open questions

1. `meticulous` and `exhaustive` add harness passes (self-review,
   alternatives). Should those be separate techniques the dial composes, as
   [`thinking-effort-and-plan-mode.md`](thinking-effort-and-plan-mode.md)
   proposes for `effort`?
2. Should `grit`'s retry count and `relentless`'s budget be visible in the
   panel, or only in config?
3. Slices: rename + importer first (behaviour-preserving), then `resolute`,
   then `grit`, then the cognition passes?

---
name: herdr-dispatcher
description: "Conduct a multi-lane engineering effort from Herdr panes: dispatch task briefs to agent panes, verify pickup by side effect, monitor by artifacts not titles, steer corrections back to the owning lane, and keep merge/CI/cleanup authority in the conductor. Use when driving parallel implementation agents from herdr, when a dispatch seems ignored, or when a pane wedges."
---

# herdr-dispatcher — conduct work through Herdr panes

Distilled from the newt-agent #1803 epic (~60 merges conducted this way).
Two roles, never blurred:

- **Conductor** (you): specification, dispatch, review, CI-watching, merges,
  worktree cleanup, operator status. The conductor edits code only to
  unblock trivially (a one-line compile fix when a lane's context is spent).
- **Lanes** (agent panes): implementation, red-first tests, pushes, PR
  bodies. A lane never merges — every brief ends with "Merge nothing."

Place models deliberately: judgement-heavy lanes get the strong model,
mechanical lanes the fast one (`/model` in the pane before the brief).

## Specify

**The spec goes in the tracker; the brief is only the envelope.** File the issue
first and let the brief say "read issue #N IN FULL — it is the spec." A brief
that carries the spec inline dies with the lane's context: after a `/clear`, a
wedge, or a handover, there is nothing left to re-read, and the replacement lane
reconstructs the requirements by guessing.

**Say where the plan is WRONG.** A lane handed a stale design document
implements it faithfully — that is the behaviour you selected for. If a
published plan has been overtaken, the issue must name the clause and override
it in as many words ("PLAN.md says X; this issue supersedes it, because Y"),
with the evidence. Silence is read as assent, and the lane has no standing to
overrule a checked-in document on its own.

Whatever a package does NOT get to decide is worth a line too, or two lanes will
each decide it. Shared types, shared file layout, a name both will touch:
settle it in the issue before dispatch, not in review afterwards.

## Dispatch

1. **Pick a pane** with `herdr agent list`. `agent_status` of `done`/`idle`
   means available. **Titles are stale** — panes keep the title of a task
   finished hours ago. Never route or reason by title; decide by status
   plus what the pane last said (`herdr pane read <pane> --lines 15`).
   **Lanes need not all be the same agent.** `herdr agent prompt` drives a
   codex pane exactly as it drives a claude one, and a mixed fan-out is often
   the right shape — the conductor's judgement is the scarce resource, not the
   lane's. What changes is the tell you read: a working codex pane shows
   `Working (Ns • esc to interrupt)`, not `✳`.
2. **Write the brief to a file** (`/tmp/msg_<task>.txt`), then:

   ```bash
   herdr agent prompt w1:pXX "$(cat /tmp/msg_<task>.txt)"
   ```

   Keeping the file matters: a wedged pane gets the same brief re-dispatched
   verbatim after recovery.
3. **Brief anatomy** — the parts that earn their keep:
   - The task, and "read issue #N IN FULL first — it is the spec."
   - **Hard constraints with file:line evidence.** State each as a
     falsification ("X was tried and is wrong because Y at Z"), not a
     preference. Constraints without evidence get relitigated.
   - **Method**: red-first (failing lines pasted in the PR body), prove by
     mutation in both directions, twins for every guard (the check must
     fail when the thing it guards is genuinely absent). For flakes:
     reproduce with a hit rate before changing anything; "cannot reproduce"
     is an acceptable report, a blind fix is not.
   - Scope AND out-of-scope, so the lane can decline adjacent work.
   - The **fresh-worktree command verbatim**:
     `git worktree add ~/workspaces/.worktrees/<task> -b <branch> origin/main`.
   - Gates (fmt, clippy -D warnings, focused suites), PR conventions
     (`Fixes #N`, risk label, no session trailers, push over SSH),
     **"Merge nothing."**
   - **Required reply items**: design decisions taken and why, red-first
     failing lines, PR number, and the head SHA *read back from origin* —
     never a SHA the lane only believes it pushed.
4. **VERIFY PICKUP — every dispatch, no exceptions.** Dispatches are
   sometimes silently swallowed (one cost 4.5 hours). `agent_status:
   working` proves nothing. Confirm by side effect within a few minutes:
   the worktree exists, or the pane transcript shows the brief being acted
   on. No side effect → treat as dropped and investigate the pane.

   Wait for it in the background rather than blocking on a sleep:

   ```bash
   until [ -d ~/workspaces/.worktrees/<task> ]; do sleep 10; done
   ```

   A careful lane reads its whole spec before touching the filesystem, so the
   worktree can lag the dispatch by minutes. Read the pane before concluding
   anything: "still reading" and "never received it" look identical from
   outside, and only one of them is a problem.
5. Stray text already sitting in a pane's input box is hard to clear
   remotely (`esc`, `dd` via `herdr agent send-keys` often don't take; many
   key names are unsupported). A queued stray line is low harm — dispatch
   anyway and let it ride ahead of the brief.

## Monitor

- **Poll artifacts, not the UI**: worktree creation, branch pushes, PR
  appearance, commits on the branch. A running Monitor pins the terminal
  title at ✳, so `herdr agent wait` / `prompt --wait` never settle —
  side effects are the only honest signal.
- Read a lane with `herdr pane read <pane> --lines N` (note: `read`, not
  `capture`). In vi-mode panes the first `esc` only leaves insert mode; the
  second interrupts the agent — count your escapes.
- **The conductor owns CI.** Tell lanes explicitly to drop CI-watching
  once the PR is up — a lane babysitting CI is a lane not working. Run
  your own heartbeat (a background Monitor on "new PRs" + periodic CI
  sweeps) as the re-invocation clock.
- **Wedged pane** (queue won't drain; Enter/C-c ineffective; "Press up to
  edit queued messages" persists):
  `herdr pane process-info --pane w1:pXX` → kill the agent PID →
  send-text `claude` + Enter to relaunch → `herdr pane report-agent` to
  re-adopt → re-dispatch the same brief file.
- **Binding dropped** ("No agent found" while claude is visibly running):
  `herdr pane report-agent` — idempotent; bindings can drop again
  mid-session.
- **Context exhaustion** (~900k tokens degrades): have the lane write a
  handover file, `/clear`, then dispatch a fresh brief that points at the
  handover. Don't push a degraded lane through a delicate change.

## Steer

- **Review every PR yourself before merging.** The body must show
  red-first evidence and twins; spot-check the diff at the risk points
  (lock discipline, containment checks, anything that could learn from
  unverified input). Merge-on-green is conditional on that review — and on
  the standing carve-outs (drafts untouchable, no releases, never weaken a
  test for green).
- **Corrections go back to the owning lane**, as a follow-up prompt
  carrying the exact error, run/job IDs, and the shape of an acceptable
  fix ("match the cfg boundary to the callers; a bare allow(dead_code) is
  the wrong answer unless argued"). Fixing in place steals the lane's
  context advantage and forks ownership.
- **CI triage**: rerun ONLY for same-tree nondeterminism (a true flake) —
  and a flake earns its own deflake issue/PR, not a third rerun.
  `gh run rerun` reuses the run's ORIGINAL merge commit and never picks up
  later fixes on main; anything that needs new code must be pushed to the
  branch.
- **After every merge, clean up**: `git worktree remove`, `git branch -D`,
  `git worktree prune`. One worktree per task; named worktrees are not
  auto-reaped.
- **When the operator recenters a design mid-flight**, rewrite the issue
  and the brief before re-dispatching — don't patch instructions into a
  moving lane.
- **Box discipline (gnuc)**: one whole-workspace gate at a time; the box is
  disk-bound, and saturation turns green red, never red green.
- **Status to the operator** on the requested cadence: outcomes first —
  what landed (with issue closures), what's in flight (lane → task), what's
  queued. Short beats complete.

## Related

- `herdr` — the pane/agent CLI itself (authoritative command reference).
- `gpg-signed-tag` — signing from an agent pane when a lane must tag.

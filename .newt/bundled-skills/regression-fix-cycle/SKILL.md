---
name: regression-fix-cycle
description: "Harden newt by running it on a real task until it completes unaided: run the task with the current build, turn every stall into a measured issue, fix the batch through lanes and reviewed PRs, rebuild from the new main, and run the same task again. Use when asked to make newt complete a task end to end (for example a progressive refactoring run from code to a PR merged on green), or to run a 'regression test and fix' loop."
when_to_use: When the operator asks for a regression-test-and-fix cycle, for newt to be tested on itself, or to repeat a failed newt run after its bugs are fixed.
version: 1.1.0
license: Apache-2.0
---

# regression-fix-cycle — run, file, fix, rebuild, repeat

Distilled from the first cycle on newt-agent (2026-11-25): eight issues from
one stalled refactoring session, nine PRs merged in a day, a rebuild, and a
retest that stalled earlier than the original and produced seven more issues.
That second list is the point. A fix batch is not done when its PRs merge. It
is done when the same task gets further on the new build.

One cycle has four stages. The exit condition is a **witnessed complete run**:
the task finishes with no human rescue. For a refactoring task that means
code, tests, PR, and merge on green.

## 1. Run the task with the build under test

- Write the task and the done condition down before starting, as one prompt
  file that every retest reuses unchanged.
- Record the build (`newt --version` prints the semver and the 12-character
  commit), the model and backend, and the launch flags. A retest that changes
  any of them is a different test: say so in the run record.
- Launch flags decide what can stall. A default session has no workspace write
  authority. `--workspace-access` is the usual launch for a coding task. Run
  the default once anyway: it found a defect the wider launch hides.
- **One operator.** Decide who answers the permission prompts before the run.
  Two people answering in the same pane leaves grants nobody can account for.
- **Do not rescue.** Answer permission prompts as an operator would: routine
  tools for the session, never a permanent grant, deny broad roots outside the
  task. Typing "continue" is allowed and is recorded as a finding.
- Keep a run ledger: for each run the launch, the end reason, round trips,
  tokens, wall clock split into operator wait and execution, and the stall.

## 2. Turn each stall into an issue

One defect per issue. Each issue separates what was **measured** (ledger,
permission log, pane output: quote it), what is **derived from the code**
(file and line at the commit), and what is **believed** or **not determined**.
Give a failing-test-first instruction and a "Done when".

Where to read the evidence:

- the usage log: end reason, attempts, tokens, elapsed time;
- the conversation store: the turn's tool events in order, with `ok`,
  `duration_ms` and `operator_wait_ms`, and the stored reply;
- the permission log: every decision with its scope and reason;
- the pane scrollback: the exact refusal text.

Before fixing, verify every "derived" claim against the code at that commit
and post corrections on the issue. In the first cycle six of eight issues
needed one: an undercount, a recommended fix that could not be written where
the issue put it, and causes the issue did not list.

Stop probing when new attempts repeat a known stall. In the first retest every
"continue" cost about three million input tokens and hit the same defect.

## 3. Fix the batch

- One issue, one branch, one PR. Coding goes to lanes (`herdr-dispatcher`).
  The conductor reviews every diff, pushes through the pre-push hook, opens
  the PR and merges on green. A lane never pushes or posts.
- The brief is an envelope: issue text, corrections, worktree, branch. The
  issue is the spec.
- Red first. The PR body shows the failing run, then the passing run.
- Lanes run the crate-scoped gates, and every `cargo test ... -- --<flag>`
  step in `ci.yml` that touches what they changed. Some test binaries are
  `harness = false` and nextest never runs them.
- **Check the fixes together before merging them apart.** Merge every open fix
  branch into one scratch tree and run clippy and the hook's test filter
  there. Then each PR can merge when its own CI is green, without a rebase
  and a fresh CI round per merge.
- A red job on an unrelated PR is evidence. Read the log before rerunning. In
  the first cycle one was a stale lockfile that made every PR red, and two
  were flaky tests that earned their own issues.
- **A flake seen twice goes into the batch.** The first cycles reran three
  known flakes again and again to land fix PRs, a full CI round each time,
  and the task's own "merge on green" would have met the same three.
  Follow "Flaky tests" in `CLAUDE.md`: one rerun, then a p1 fix.
- A PR closes its issue only when the issue's "Done when" is fully met.
  Anything that needs the rebuilt binary stays open for stage 4.

## 4. Rebuild and retest

- Build from the new `main` once every fix in the batch is merged. Install it
  beside the daily binary, never over it.
- First run the live checks left open in stage 3 and close those issues with
  the evidence.
- Then run the stage 1 task again, unchanged. If it stalls, go to stage 2 with
  the new list. If it completes, record the witnessed run and stop.
- Expect a fix to expose what it was hiding. The ceiling added for one issue
  stopped a turn three rounds after it wrote a file, because writes in a task
  worktree were never counted as progress. Only the retest could show that.

## What must never leave the machine

Issues, PR bodies and comments are public. No session links, absolute local
paths, host names or usernames (see `CLAUDE.md`).

## Related

`progressive-refactoring` (the usual task), `herdr-dispatcher` and
`herdr-shepherd` (lanes), `green-the-board` (CI), `worktree-hygiene`.

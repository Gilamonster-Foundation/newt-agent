---
name: herdr-shepherd
description: "Be the conductor's deputy in a Herdr fleet: rescue lanes that stalled (quota, crash, a Codex out of messages), verify what they actually did against git and GitHub before acting on their transcripts, steer sibling panes, relay guards before they execute, and never fabricate a finish for work whose evidence lives only in a dead session. Use when a lead hands you 'shepherd the other panes', when a pane dies mid-task, or when you are asked to take over orphaned work."
---

# herdr-shepherd — keep the flock honest

Distilled from a fleet day on newt-agent (2026-09-13): four claude panes, one
codex pane, two build boxes, a spend-limit outage in the middle. The lead
(`herdr-dispatcher`) holds specification, review, CI and merges. The shepherd
holds everything that keeps the *lanes* truthful while the lead is busy:
rescue, relay, verification, and the report that says what is real.

Two things a shepherd never does: **merge**, and **post under the operator's
login without sign-off**. Everything else below is in scope.

## Verify before you act on a claim

A pane's transcript is a *claim*. GitHub and git are the *state*. Check the
state first, every time, even when the claim is your own lead's:

```bash
gh pr view <n> --json state,mergedAt,mergeCommit,headRefOid,baseRefName
git ls-remote --heads origin <branch>        # gone means merged-and-deleted or never pushed
git worktree list; herdr pane list          # who owns a branch RIGHT NOW
```

The day's cheapest catch was "#2289 is merged" — true, but only verifiable
by reading it back; the most expensive would have been a takeover of a
branch a live pane still owned. **Ownership = a live pane whose transcript
names the branch, or a worktree on it.** No owner → orphaned → yours to take,
in a fresh worktree of your own, with the lead told.

## Rescue

When a lane dies (spend limit, crash, quota, wedge):

1. **Read what it was doing** from the pane, then re-derive it from
   artifacts: branch pushed? PR open? CI state? uncommitted work in its
   worktree (`git -C <wt> status --short`)?
2. **Rescue the artifacts, not the narrative.** Tarball an abandoned
   worktree's uncommitted files before touching anything; never rewrite the
   dead lane's history.
3. **Do not fabricate a finish.** If the result exists only in the dead
   session's tool history — a citation check whose per-item verdicts never
   reached disk, a test run whose output was never captured — leave it for
   that session to resume and say so. Committing a document whose subject is
   evidence integrity with guessed evidence is the exact failure it warns
   about. "The 4 deliverables are safe on disk, uncommitted; the session
   must finish its own commit" is a complete, correct report.
4. **A workflow that returned all-clear with zero agents completed is
   vacuous.** After an outage, treat any "clear" from that window as not
   run; re-run before anyone merges on it.

## Relay guards before they execute

The lead will hand you exclusions — worktrees, branches, build dirs another
lane owns. Get them into the executing pane's instruction **before** it runs,
not after: if the command is still sitting unsent in its input box, append
the guard to the same instruction and have the pane confirm the list back.
Verify afterwards that the protected paths still exist. A guard that arrives
after `rm -rf` is a post-mortem.

## Take-over discipline

An orphaned PR that is red is yours to make green, not to redesign:

- One worktree of your own from the branch head (`git worktree add
  ~/workspaces/.worktrees/<task> -b <local> origin/<branch>`), never the
  dead lane's directory.
- Fix what CI names. Rebuild expected values **through the same production
  calls** the code uses (a receipt helper, a renderer) rather than pinning a
  literal, so the assertion cannot drift from the producer again.
- Follow the precedent in the file you touch (an inventory entry's trust
  class, a test's serial lane) instead of inventing a shape.
- Push to the same branch, `--no-verify` only where the repo's exception
  allows it, **report CI when it lands, not before** — "CI just started" is
  the honest status; predicted green is not.

## Remote boxes

The fleet may include a macOS and a Windows builder reached over ssh. What
cost round-trips, so you do not pay them again:

- Windows: ship a `.ps1` with `scp` and run it with
  `powershell -NoProfile -ExecutionPolicy Bypass -File`; inline `-Command`
  quoting breaks every time. `npm.cmd`, not `npm`. Keep the ssh session open
  for long jobs (a hidden `Start-Process` dies with the session). Cap the
  build (`CARGO_BUILD_JOBS=8`) — a 16 GB box OOMs at full parallelism.
- Non-interactive `codex exec` over ssh needs `ssh -n`, or it waits on stdin.
- Capture whole logs to a file on the box and grep them; a `tail -40` that
  drops the failing step costs a rerun.
- **Clean up your evidence files** on the box when done; leave the writeup
  in the repo's recovery notes, not the raw logs on someone's machine.

## Evidence in what you push

Every PR body: What this PR does / Test plan / Out of scope; the test plan
shows **measured** output (the command and what it printed), red-before and
green-after for a fix, the reproduction shape for a platform bug ("same root
cause, platform-appropriate error code"). Grade anything unrun as believed.
Run the pre-publish grep — hostnames, box names, home paths, session links —
before `gh pr create`. Risk label. `Fixes #N` only when it closes #N.

## Boundaries you hold for the lead

- **No permission laundering.** If a step needs `sudo` or an action your
  session was not granted, it is the operator's step: leave it typed but
  unsent in a pane, name it in your report, do not ask a sibling to run it.
- **No posting for the operator.** Reviews, issue comments, verdicts — the
  lead drafts and the operator signs. A codex lane once posted three "MERGE"
  verdicts under the operator's login that contradicted CI; that is the
  failure this line exists for.
- **Correct the brief when the code disagrees with it.** A lead's brief can
  name the wrong file or miss a second residual; say so with the file:line,
  and fix the real thing. The brief is the envelope, not the spec.

## Report

One line per pane, then what changed, then what is blocked and on whom:

```
research01: housekeeping done, 8G freed; sudo step left for the operator
epistemic-infra: 4 deliverables on disk, uncommitted — its own session must finish
#2289: fixed on its branch (1dade380), CI running; not merged
```

Send it to the lead (`SendMessage` to its session, or a prompt into its
pane) and end with `idle`. A shepherd that is idle and says so is worth more
than one that is busy and vague.

## Related

- `herdr-dispatcher` — the lead's half: specify, dispatch, review, merge.
- `herdr` — the pane/agent CLI itself.
- `worktree-hygiene` — the cleanup rules the take-over discipline assumes.

---
name: github-contributor
description: How to be a good outside contributor to a repo you do NOT own — scope small, write terse respectful PRs, keep them rebased and mergeable, take review criticism gracefully, and check in regularly WITHOUT overwhelming the maintainer. The maintainer's attention is the scarcest resource; every rule spends as little of it as possible.
when_to_use: When opening or maintaining a pull/merge request to an upstream project you don't control; when deciding whether to nudge a maintainer (usually: don't); when a PR goes stale, conflicted, or gets change-requests; when a first-time-contributor PR shows BLOCKED / action_required and you're tempted to think something's wrong. Also the reference for "how do I not annoy this maintainer" and for setting up watch-don't-poke PR monitoring.
version: 1.0.0
license: Apache-2.0
---

# Being a good GitHub contributor

**When:** you are contributing to a repository you do **not** own — a PR/MR to
an upstream project, where a maintainer you don't control decides if and when
your work lands. Read this before opening the PR, and again before every nudge.

**The prime directive:** *the maintainer's time and attention are the scarcest
resource in the whole exchange.* Every choice below optimizes for spending as
little of it as possible while still getting your change reviewed. You are
asking a busy stranger for a favor. Act like it.

---

## The mental model

You do not merge. You do not set the schedule. You cannot approve your own
work. Your only levers are:

1. **Make the change trivially easy to say yes to** (small, clean, tested, mergeable).
2. **Make the change trivially easy to understand** (terse, structured, self-explaining).
3. **Keep it ready** (rebased, green, conflict-free) so a "yes" never bounces.
4. **Get out of the way** (don't nag, don't argue, don't pile on).

Everything a maintainer has to do that *you* could have done for them is a tax
on their goodwill. Pay it yourself.

---

## Before you open the PR

- **One logical change per PR.** A bug fix OR a feature OR a refactor — never a
  bundle. Small PRs get reviewed in the gaps of someone's day; big ones sit for
  weeks because there's never a big enough gap.
- **Match the house style.** Read `CONTRIBUTING.md`, skim recent merged PRs, and
  copy the project's commit-message and code conventions *exactly*. Don't make
  the maintainer teach you their own norms.
- **Bring the tests.** Every fix ships a regression test that would have failed
  before and passes after. A fix without a test asks the maintainer to trust you;
  a fix with one lets them verify at a glance.
- **Self-review the diff first.** Read your own patch as if you were the
  reviewer. Delete debug noise, stray files, unrelated churn, commented-out code.
  The cleaner it arrives, the fewer round-trips it costs.
- **Run their CI locally if you can.** Mirror the project's checks (fmt, lint,
  test, and any project-specific `xtask`/`make`/`just` targets) so the PR is
  green on arrival. Red CI on your own commits is *your* job to fix, not the
  maintainer's to point out.

## Writing the PR itself — be terse

The PR description is an imposition on the reader. Make it fast.

- **Title:** one line, imperative, < ~70 chars, conventional-commit prefix if
  the repo uses them (`fix(pipeline): run compound command stages concurrently`).
- **Body:** the minimum that answers *what* and *why*, then *how you verified*.
  A tight template:
  ```
  ## Summary
  One or two sentences: what changed and why it matters.

  ## Test plan
  How you proved it works (new test name, repro steps, CI matrix).

  Fixes #NNN   ← only if it actually closes an issue
  ```
- **No walls of text.** No autobiography, no "I spent all weekend on this," no
  restating the diff in prose. If the code needs a paragraph to explain, that
  paragraph belongs in a code comment, not the PR.
- **Link related work with one line.** If several PRs came out of the same
  investigation, cross-link them so the reviewer sees the shape — a single
  sentence per PR, no more:
  > `Found alongside #1184 and #1242 during practical testing of brush as an embedded shell.`

## Do NOT overwhelm the maintainer

This is the failure mode that gets contributors quietly ignored.

- **Don't open a swarm at once.** A handful of small, related PRs is fine (and
  cross-link them). A dozen dumped in one afternoon reads as "review my backlog
  for me" and gets triaged to *later* — indefinitely.
- **Don't @-mention to demand attention.** No "any update?", "please review",
  "bump", "friendly ping @maintainer" every two days. Maintainers see their
  queue; pinging moves you *down* it, not up.
- **Don't argue in the thread.** If you disagree with a review, state your
  reasoning once, briefly, then defer. The repo is theirs.
- **Don't relitigate decisions** or reopen settled threads. Resolved is resolved.
- **Don't make them chase context.** Everything needed to review should be in the
  PR. Don't answer "why?" with "see our internal doc" — they can't.
- **One comment, not five.** Batch your replies. Ten notifications for one PR is
  ten interruptions.

**Rule of thumb:** if a message doesn't give the maintainer new information or
unblock *them*, don't send it.

## Keep the PR fresh — rebase discipline

A PR that goes stale is a PR that gets closed.

- **Rebase on the upstream default branch**, don't merge it in. `git fetch
  upstream && git rebase upstream/main` keeps history linear and reviewable;
  merge commits from `main` into your branch muddy the diff.
- **Resolve conflicts promptly** — a `DIRTY`/conflicted PR can't be merged even
  if approved, so it's dead weight until you fix it. This is *your* job.
- **Keep it mergeable and green** so that the instant a maintainer decides to
  merge, nothing is in the way. "Approved but now conflicting" wastes the exact
  attention you spent months earning.
- **Force-push with `--force-with-lease`** (never bare `--force`) when you
  rebase/amend, so you never clobber work you didn't fetch.
- **Automate the boring part.** A nightly job can rebase your open PRs onto
  upstream and force-with-lease *only when the rebase is clean and all checks
  pass* — touching nothing on conflict or red. That keeps PRs review-ready with
  zero daily effort. (We run exactly this for the brush PRs.)

## Check in regularly — without nagging

"Regularly" means *you* stay informed, not that you *contact* the maintainer.

- **Watch, don't poke.** Monitor your PRs for new reviews, comments, CI changes,
  merge-state changes, and release landings. Act on what you see; stay silent
  when there's nothing to do.
- **The "waiting on maintainer" state = do nothing.** If your PR is clean,
  green, mergeable, and has no change-requests, the correct action is *wait*.
  Not ping. Not rebase-for-no-reason. Wait.
- **Only re-engage when it's genuinely yours to move:** a conflict appeared,
  changes were requested, CI went red on your commits, or the maintainer asked a
  question. Then respond fast and completely.
- **A single, low-frequency, factual check-in is acceptable** if a PR has been
  silent for a long time *and* something material changed (e.g. "rebased onto
  latest; still passing" once, not weekly). When truly unsure whether the PR is
  wanted at all, one polite question beats months of silent hope.

A clean way to operationalize "watch, don't poke" is scheduled monitoring that
reports to *you*, not the maintainer: e.g. a **daily monitor** (news / CI /
merge-state / release landings, alert only on real change) plus a **weekly
action check** that sorts each PR into *needs-me* vs *waiting-on-maintainer* — so
the default answer to "should I do something?" is a visible "no."

## Taking criticism

Review feedback is the maintainer investing their time in your code. Treat it
as the gift it is.

- **Assume good faith and competence.** They know the codebase; you're the
  guest. "Why is it this way?" usually has an answer you don't have yet.
- **Address every comment** — either make the change, or reply with a short,
  specific reason. Never leave a review comment silently unaddressed.
- **Concede gracefully.** "Good catch, fixed in <sha>." moves faster than a
  defense. You're not losing; the code is winning.
- **Separate ego from patch.** Feedback is about the diff, not about you. Don't
  take it personally and don't perform contrition either — just fix it.
- **Push fixups, keep it easy to re-review.** Small follow-up commits during
  review (so the reviewer can see what changed since their pass); squash/clean
  up before the final merge if the project prefers linear history.
- **Say thanks once, sincerely, and move on.** Gratitude, not groveling.

## The first-time-contributor gate (know this one)

On many repos, a first-time / outside contributor (`author_association: NONE`)
has their PR workflows held in `action_required` — **CI won't run until the
maintainer clicks "Approve and run workflows."** Your PR shows `BLOCKED` with no
checks even though the code is fine. There is nothing to fix on your end and no
API to self-approve; only the maintainer can release it. **Once your first PR
merges, your association flips to `CONTRIBUTOR` and CI auto-runs on subsequent
pushes.** Don't mistake this gate for a problem with your patch, and don't ping
about it repeatedly — it resolves itself the moment they engage.

---

## Case study: our brush (reubeno/brush) contributions

Concrete application of everything above, all public PRs:

- **Three small, single-purpose PRs**, each one logical change:
  `#1184` (CommandInterceptor exec/open hook), `#1242` (run compound command
  stages concurrently), `#1244` (opt-in kill-on-drop for spawned commands) —
  all surfaced from real use of brush as an embedded shell, **cross-linked with
  one-line comments** so the maintainer saw they came from one testing pass.
- **CI parity locally + a fork CI mirror** to run the full OS matrix
  (Linux/macOS/Windows/wasm) the local box can't. This caught a **macOS-only
  test bug** (`/bin/true` absent on macOS runners); we diagnosed it with a
  throwaway probe workflow, fixed the test to use `/usr/bin/true`, folded the
  fix into the feature commit, and force-with-lease'd — so the maintainer never
  had to see or mention the breakage.
- **Nightly auto-rebase** onto `upstream/main`, force-with-lease **only** when
  the rebase is clean and every CI-parity check is green — otherwise it touches
  nothing and logs. PRs stay perpetually review-ready at zero daily cost.
- **Watch, don't poke:** daily + Friday scheduled monitors keep us informed;
  we sent the maintainer nothing but the code and the one-line cross-links.
- **The gate played out exactly as documented:** all three sat `BLOCKED` /
  `action_required` as first-time-contributor PRs. We waited. `#1244` was
  approved and merged; that flipped `author_association` to `CONTRIBUTOR`, CI
  now auto-runs, and `#1184`/`#1242` went `CLEAN` + mergeable — waiting only on
  review. At no point did nudging the maintainer enter the plan.

---

## Quick checklists

**Before opening a PR**
- [ ] One logical change only
- [ ] Matches repo conventions (read CONTRIBUTING.md + recent PRs)
- [ ] Regression test included, passes; would have failed before
- [ ] Self-reviewed the diff; no stray/unrelated changes
- [ ] CI-parity checks run locally and green
- [ ] Terse title + `## Summary` / `## Test plan` body; `Fixes #N` only if apt
- [ ] Related PRs cross-linked with one line each
- [ ] Privacy pass: no internal hosts/IPs/paths/secrets in code or description

**While the PR is open**
- [ ] Rebased (not merged) onto upstream default; mergeable + green
- [ ] Every review comment addressed (changed or briefly justified)
- [ ] Fixups pushed; force-with-lease only
- [ ] "Waiting on maintainer"? → do nothing, just keep it fresh
- [ ] No pings, no arguing, no pile-on

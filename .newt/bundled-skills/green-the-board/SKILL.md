---
name: green-the-board
description: Get main and every open PR back to green after a CI break. Fan out to diagnose all red pipelines at once, decide whether they share one root cause, fix that root ONCE (usually on main), then propagate the single fix by forward-merge instead of duplicating it per branch. Minimizes total changes and avoids conflicts.
when_to_use: When main's pipeline is red and/or several open PRs are failing CI, especially when the failures look related or platform-specific (a Windows-only or one-OS-only job while every other job is green). Reach for this before hand-patching each PR separately — the whole point is to avoid N copies of the same fix. Also the go-to for "clean up all my open PRs" or a recurring periodic pipeline sweep.
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: ["git", "gh", "cargo"] }
  fs_read: all
  net: { only: [] }
  max_calls: unlimited
---

# Green the Board

A repeatable playbook for cleaning up a broken CI board — `main` plus every
open PR — with the **minimum number of changes**. The core move is a synthesis
step most people skip: before fixing anything, find out whether the red
pipelines share **one** root cause. They usually do. Then you fix it **once**
and let the fix flow to the PRs, instead of hand-patching each branch (which
duplicates work and creates merge conflicts later).

> **The shape of the win:** one code edit on `main` + N conflict-free
> forward-merges — not N copies of the same fix.

## Why this works

Open PRs almost always contain (or were branched from) the current `main`. So a
bug that lands on `main` is **inherited** by every PR built on it — the PRs
aren't independently broken, they're carrying the same defect. That means:

- There is usually **one** thing to fix, not N.
- Fixing it on `main` and forward-merging into each PR lands the fix in every
  branch **without duplicating the edit** and **without conflicts** — provided
  the PRs don't themselves modify the broken region (verify this; see step 2).
- It respects a hard rule most repos share: **never push to `main`** — the fix
  goes through its own tiny PR, and the feature PRs stay independent.

## The method

### 1. Fan out and read every red pipeline at once

Don't fix the first failure you see. Inventory the whole board first.

```bash
# Every open PR, with mergeability + base/head.
gh pr list --repo OWNER/REPO --state open --limit 50 \
  --json number,title,headRefName,baseRefName,mergeable

# Per PR: which checks are red? (look for a SINGLE failing job across all)
for pr in <numbers>; do
  gh pr view $pr --repo OWNER/REPO --json statusCheckRollup \
    -q '[.statusCheckRollup[]|select(.__typename=="CheckRun")|"\(.conclusion) \(.name)"]|.[]'
done

# main's latest run + job breakdown.
run=$(gh run list --repo OWNER/REPO --branch main --limit 1 --json databaseId -q '.[0].databaseId')
gh run view $run --repo OWNER/REPO --json conclusion,jobs \
  -q '.jobs[]|"\(.conclusion) \(.name)"'
```

**Signal to look for:** the *same one job* is red everywhere (e.g. only "Windows
build + test") while everything else is green. That is the fingerprint of a
single inherited root cause.

### 2. Get the actual error, and confirm it's shared

Pull the failing log for `main` and each PR and compare the top error.

```bash
gh run view <run-id> --repo OWNER/REPO --log-failed \
  | grep -iE "error\[E|error:|unresolved|cannot find|panicked|FAILED" \
  | grep -viE "warning|waiting" | head
```

If `main` and every PR show the **identical** first error, you have one root
cause. Now check the topology to choose the propagation strategy:

```bash
git fetch origin main <each-pr-branch>
# Do the PRs already contain main's broken HEAD? (yes => they inherit it)
git merge-base --is-ancestor origin/main origin/<pr-branch> && echo "inherits main"
# Do the PRs touch the broken region themselves? (empty => forward-merge is conflict-free)
git diff origin/main...origin/<pr-branch> -- path/to/broken/file | grep -i "<broken symbol>"
```

- **PRs inherit the break and don't touch the region** → fix `main`, forward-merge. (The common case.)
- **A PR modifies the broken region** → that PR needs the fix applied *in its own diff* (a real commit), because a forward-merge would conflict there.
- **A PR fails on something *different*** → it has its own bug; handle it separately after the shared fix.

### 3. Fix the shared root ONCE, on its own tiny PR

Never push to `main`. Branch, make the minimal edit, and — critically —
**verify it against the exact command the failing CI job runs**, on the same
platform when the break is platform-specific. Read the workflow to get the real
command and flags:

```bash
grep -n "runs-on\|cargo \|--features\|-D warnings" .github/workflows/ci.yml
```

Then reproduce locally (example: a Windows-only Rust break, verified on a
Windows box):

```bash
cargo clippy --workspace --all-targets --features <same-as-CI> -- -D warnings   # exit 0
cargo test  --workspace --features <same-as-CI> --no-run                        # test bins link
cargo fmt --all -- --check
```

Open the fix PR (three-section body: **What this PR does / Test plan / Out of
scope**), wait for CI green, **merge on green**. This unblocks everything.

### 4. Propagate by forward-merge — do NOT duplicate the edit

Once `main` is fixed, pull it into each PR. Use a **merge** (adds a commit,
preserves the PR author's history), not a rebase/force-push.

```bash
# Docs-only / no-risk PRs: the GitHub "Update branch" button.
gh pr update-branch <pr-number>

# Code PRs: merge locally, RE-VERIFY the merged tree against the real CI command
# (the early error may have masked a second issue), then push.
git checkout -B <pr-branch> origin/<pr-branch>
git merge --no-edit origin/main         # expect 0 conflicts if step 2 said so
cargo clippy --workspace --all-targets --features <same-as-CI> -- -D warnings
git push origin <pr-branch>
```

Re-verifying each merged code tree locally (with a warm build cache it's cheap)
catches a *masked second failure* before you spend a full CI round-trip on it.

### 5. Confirm the whole board, then hand back

Wait for the fresh runs on `main` + every PR and confirm the previously-red job
is now green everywhere; all other jobs were already green.

```bash
for b in main <pr-branches>; do
  id=$(gh run list --repo OWNER/REPO --branch "$b" --limit 1 --json databaseId -q '.[0].databaseId')
  echo "$b: $(gh run view $id --repo OWNER/REPO --json conclusion -q .conclusion)"
done
```

**Do not merge the feature PRs yourself.** Getting their *pipelines* green is
the job; they merge on their own review. Report the board and flag the review
point.

## Diagnostic pattern: the platform-`cfg` break (worked example)

The example this skill was distilled from: `main` and all three open PRs failed
**only** the "Windows build + test" job with:

```
error[E0432]: unresolved import `crate::permissions::prompt_stdin_active`
```

Root cause: the function was defined `#[cfg(any(unix, test))]` (its only caller
was a `#[cfg(unix)]` interrupt watcher), but a refactor had moved its `use` into
an **unconditional** import block. On a Windows non-test build the symbol
doesn't exist → the import fails to resolve → compilation halts (killing both
the `clippy` and `cargo test` steps). The Unix CI jobs never saw it.

Fix — gate the **import** to match the definition and its sole use, rather than
making the function cross-platform (that only moves the failure to a Windows
dead-code / unused-import warning under `-D warnings`):

```rust
use crate::permissions::{ /* the always-available items */ };
#[cfg(unix)]
use crate::permissions::prompt_stdin_active;
```

**Generalize the tell:** when exactly one OS's job is red with an *unresolved
import*, *cannot find function/type*, or *unused import/dead code* error, suspect
a `#[cfg(...)]` mismatch between a definition and its import/call site. The
platform whose job is green is the one the `cfg` was written for. The fix is
almost always to align the *narrower* side's gate — most cheaply the import —
not to widen the definition.

## Guardrails

- **Never push to `main`.** The shared fix is its own PR; feature PRs stay
  independent.
- **Honest verification.** Run the *exact* command the failing CI job runs
  (right flags, right OS), not a convenient approximation. Don't `--no-verify`
  or relax a gate to get green — fix the underlying issue.
- **Forward-merge, not rebase.** Merging `main` into a PR "adds a commit" and
  preserves the author's history; force-pushing a rebase rewrites work that may
  not be yours.
- **One logical change per PR**, with the What / Test plan / Out of scope body.
- **Minimal, intent-preserving fixes.** Prefer the smallest edit that satisfies
  the gate over a broad refactor; note anything larger as a follow-up.

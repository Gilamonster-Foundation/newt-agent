---
name: worktree-hygiene
description: The per-PR workspace lifecycle — set up one ISOLATED git worktree per PR, build/verify without cross-worktree contamination, and tear the worktree down on handoff. The "clean the board" counterpart to green-the-board (green it, then clean it). Prevents the two failure classes a shared build tree breeds — false-green golden/snapshot/coverage results, and disk-filling abandoned worktrees.
when_to_use: Starting focused work on a PR or fix (create the worktree first); before trusting a golden/snapshot/coverage result on a box where a shared CARGO_TARGET_DIR may be set; and when a PR is merged or its pipeline is green and handed off (remove the worktree). Also the go-to for "clean up my worktrees", a disk that is filling with target/ dirs, or "why did this test pass locally but fail in CI".
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: ["git", "cargo", "du", "df"] }
  fs_read: all
  fs_write: all
  net: { only: [] }
  max_calls: unlimited
---

# Worktree Hygiene

The lifecycle discipline that makes `green-the-board` honest. Greening a board
is only trustworthy if each PR was built and verified in **isolation** — an
agent that greens a PR against a contaminated shared build tree can ship a
false green. This skill is the other half of that loop: **green it, then clean
it**, one worktree per PR, no shared state, nothing left behind.

> **The shape of the win:** every PR gets its own worktree and its own
> `target/`; a golden that passes did so against *this* PR's freshly-built
> binary; and when the PR is handed off the worktree is gone. No contamination,
> no 280 GB of orphaned `target/` dirs.

## Why this exists — the two failure classes a shared tree breeds

A single build tree shared across sibling worktrees (via a shared
`CARGO_TARGET_DIR` env var, or a cargo-config `[build] target-dir`) looks like
a caching win and is actually a correctness hazard:

1. **False-green golden/snapshot/coverage results.** Sibling worktrees at
   different commits or feature-flag sets overwrite the *same* build
   artifacts. A test that rebuilds a binary and captures its output can capture
   a **stale sibling's** binary instead of this PR's — passing locally against
   the old baseline while CI (which builds clean, per-checkout) fails. Coverage
   totals are likewise polluted by unrelated crates' uncovered lines.
2. **Disk-filling orphans.** Abandoned worktrees each carry a full `target/`.
   In this workspace they once reached **~280 GB** and pushed the disk to 93%.

Both are eliminated by the same move: **one isolated worktree per PR, removed
on handoff.**

## The method

### 1. Set up — one isolated worktree per PR

Never work a PR in the primary checkout, and never share a worktree between two
PRs. Create it under the repo's `.worktrees/` and branch by convention.

```bash
cd <repo>
git fetch origin main
git worktree add -b <step-NN.M-or-feat/...> .worktrees/<short-name> origin/main
cd .worktrees/<short-name>
```

If more than one coding agent runs in the same repo at once, each MUST get its
own worktree — concurrent edits in one tree corrupt each other.

### 2. Build & verify — refuse a contaminated build tree

Before you trust *any* result you will act on (a golden re-baseline, a coverage
number, a "tests pass" claim), confirm the build tree is this worktree's own —
not a shared one a sibling can overwrite.

```bash
# Is a shared target dir in force? (the contamination source)
echo "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-<unset — good>}"
cargo metadata --no-deps --format-version 1 | grep -o '"target_directory":"[^"]*"'
```

- **`CARGO_TARGET_DIR` unset / resolves to this worktree's `./target`** → trust
  the result.
- **Shared dir in force** → run the trusted build in an **isolated** target and
  verify the built artifact directly before believing it:

  ```bash
  ISO=/tmp/iso-<pr>-target
  CARGO_TARGET_DIR="$ISO" cargo build --bin <bin>        # this PR's source only
  "$ISO/debug/<bin>" <subcmd>                            # eyeball the real output
  CARGO_TARGET_DIR="$ISO" cargo test -p <crate> <golden> # re-baseline / verify here
  ```

  Or scrub it per-command: `env -u CARGO_TARGET_DIR cargo test …`. Note that
  unsetting in a *prior* shell call does not stick if the parent process
  exported it — scrub it **in the same command** as the build.

A golden/snapshot "passing" is **not evidence** when a shared target dir was
active. Re-baseline goldens (`NEWT_GOLDEN_UPDATE=1` or the project's equivalent)
only against a binary you built and eyeballed in an isolated tree.

### 3. Tear down — remove the worktree on handoff

When the PR is **merged**, or its **pipeline is green and handed off for
review**, the worktree's job is done. Getting the pipeline green is the goal;
you do not merge feature PRs yourself (see `green-the-board`). Either way, once
you are done working the branch:

```bash
git -C <repo>/.worktrees/<name> status --short   # nothing real uncommitted?
git worktree remove <repo>/.worktrees/<name>      # clean tree
# only-scratch left (e.g. *.profraw)? discard it explicitly:
git worktree remove --force <repo>/.worktrees/<name>
```

Preserve anything real first (uncommitted work, notes) — `--force` discards
untracked files, so look before you use it. Named worktrees under
`.worktrees/` are **not** auto-reaped on a normal exit; remove them yourself.
The backstops (a `SessionEnd` reaper for clean+pushed worktrees, a weekly
cron) exist so a *forgotten* worktree is eventually reclaimed — they are a
safety net, not a substitute for step 3.

### 4. Watch the disk

```bash
df -h / | tail -1
du -sh <repo>/.worktrees/*/target 2>/dev/null   # who is holding space?
```

Prefer `cargo check` / `cargo clippy` for validation; reserve
`cargo build --release` for actual deploys (after which the worktree `target/`
is disposable). Check `df -h /` before a large build.

## Diagnostic pattern: the shared-target false-green (worked example)

The story this skill was distilled from. Greening a stack of PRs, a golden test
— `newt-eval`'s `golden_help_surface_boundary` — **passed locally but failed in
CI on the same commit**. The test rebuilds `newt` via `ensure_worker_built()`
then captures its `newt help` through `locate_worker_bin()`, which **honors
`CARGO_TARGET_DIR` first**. With a shared `/tmp/.cargo-target` on the box, that
returned a **stale sibling-worktree binary** (old `/mode` help) matching the old
golden — while CI, building clean into a per-checkout `./target`, produced the
new `/posture` help and correctly failed.

**Generalize the tell:** a golden/snapshot/coverage test that is **green
locally but red in CI** (or vice-versa), especially one that shells out to a
freshly-built project binary, points at a **shared build tree**, not a flaky
test. Verify the built artifact directly; re-baseline only in an isolated
target. The environment whose result you can reproduce from a clean tree is the
truth.

## Guardrails

- **One worktree per PR; remove it on handoff.** Not one shared between PRs,
  not left dangling after merge.
- **Never trust a result from a shared build tree.** Isolate (`CARGO_TARGET_DIR`
  per-PR) or scrub (`env -u CARGO_TARGET_DIR`) for anything you will act on.
- **Never `rm -rf` a worktree or target by hand.** Use `git worktree remove`
  (which refuses to clobber real work), the project's sanctioned cleanup
  script, or move-to-graveyard.
- **Pairs with `green-the-board`.** That skill gets the pipelines green; this
  one keeps the workspace clean while doing it and tears it down after.
- Full workspace policy: `WORKSPACE_RULES.md` → "Agent disk & cleanup
  discipline" (don't re-add a shared target-dir in either form; "Green the
  board, then CLEAN the board — every PR").

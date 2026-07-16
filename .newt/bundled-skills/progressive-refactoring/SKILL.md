---
name: progressive-refactoring
description: Repeatable large-file reduction workflow for repositories with god modules. Use when asked to scan the top refactoring candidates, set a maximum file-size threshold, extract cohesive modules by pure code motion, open one-file-per-PR refactor branches, then repeat after each merge until every tracked source file is below the threshold.
when_to_use: When the user asks for progressive refactoring, top-10 largest-file cleanup, god-module reduction, repeated <name>/refactor PR passes, or a threshold-driven plan such as "get every file below 10,000 lines".
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: ["git", "gh", "cargo", "just", "rg", "sed", "wc", "sort", "head", "xargs"] }
  fs_read: all
  fs_write: all
  net: { only: ["github.com", "api.github.com"] }
  max_calls: unlimited
---

# Progressive Refactoring

A repeatable workflow for reducing oversized source files without behavior
changes. The unit of delivery is **one original source file per PR**.

## Operating Contract

- Treat the user-provided threshold as the line-count ceiling for tracked source
  files. If no threshold is provided, ask for it before starting implementation.
- Measure line counts with repo ground truth, normally:

  ```bash
  git ls-files '*.rs' | xargs wc -l | sort -nr | head -10
  ```

- Exclude lockfiles, binary assets, generated files, and vendored artifacts from
  the refactor candidate list unless the user explicitly includes them.
- Recompute the top-10 list after every merge. Do not rely on yesterday's list.
- Use the branch pattern `<name>/refactor` exactly as requested by the user
  (for Codex, usually `codex/refactor`).
- Keep each PR scoped to **one original oversized file**. It may add new module
  files and move that file's tests, but it must not refactor a second original
  file in the same PR.
- Prefer pure code motion. Do not mix cleanup, redesign, renaming, or behavior
  changes into the extraction pass.
- Preserve public APIs with re-exports or wrappers when needed.
- Keep test coverage the same or higher. If the user asks to increase coverage,
  add focused tests for moved seams or edge cases before opening the PR.

## Round Setup

1. Verify the worktree is clean:

   ```bash
   git status --short --branch
   ```

2. Refresh `main`:

   ```bash
   git fetch origin main
   git checkout main
   git pull
   ```

3. Clean up the previous refactor branch after its PR is merged:

   ```bash
   git branch -d <name>/refactor
   ```

   If the local branch is already gone, continue. If safe delete fails because
   Git cannot prove the merge, verify the PR merge first; do not force-delete
   without evidence.

4. Create the next round branch from current `main`:

   ```bash
   git checkout -b <name>/refactor
   ```

5. Print the current top 10 tracked source files and identify which files are
   still above the threshold.

## Choosing The File

- Pick the largest file above the threshold unless the user directs otherwise.
- If two files are close in size, prefer the one with the clearest cohesive
  extraction seams and the least cross-module risk.
- Load or apply the `functional-cohesion` skill before deciding seams.
- Name extracted modules by job, not by category. Avoid generic `utils`,
  `helpers`, or `types` buckets unless the existing codebase already uses that
  vocabulary for a cohesive subsystem.
- A good first extraction target has:
  - a cluster of functions used together for one job,
  - tests already adjacent to the cluster,
  - a narrow caller surface,
  - little or no public API exposure.

## Implementing One PR

1. Inventory the target file:

   ```bash
   rg -n "fn |struct |enum |mod tests|#\\[cfg\\(test\\)\\]" path/to/file.rs
   ```

2. Select one or more cohesive clusters from that **same original file**.
3. Move code and its co-located tests into new module files. Keep the first pass
   as mechanical as possible.
4. Rebalance imports and visibility:
   - make only the reached items `pub(crate)`,
   - keep module-private internals private,
   - re-export public API items from the original module if callers depend on
     their old path.
5. Add focused tests only where they improve safety or satisfy a coverage
   requirement. Good tests cover parser edges, formatting branches, overflow
   branches, or public re-export compatibility around the moved code.
6. After each meaningful extraction, run at least:

   ```bash
   cargo build --workspace
   ```

7. Before commit, run the local acceptance gate:

   ```bash
   cargo fmt --all
   just check
   just cov-ci
   ```

   If coverage changed, report the before and after percentages. Coverage must
   not go down.

## PR Discipline

- Commit only after verifying the exact staged diff.
- Include the required co-author trailer for LLM-authored work when this repo
  requires it.
- Push the branch:

  ```bash
  git push -u origin <name>/refactor
  ```

- Open the PR with these sections:
  - `What this PR does`
  - `Test plan`
  - `Out of scope`

- In the PR body, state:
  - the original file targeted,
  - the extracted module files,
  - the new line count of the original file,
  - the threshold,
  - the coverage result.

## Repeat Loop

After the PR merges:

1. Confirm the merge is on `origin/main`.
2. Delete or stop tracking the previous `<name>/refactor` branch as appropriate.
3. Refresh local `main`.
4. Recreate `<name>/refactor` from current `main`.
5. Recompute the top-10 tracked source files.
6. Start the next one-file PR for the largest remaining file above threshold.
7. Stop when every tracked source file is below the threshold or when the user
   ends the run.

## Reporting Format

When reporting candidates, include a compact table:

| Rank | Lines | File | Above Threshold? | Suggested Next Cut |
|---:|---:|---|---|---|

When finishing a round, report only:

- PR link,
- commit hash,
- target file and resulting line count,
- tests and coverage,
- next candidate above threshold.

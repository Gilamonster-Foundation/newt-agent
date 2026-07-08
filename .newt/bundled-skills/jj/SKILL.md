# Jujutsu (jj)

Execute `jj` commands for Jujutsu/QuiltFS operations. Modern Git-compatible DVCS with improved UX. Use when the workspace uses jj instead of Git.

## Prerequisites

- `jj` installed (`brew install jj`, or download from github.com/martinvonz/jj/releases)
- Initialized repo: `jj init --git-dir /path/to/repo/.git`

## Common Operations

### Status & Logs

```bash
# View working copy
jj status
jj diff

# Log (commit history)
jj log -L 20
jj log --all-commits -L 50
jj log --graph

# Show commit details
jj show abc1234
jj diff -c abc1234
```

### Branches & Remotes

```bash
# List branches
jj branch list
jj branch list --remote

# Create/update branch
jj branch set my-branch -m "Update description"
jj branch move old-name new-name

# Push/pull to remote (Git)
jj git push
jj git pull --rebase
jj git fetch
```

### Working Copy & Commits

```bash
# Create new commit
jj new
jj edit  # Alias for `jj new`

# Amend working copy as new commit
jj squash -c HEAD  # Squash WC into HEAD
jj squash -b my-branch  # Squash into branch tip

# Undo last operation
jj undo

# Discard changes
jj abandon  # Abandon WC commits
```

### Diff & Edit

```bash
# File diff (uncommitted)
jj diff

# Commit diff
jj diff -c abc1234

# Edit specific file in commit
jj edit abc1234 --file file.cpp
jj diff -c HEAD -- file.cpp
```

### Rebase & History

```bash
# Move commits to new parent
jj rebase -d main  # Move WC to main's tip
jj rebase -s abc1234 -d def5678  # Move commit range

# Interactively rebase (edit history)
jj interactive rebase

# Split/merge commits
jj split  # Split WC into multiple commits
jj squash  # Merge selected commits
```

### Undo & Restore

```bash
# Undo last operation
jj undo

# View undo log
jj op log -L 10

# Restore from previous state
jj restore --from <op> --to .
```

### Search & Help

```bash
# Search commits
jj log -m "fix bug"
jj log -u $USER

# Open help
jj help commands
jj help log
```

## Tips

- `jj` uses "commits" not "revisions"; each commit is immutable once created
- Working copy (WC) is a special commit; use `jj new` to create it
- Branches are lightweight pointers, not history markers
- Git backend: `jj git push/pull/fetch` for remote operations
- Conflict resolution: edit files, then `jj resolve --auto` or `jj squash -c HEAD`

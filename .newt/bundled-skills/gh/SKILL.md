# GitHub CLI (gh)

Execute `gh` commands for GitHub operations. Use this instead of web_fetch for GitHub APIs when a token is available.

## Prerequisites

- `gh` installed and authenticated (`gh auth login`)
- Token with appropriate scopes (repo, gist, etc.)

## Common Operations

### Repositories

```bash
# List repos
gh repo list --limit 50
gh repo list --owner octocat --type public

# Create/delete
gh repo create my-repo --public --description "A test repo"
gh repo delete my-repo

# Clone
gh repo clone owner/repo

# View info
gh repo view owner/repo --json name,description,url,defaultBranchRef
```

### Pull Requests

```bash
# List PRs
gh pr list --state all --limit 20
gh pr list --author @me --state open

# Create PR
gh pr create \
  --title "feat: add new feature" \
  --body "Description of changes..." \
  --base main \
  --head feature-branch

# View PR
gh pr view 42 --json title,body,author,createdAt,statusCheckRollup
gh pr diff 42 | head -100

# Merge/close
gh pr merge 42 --squash --delete-remote-branch
gh pr close 42

# Checkout PR locally
gh pr checkout 42
```

#### Draft PRs are a hard stop — never merge, never un-draft

A **draft** PR is an explicit "do NOT merge yet" signal from its author. It
is the mechanism people use precisely to keep a "merge on green" instruction
from firing on work that is not ready (missing a release gate, awaiting a
dependency, deliberately parked).

- **Never merge a draft** — GitHub blocks it, and forcing past it (marking
  ready just to merge) defeats the author's intent.
- **Never mark a draft ready** (`gh pr ready`) unless a human explicitly asks
  for that PR by number. "Merge on green" / "merge my PRs" does **not**
  authorize un-drafting anything — a blanket instruction never reaches a draft.
- When a "merge on green" sweep encounters a draft, **skip it and say so**;
  don't ask whether to promote it.
- Check before acting: `gh pr view <n> --json isDraft --jq .isDraft`. In a
  list, `gh pr list --json number,title,isDraft` — a `[draft]` tag means hands
  off.
- If a draft carries a release constraint (e.g. "hold until 0.8.0"), that
  belongs written down in the roadmap and linked to the PR, so the hold
  survives even if the branch is closed by accident.

### Issues

```bash
# List issues
gh issue list --state all --limit 20
gh issue list --author @me --label bug

# Create issue
gh issue create \
  --title "Bug: something broken" \
  --body "Steps to reproduce..." \
  --label bug,high-priority

# View/close
gh issue view 100
gh issue close 100
```

### Reviews & Comments

```bash
# Review PRs
gh pr review 42 --approve --body "Looks good!"
gh pr review 42 --changes --request-changes --body "Needs work"

# Comment on PR/issue
gh pr comment 42 --body "LGTM"
gh issue comment 100 --body "Fixed in commit abc123"

# List reviews
gh pr review list --limit 10
```

### Workflows & Actions

```bash
# List runs
gh run list --limit 10
gh run list --workflow ci.yml --status failure

# View/run details
gh run view 123456 --log
gh run watch 123456

# Re-run
gh run rerun 123456 --failed
```

### Releases

```bash
# List
gh release list --limit 10

# Create
gh release create v1.0.0 \
  --title "Release 1.0" \
  --notes "Changelog here..." \
  ./dist/app.tar.gz

# Download
gh release download v1.0.0 --pattern "*.tar.gz"
```

### Search

```bash
gh search repos "rust async" --sort stars --limit 20
gh search issues "bug open" --author @me --limit 10
gh search prs "review-requested:@me" --limit 20
```

## JSON Output

For programmatic use, always add `--json` with needed fields:

```bash
gh pr view 42 --json title,number,state,createdAt,author,repository
gh repo view --json name,defaultBranchRef,url
```

Available fields: see `gh api /graphql -f query='{...}'` or check docs.

## Tips

- Use `--web` to open in browser instead of CLI output
- Pipe diff to `diff` for side-by-side viewing
- Use `--repo owner/repo` when not inside a repo clone
- Environment: `GH_TOKEN`, `GITHUB_TOKEN`

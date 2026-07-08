# GitLab CLI (glab)

Execute `glab` commands for GitLab operations. Falls back to `git` + `gh` when glab is unavailable.

## Prerequisites

- `glab` installed (`brew install glab` or download from gitlab.com/gitlab-org/cli/releases)
- Authenticated: `glab auth login --hostname gitlab.example.com`

## Common Operations

### Projects & Repositories

```bash
# List projects
glab project list --limit 20
glab project list --owner username

# View/create
glab project view owner/repo
glab project create my-project --description "A new project" --visibility public
```

### Merge Requests (equivalent to GitHub PRs)

```bash
# List MRs
glab mr list --state opened --limit 20
glab mr list --author @me --state merged

# Create MR
glab mr create \
  --title "feat: add feature" \
  --description "Description..." \
  --source-branch feature-branch \
  --target-branch main

# View/diff
glab mr view 42
glab mr diff 42 | head -100

# Merge/close
glab mr merge 42 --remove-source-branch
glab mr close 42

# Checkout MR locally
glab mr checkout 42
```

### Issues

```bash
# List issues
glab issue list --state opened --limit 20
glab issue list --author @me --label bug

# Create issue
glab issue create \
  --title "Bug: something broken" \
  --description "Steps to reproduce..." \
  --label bug,high-priority

# View/close
glab issue view 100
glab issue close 100
```

### Reviews & Comments

```bash
# Review MRs
glab mr review 42 --approve --message "Looks good!"
glab mr review 42 --request-changes --message "Needs work"

# Comment on MR/issue
glab mr comment 42 --message "LGTM"
glab issue comment 100 --message "Fixed in commit abc123"
```

### Pipelines & CI

```bash
# List pipelines
glab pipeline list --limit 10
glab pipeline list --status failed

# View/run details
glab pipeline view 123456
glab pipeline logs 123456

# Re-run
glab pipeline retry 123456
```

### Releases

```bash
# List
glab release list --limit 10

# Create
glab release create v1.0.0 \
  --name "Release 1.0" \
  --description "Changelog here..." \
  ./dist/app.tar.gz

# Download
glab release download v1.0.0 --pattern "*.tar.gz"
```

## Tips

- Use `--json` for programmatic output (fields vary by command)
- Set `GLAB_API_URL` for self-hosted instances
- Environment: `GITLAB_TOKEN`, `GLAB_ACCESS_TOKEN`

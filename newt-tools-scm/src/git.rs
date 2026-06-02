//! SCM git tools backed by kyln-git.
//!
//! Each handler takes a JSON `args` value from the MCP `tools/call`
//! dispatch and returns an MCP content envelope (structured JSON wrapped
//! in the standard text content shape).
//!
//! The kyln backend auto-selects LibGit2 when available, falling back
//! to the CLI backend transparently.

use chrono::Utc;
use kyln_git::BlameOptions;
use kyln_git::GrepOptions;
use kyln_git::ListBranchesOptions;
use kyln_git::LogOptions;
use kyln_git::Signature;
use kyln_git::open_backend;
use serde_json::Value;
use std::path::Path;

// ── Tool definitions ──────────────────────────────────────────────────────────

/// MCP tool definitions for all `scm_git_*` tools.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        // ── Read ────────────────────────────────────────────────────────────
        serde_json::json!({
            "name": "scm_git_log",
            "description": "List git commits. Returns structured JSON with id, author, timestamp, message, parents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Absolute path to git repo root" },
                    "max_count": { "type": "integer", "description": "Max commits (default 20)" },
                    "author": { "type": "string", "description": "Filter by author name/email (regex)" },
                    "path_filter": { "type": "string", "description": "Only commits touching this path" },
                    "since": { "type": "string", "description": "Only commits after this date (ISO 8601 or '2 weeks ago')" }
                },
                "required": ["repo"]
            }
        }),
        serde_json::json!({
            "name": "scm_git_blame",
            "description": "Line-by-line authorship for a file. Returns commit id, author, timestamp, and content per line.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Absolute path to git repo root" },
                    "file": { "type": "string", "description": "File path relative to repo root" },
                    "revision": { "type": "string", "description": "Blame from this revision (default HEAD)" }
                },
                "required": ["repo", "file"]
            }
        }),
        serde_json::json!({
            "name": "scm_git_grep",
            "description": "Search tracked files for a pattern. Returns file path, line number, and content per match.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Absolute path to git repo root" },
                    "pattern": { "type": "string", "description": "Pattern to search for" },
                    "revision": { "type": "string", "description": "Search this revision (default: working tree)" },
                    "case_insensitive": { "type": "boolean", "description": "Case-insensitive match" },
                    "max_count": { "type": "integer", "description": "Max matches to return" }
                },
                "required": ["repo", "pattern"]
            }
        }),
        serde_json::json!({
            "name": "scm_git_diff",
            "description": "Diff statistics between two revisions. Defaults to HEAD~1..HEAD (last commit). Returns per-file insertions/deletions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Absolute path to git repo root" },
                    "from": { "type": "string", "description": "Start revision (default HEAD~1)" },
                    "to":   { "type": "string", "description": "End revision (default HEAD)" }
                },
                "required": ["repo"]
            }
        }),
        serde_json::json!({
            "name": "scm_git_status",
            "description": "Working tree status. Returns staged, unstaged, and untracked file lists.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Absolute path to git repo root" }
                },
                "required": ["repo"]
            }
        }),
        serde_json::json!({
            "name": "scm_git_branch_list",
            "description": "List branches (local and remote) with current HEAD marker and upstream tracking.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Absolute path to git repo root" }
                },
                "required": ["repo"]
            }
        }),
        // ── Write ───────────────────────────────────────────────────────────
        serde_json::json!({
            "name": "scm_git_branch_create",
            "description": "Create a new branch from an optional start point (defaults to HEAD).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo":        { "type": "string", "description": "Absolute path to git repo root" },
                    "name":        { "type": "string", "description": "New branch name" },
                    "start_point": { "type": "string", "description": "Commit/ref to branch from (default HEAD)" },
                    "force":       { "type": "boolean", "description": "Reset if branch already exists" }
                },
                "required": ["repo", "name"]
            }
        }),
        serde_json::json!({
            "name": "scm_git_branch_delete",
            "description": "Delete a local branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo":  { "type": "string", "description": "Absolute path to git repo root" },
                    "name":  { "type": "string", "description": "Branch name to delete" },
                    "force": { "type": "boolean", "description": "Force delete (even if unmerged)" }
                },
                "required": ["repo", "name"]
            }
        }),
        serde_json::json!({
            "name": "scm_git_commit",
            "description": "Stage files and create a commit. Supports author rewriting via author_name/author_email — use 'hartsock@users.noreply.github.com' for GitHub pushes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo":         { "type": "string", "description": "Absolute path to git repo root" },
                    "message":      { "type": "string", "description": "Commit message" },
                    "paths":        { "type": "array",  "items": { "type": "string" },
                                      "description": "Paths to stage (empty = stage all tracked changes)" },
                    "author_name":  { "type": "string", "description": "Override author name (default: git config user.name)" },
                    "author_email": { "type": "string", "description": "Override author email for identity rewriting" }
                },
                "required": ["repo", "message"]
            }
        }),
        serde_json::json!({
            "name": "scm_git_push",
            "description": "Push a ref to a remote. Raises PushRejected on non-fast-forward; pull first then retry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo":    { "type": "string", "description": "Absolute path to git repo root" },
                    "remote":  { "type": "string", "description": "Remote name (default 'origin')" },
                    "refspec": { "type": "string", "description": "Refspec to push (default 'HEAD')" }
                },
                "required": ["repo"]
            }
        }),
        serde_json::json!({
            "name": "scm_git_pull",
            "description": "Fetch remote and rebase the current branch on top. Returns AlreadyUpToDate, FastForward, or Rebased{N}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo":   { "type": "string", "description": "Absolute path to git repo root" },
                    "remote": { "type": "string", "description": "Remote name (default 'origin')" },
                    "branch": { "type": "string", "description": "Remote branch to pull (default 'main')" }
                },
                "required": ["repo"]
            }
        }),
    ]
}

// ── Read handlers ─────────────────────────────────────────────────────────────

/// Handle `scm_git_log`.
pub fn handle_scm_git_log(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;
    let backend = open_at(repo)?;

    let mut opts = LogOptions::new().max_count(20);
    if let Some(n) = args["max_count"].as_u64() {
        opts = opts.max_count(n as usize);
    }
    if let Some(a) = args["author"].as_str() {
        opts = opts.author(a);
    }
    if let Some(p) = args["path_filter"].as_str() {
        opts = opts.path(p);
    }
    if let Some(s) = args["since"].as_str() {
        opts = opts.since_date(s);
    }

    let result = backend
        .log(opts)
        .map_err(|e| anyhow::anyhow!("git log: {e}"))?;
    json_envelope(&result)
}

/// Handle `scm_git_blame`.
pub fn handle_scm_git_blame(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;
    let file = req(args, "file")?;
    let backend = open_at(repo)?;

    let mut opts = BlameOptions::new();
    if let Some(rev) = args["revision"].as_str() {
        opts = opts.revision(rev);
    }

    let result = backend
        .blame(Path::new(file), opts)
        .map_err(|e| anyhow::anyhow!("git blame: {e}"))?;
    json_envelope(&result)
}

/// Handle `scm_git_grep`.
pub fn handle_scm_git_grep(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;
    let pattern = req(args, "pattern")?;
    let backend = open_at(repo)?;

    let mut opts = GrepOptions::new(pattern);
    if let Some(rev) = args["revision"].as_str() {
        opts = opts.revision(rev);
    }
    if let Some(ci) = args["case_insensitive"].as_bool() {
        opts = opts.case_insensitive(ci);
    }
    if let Some(n) = args["max_count"].as_u64() {
        opts = opts.max_count(n as usize);
    }

    let result = backend
        .grep(opts)
        .map_err(|e| anyhow::anyhow!("git grep: {e}"))?;
    json_envelope(&result)
}

/// Handle `scm_git_diff` — diff stats between two revisions.
pub fn handle_scm_git_diff(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;
    let backend = open_at(repo)?;

    let from_spec = args["from"].as_str().unwrap_or("HEAD~1");
    let to_spec = args["to"].as_str().unwrap_or("HEAD");

    let from_id = backend
        .rev_parse(from_spec)
        .map_err(|e| anyhow::anyhow!("bad rev '{from_spec}': {e}"))?
        .primary()
        .clone();
    let to_id = backend
        .rev_parse(to_spec)
        .map_err(|e| anyhow::anyhow!("bad rev '{to_spec}': {e}"))?
        .primary()
        .clone();

    let stat = backend
        .diff_stat(&from_id, Some(&to_id))
        .map_err(|e| anyhow::anyhow!("git diff: {e}"))?;
    json_envelope(&stat)
}

/// Handle `scm_git_status` — working tree status via git CLI.
pub fn handle_scm_git_status(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;

    let out = std::process::Command::new("git")
        .args(["-C", repo, "status", "--porcelain=v1"])
        .output()
        .map_err(|e| anyhow::anyhow!("git status: {e}"))?;

    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git status failed: {msg}");
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut staged: Vec<Value> = vec![];
    let mut unstaged: Vec<Value> = vec![];
    let mut untracked: Vec<Value> = vec![];

    for line in text.lines() {
        if line.len() < 3 {
            continue;
        }
        let x = line.chars().next().unwrap_or(' ');
        let y = line.chars().nth(1).unwrap_or(' ');
        let path = &line[3..];

        if x == '?' && y == '?' {
            untracked.push(serde_json::json!({"path": path}));
        } else {
            if x != ' ' {
                staged.push(serde_json::json!({"status": x.to_string(), "path": path}));
            }
            if y != ' ' {
                unstaged.push(serde_json::json!({"status": y.to_string(), "path": path}));
            }
        }
    }

    let result = serde_json::json!({
        "staged": staged,
        "unstaged": unstaged,
        "untracked": untracked,
        "clean": staged.is_empty() && unstaged.is_empty() && untracked.is_empty()
    });
    Ok(mcp_text_content(&serde_json::to_string_pretty(&result)?))
}

/// Handle `scm_git_branch_list`.
pub fn handle_scm_git_branch_list(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;
    let backend = open_at(repo)?;

    let branches = backend
        .list_branches(ListBranchesOptions::all())
        .map_err(|e| anyhow::anyhow!("git branch: {e}"))?;
    json_envelope(&branches)
}

// ── Write handlers ────────────────────────────────────────────────────────────

/// Handle `scm_git_branch_create`.
pub fn handle_scm_git_branch_create(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;
    let name = req(args, "name")?;
    let force = args["force"].as_bool().unwrap_or(false);
    let backend = open_at(repo)?;

    // Resolve optional start_point to a CommitId
    let start = match args["start_point"].as_str() {
        Some(spec) => {
            let id = backend
                .rev_parse(spec)
                .map_err(|e| anyhow::anyhow!("bad start_point '{spec}': {e}"))?
                .primary()
                .clone();
            Some(id)
        }
        None => None,
    };

    let branch = backend
        .create_branch(name, start.as_ref(), force)
        .map_err(|e| anyhow::anyhow!("create branch: {e}"))?;
    json_envelope(&branch)
}

/// Handle `scm_git_branch_delete`.
pub fn handle_scm_git_branch_delete(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;
    let name = req(args, "name")?;
    let force = args["force"].as_bool().unwrap_or(false);
    let backend = open_at(repo)?;

    backend
        .delete_branch(name, force)
        .map_err(|e| anyhow::anyhow!("delete branch: {e}"))?;
    Ok(mcp_text_content(&format!("deleted branch {name}")))
}

/// Handle `scm_git_commit` — stage + commit with optional author rewriting.
///
/// Author rewriting: pass `author_email: "hartsock@users.noreply.github.com"`
/// to stamp the commit with the correct forge-specific identity.
pub fn handle_scm_git_commit(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;
    let message = req(args, "message")?;
    let backend = open_at(repo)?;

    // Resolve author: prefer explicit args, fall back to git config
    let author_name = match args["author_name"].as_str() {
        Some(n) => n.to_string(),
        None => git_config(repo, "user.name")?,
    };
    let author_email = match args["author_email"].as_str() {
        Some(e) => e.to_string(),
        None => git_config(repo, "user.email")?,
    };
    let author = Signature::new(&author_name, &author_email, Utc::now());

    // Stage paths (empty slice = all tracked changes)
    let paths: Vec<std::path::PathBuf> = args["paths"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(std::path::PathBuf::from))
                .collect()
        })
        .unwrap_or_default();

    let path_refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    backend
        .add(&path_refs)
        .map_err(|e| anyhow::anyhow!("git add: {e}"))?;

    let commit_id = backend
        .commit(message, &author)
        .map_err(|e| anyhow::anyhow!("git commit: {e}"))?;

    Ok(mcp_text_content(&format!(
        "committed {commit_id} by {author_name} <{author_email}>"
    )))
}

/// Handle `scm_git_push`.
pub fn handle_scm_git_push(args: &Value) -> anyhow::Result<Value> {
    let repo = req(args, "repo")?;
    let remote = args["remote"].as_str().unwrap_or("origin");
    let refspec = args["refspec"].as_str().unwrap_or("HEAD");
    let backend = open_at(repo)?;

    backend
        .push(remote, refspec)
        .map_err(|e| anyhow::anyhow!("git push {remote} {refspec}: {e}"))?;
    Ok(mcp_text_content(&format!("pushed {refspec} to {remote}")))
}

/// Handle `scm_git_pull` — fetch + rebase.
pub fn handle_scm_git_pull(args: &Value) -> anyhow::Result<Value> {
    use kyln_git::PullRebaseOutcome;

    let repo = req(args, "repo")?;
    let remote = args["remote"].as_str().unwrap_or("origin");
    let branch = args["branch"].as_str().unwrap_or("main");
    let backend = open_at(repo)?;

    let outcome = backend
        .pull_rebase(remote, branch)
        .map_err(|e| anyhow::anyhow!("git pull --rebase {remote}/{branch}: {e}"))?;

    let summary = match outcome {
        PullRebaseOutcome::AlreadyUpToDate => "already up to date".to_string(),
        PullRebaseOutcome::FastForward => "fast-forwarded".to_string(),
        PullRebaseOutcome::Rebased { commits_replayed } => {
            format!("rebased {commits_replayed} commit(s) on {remote}/{branch}")
        }
    };
    Ok(mcp_text_content(&summary))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn open_at(repo: &str) -> anyhow::Result<Box<dyn kyln_git::GitBackend>> {
    open_backend(Path::new(repo))
        .map_err(|e| anyhow::anyhow!("cannot open git repo at {repo}: {e}"))
}

fn req<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing required argument: {key}"))
}

fn git_config(repo: &str, key: &str) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .args(["-C", repo, "config", "--get", key])
        .output()
        .map_err(|e| anyhow::anyhow!("git config {key}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        anyhow::bail!(
            "git config {key} not set — pass author_name/author_email explicitly"
        )
    }
}

fn json_envelope<T: serde::Serialize>(value: &T) -> anyhow::Result<Value> {
    Ok(mcp_text_content(&serde_json::to_string_pretty(value)?))
}

fn mcp_text_content(text: &str) -> Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": text }]
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_count() {
        assert_eq!(tool_definitions().len(), 11);
    }

    #[test]
    fn tool_definitions_names() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        for expected in [
            "scm_git_log",
            "scm_git_blame",
            "scm_git_grep",
            "scm_git_diff",
            "scm_git_status",
            "scm_git_branch_list",
            "scm_git_branch_create",
            "scm_git_branch_delete",
            "scm_git_commit",
            "scm_git_push",
            "scm_git_pull",
        ] {
            assert!(names.contains(&expected), "missing tool: {expected}");
        }
    }

    #[test]
    fn tool_definitions_have_required_fields() {
        for def in tool_definitions() {
            let name = def["name"].as_str().unwrap();
            assert!(def["description"].as_str().is_some(), "{name}: missing description");
            assert!(def["inputSchema"]["properties"].is_object(), "{name}: missing properties");
            assert!(def["inputSchema"]["required"].as_array().is_some(), "{name}: missing required");
        }
    }

    #[test]
    fn missing_required_args_error() {
        let err = |h: fn(&Value) -> anyhow::Result<Value>| {
            h(&serde_json::json!({})).unwrap_err().to_string()
        };
        assert!(err(handle_scm_git_log).contains("repo"));
        assert!(err(handle_scm_git_blame).contains("repo"));
        assert!(err(handle_scm_git_grep).contains("repo"));
        assert!(err(handle_scm_git_diff).contains("repo"));
        assert!(err(handle_scm_git_status).contains("repo"));
        assert!(err(handle_scm_git_branch_list).contains("repo"));
        assert!(err(handle_scm_git_branch_create).contains("repo"));
        assert!(err(handle_scm_git_branch_delete).contains("repo"));
        assert!(err(handle_scm_git_commit).contains("repo"));
        assert!(err(handle_scm_git_push).contains("repo"));
        assert!(err(handle_scm_git_pull).contains("repo"));
    }

    #[test]
    fn bad_repo_path_gives_clear_error() {
        let err = handle_scm_git_log(&serde_json::json!({"repo": "/no-such-xyz"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot open git repo"), "got: {err}");
    }

    #[test]
    fn scm_git_blame_missing_file() {
        let err = handle_scm_git_blame(&serde_json::json!({"repo": "/tmp"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("file"), "got: {err}");
    }

    #[test]
    fn scm_git_grep_missing_pattern() {
        let err = handle_scm_git_grep(&serde_json::json!({"repo": "/tmp"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("pattern"), "got: {err}");
    }
}

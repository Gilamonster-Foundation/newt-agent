//! Optional embedded Git adapter contract extracted from Newt.
//!
//! This legacy schema is not a substitute for the native Git command line.
//! Harnesses opt in explicitly; the adapter receives authority from its caller.

use crate::git_caveats::GitCaveats;

/// Operations whose complete filesystem read surface is bounded at the
/// injected seam. Other ops require unrestricted filesystem reads until
/// their engine reads receive equivalent scope enforcement, even in Act.
pub const SCOPED_READ_OPS: &[&str] = &["branch-list"];

pub fn is_scoped_read_op(op: &str) -> bool {
    SCOPED_READ_OPS.contains(&op)
}

fn requires_scoped_ops(read_scope: &crate::caveats::Scope<String>) -> bool {
    !matches!(read_scope, crate::caveats::Scope::All)
}

/// Refuse legacy engine reads that cannot honor a bounded filesystem grant.
/// Git write permission and prompt disposition cannot widen this read scope.
pub fn check_git_read_scope(
    op: &str,
    read_scope: &crate::caveats::Scope<String>,
) -> Result<(), &'static str> {
    if requires_scoped_ops(read_scope) && !is_scoped_read_op(op) {
        Err("Git operation unavailable with scoped fs_read; only branch-list has bounded reads")
    } else {
        Ok(())
    }
}

pub fn definition_for_read_scope(read_scope: &crate::caveats::Scope<String>) -> serde_json::Value {
    if requires_scoped_ops(read_scope) {
        read_only_definition()
    } else {
        git_tool_definition()
    }
}

pub fn read_only_definition() -> serde_json::Value {
    let mut def = git_tool_definition();
    def["function"]["description"] = serde_json::json!(
        "List and count local and cached remote-tracking branches with op=branch-list. \
         scope=local|remote|all (default all). Remote-tracking refs are local cached data; \
         this does not fetch or count open pull requests. Repository and Git metadata \
         must be within the session's filesystem read grants. Optional cwd selects \
         a repository/worktree root relative to the session workspace for this call only. Other Git operations \
         are unavailable under this turn's authority."
    );
    def["function"]["strict"] = serde_json::json!(true);
    def["function"]["parameters"]["properties"]["op"]["enum"] = serde_json::json!(SCOPED_READ_OPS);
    def["function"]["parameters"]["properties"]["scope"]["type"] =
        serde_json::json!(["string", "null"]);
    def["function"]["parameters"]["properties"]["scope"]["enum"] =
        serde_json::json!(["local", "remote", "all", null]);
    def["function"]["parameters"]["required"] = serde_json::json!(["op", "scope", "cwd"]);
    def["function"]["parameters"]["additionalProperties"] = serde_json::json!(false);
    if let Some(properties) = def["function"]["parameters"]["properties"].as_object_mut() {
        properties.retain(|name, _| matches!(name.as_str(), "op" | "scope" | "cwd"));
    }
    def
}

/// The injected git capability. Object-safe and shareable (the loop holds
/// `&dyn GitTool`; `Send + Sync` because the borrow lives across `.await`
/// points, exactly like [`RecallSource`](super::recall::RecallSource)).
///
/// `op` is one of `status` | `log` | `diff` | `add` | `commit` | `branch`;
/// `args` is the tool-call argument object. The implementation enforces
/// both the Git operation caveats and the required session filesystem caveats
/// (fail-closed), and returns either a rendered, model-readable result
/// string (`Ok`) or an error string the tool layer surfaces verbatim (`Err`) —
/// including capability denials, so the model can see *why* a write was refused.
pub trait GitTool: Send + Sync {
    /// Number of host-confirmed commits since the session's last telemetry
    /// drain. Signing, preparation, output text, and unrelated ref moves do
    /// not increment this signal. The session must not drain it during a tool
    /// call, so callers can compare observations around that call.
    fn confirmed_commit_count(&self) -> usize {
        0
    }

    /// Host-only policy for native commit hooks. This grants no repository or
    /// process authority; the caller must bind it to an authorized invocation.
    fn native_commit_policy(&self) -> Option<std::sync::Arc<dyn crate::native_git::CommitPolicy>> {
        None
    }

    fn dispatch(
        &self,
        op: &str,
        args: &serde_json::Value,
        caveats: &GitCaveats,
        session: &crate::caveats::Caveats,
    ) -> Result<String, String>;
}

/// The advertised `git` tool definition — pushed into the tool list only when a
/// [`GitTool`] is injected with its filesystem read scope, so
/// eval / headless / non-repo sessions never see it.
pub fn git_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "git",
            "description": "Run a git operation through the embedded engine \
                            (NOT run_command; use this advertised tool — do not \
                            look for a separate git tool). Local-only: initialize \
                            a repo here with init (when the workspace is not yet a \
                            git repo), read status/log/diff, list and count branches \
                            (branch-list; local and cached remote-tracking refs), stage with add, \
                            commit, amend the last commit, create a branch \
                            (branch), switch to / create-and-switch a branch \
                            (checkout), delete a branch (branch-delete), and \
                            rebase (structured plan: reword/squash/drop). \
                            Writes (init/add/commit/amend/branch/checkout/ \
                            branch-delete/rebase) require the session to permit \
                            them; there are no network ops (pull/fetch/push are \
                            unavailable).",
            "parameters": {
                "type": "object",
                "properties": {
                    "cwd": {
                        "type": ["string", "null"],
                        "description": "Optional repository/worktree root relative to the session workspace, e.g. .worktrees/task; null or omission uses the session root. Applies only to this call. Absolute paths, parent traversal and targets outside the workspace are refused. Existing read/write authority still applies, including shared Git metadata."
                    },
                    "op": {
                        "type": "string",
                        "enum": ["init", "status", "log", "diff", "add", "commit", "amend", "branch", "branch-list", "rebase", "checkout", "branch-delete", "stash", "stash-list", "stash-pop", "stash-apply", "stash-drop"],
                        "description": "The git operation to run."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["local", "remote", "all"],
                        "description": "For op=branch-list: local branches, cached remote-tracking branches, or both (default all). Excludes symbolic remote aliases; no network access."
                    },
                    "index": {
                        "type": "integer",
                        "description": "For op=stash-pop/stash-apply/stash-drop: the stash index k (stash@{k}); default 0 (newest)."
                    },
                    "onto": {
                        "type": "string",
                        "description": "For op=rebase: the base commit/ref to replay the plan onto."
                    },
                    "plan": {
                        "type": "array",
                        "description": "For op=rebase: ordered steps, oldest first. Each: \
                                        {commit, action: pick|reword|squash|fixup|drop, message?}. \
                                        reword/squash use message; squash folds into the previous \
                                        commit keeping both messages; fixup folds discarding it; \
                                        drop removes the commit. Aborts (no change) on any conflict.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "commit": { "type": "string" },
                                "action": {
                                    "type": "string",
                                    "enum": ["pick", "reword", "squash", "fixup", "drop"]
                                },
                                "message": { "type": "string" }
                            },
                            "required": ["commit", "action"]
                        }
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "For op=add: the paths to stage. For op=log/diff: an optional pathspec filter."
                    },
                    "revision": {
                        "type": "string",
                        "description": "For op=log: a single rev to start from instead of HEAD, or an A..B range (walks from B, excluding everything reachable from A)."
                    },
                    "rev": {
                        "type": "string",
                        "description": "For op=diff: a revision to diff against the worktree, or an A..B range (equivalent to rev=A, rev2=B)."
                    },
                    "rev2": {
                        "type": "string",
                        "description": "For op=diff: paired with rev for a rev-vs-rev diff (git diff rev rev2)."
                    },
                    "stat": {
                        "type": "boolean",
                        "description": "For op=diff: include per-file insertion/deletion counts (default false)."
                    },
                    "message": {
                        "type": "string",
                        "description": "For op=commit: the commit message. For \
                                        op=amend: the reworded message (omit to \
                                        keep the existing one)."
                    },
                    "name": {
                        "type": "string",
                        "description": "The branch name — for op=branch (create), \
                                        op=checkout (switch to / create), and \
                                        op=branch-delete."
                    },
                    "create": {
                        "type": "boolean",
                        "description": "For op=checkout: create the branch at HEAD \
                                        when it does not exist (default true, i.e. \
                                        `checkout -b`). Set false to switch to an \
                                        existing branch only."
                    },
                    "spec": {
                        "type": "string",
                        "enum": ["worktree", "staged"],
                        "description": "For op=diff: worktree (unstaged, default) or staged."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "For op=log: max commits (default 20)."
                    }
                },
                "required": ["op"]
            }
        }
    })
}

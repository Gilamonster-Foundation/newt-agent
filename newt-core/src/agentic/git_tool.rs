//! The `git` tool seam (PR4, issue #461).
//!
//! `newt-git` (the embedded `GitEngine` over `grit-lib`) depends on `newt-core`
//! for [`GitCaveats`](crate::git_caveats::GitCaveats), so `newt-core` can NOT
//! depend on `newt-git` (circular). The agent loop reaches the engine through
//! the same **trait-injection** seam already used for
//! [`NoteSink`](super::note_sink::NoteSink) /
//! [`RecallSource`](super::recall::RecallSource) /
//! [`MemorySource`](super::memory_fetch::MemorySource): newt-core defines this
//! trait, `newt-git` implements it (`LocalGitTool`), and the binary
//! (`newt-tui`/`newt-cli`) injects the impl into [`ChatCtx`](super::ChatCtx)
//! per turn. `execute_tool` dispatches the `git` tool through it; the tool is
//! advertised only when an impl is injected (presence gate).

use crate::git_caveats::GitCaveats;

/// The injected git capability. Object-safe and shareable (the loop holds
/// `&dyn GitTool`; `Send + Sync` because the borrow lives across `.await`
/// points, exactly like [`RecallSource`](super::recall::RecallSource)).
///
/// `op` is one of `status` | `log` | `diff` | `add` | `commit` | `branch`;
/// `args` is the tool-call argument object. The implementation enforces
/// `caveats` (fail-closed) and returns either a rendered, model-readable result
/// string (`Ok`) or an error string the tool layer surfaces verbatim (`Err`) —
/// including capability denials, so the model can see *why* a write was refused.
pub trait GitTool: Send + Sync {
    fn dispatch(
        &self,
        op: &str,
        args: &serde_json::Value,
        caveats: &GitCaveats,
    ) -> Result<String, String>;
}

/// The advertised `git` tool definition — pushed into the tool list only when a
/// [`GitTool`] is injected (`merged_tool_definitions(.., with_git = true)`), so
/// eval / headless / non-repo sessions never see it.
pub fn git_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "git",
            "description": "Run a git operation through the embedded engine \
                            (NOT run_command). Local-only: read status/log/diff, \
                            stage with add, commit, amend the last commit, and \
                            create branches. Writes (add/commit/amend/branch) \
                            require the session to permit them; network ops are \
                            unavailable.",
            "parameters": {
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["status", "log", "diff", "add", "commit", "amend", "branch"],
                        "description": "The git operation to run."
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "For op=add: the paths to stage."
                    },
                    "message": {
                        "type": "string",
                        "description": "For op=commit: the commit message. For \
                                        op=amend: the reworded message (omit to \
                                        keep the existing one)."
                    },
                    "name": {
                        "type": "string",
                        "description": "For op=branch: the new branch name."
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

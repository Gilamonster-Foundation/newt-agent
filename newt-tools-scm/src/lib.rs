//! Newt-Agent SCM tool surface — git log, blame, grep via kyln backends.
//!
//! Each tool module is gated behind a Cargo feature so the MCP server
//! can enable only what's installed on the host.
//!
//! # Feature flags
//!
//! | Feature      | Dep added    | Tools exposed                              |
//! |--------------|--------------|---------------------------------------------|
//! | `tools-git`  | `kyln-git`   | `scm_git_log`, `scm_git_blame`, `scm_git_grep` |

#[cfg(feature = "tools-git")]
pub mod git;

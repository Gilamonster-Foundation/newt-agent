//! Fixed-form failure labels and harness-authored recovery guidance. No child
//! diagnostic bytes are retained here; branch names come from the checked plan.
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureCategory {
    Network,
    Auth,
    RefusedByHarness,
    GitError,
    NonFastForward {
        branch: String,
        remote: String,
    },
    Rejected,
    /// A successful `gh pr create` did not return exactly the expected PR URL.
    Parse,
}

impl fmt::Display for FailureCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Network => "network",
            Self::Auth => "auth",
            Self::RefusedByHarness => "refused_by_harness",
            Self::GitError => "git_error",
            Self::NonFastForward { .. } => "non_fast_forward",
            Self::Rejected => "rejected",
            Self::Parse => "parse",
        })
    }
}

impl FailureCategory {
    pub(super) fn write_guidance(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFastForward { branch, remote } => {
                let next = crate::mcp::shell_quote_arg(&format!("{branch}-2"));
                let tracking = crate::mcp::shell_quote_arg(&format!("{remote}/{branch}"));
                let remote = crate::mcp::shell_quote_arg(remote);
                write!(f, ": remote branch '{}' already exists with history not contained in the local commit; force-push is not allowed. Push to a NEW branch name: run `git checkout -b {next}` as a standalone command, then `git push {remote}` (choose another unused name if needed). If you meant to continue the existing branch, fetch {remote} and rebase onto {tracking}, resolving any conflicts, then retry. The governed push destination must match the checked-out branch.", branch.escape_default())
            }
            Self::Auth => f.write_str(": authentication or repository access failed; check credentials and permission for the approved remote, then retry."),
            Self::Network => f.write_str(": the approved remote could not be reached; check DNS, connectivity, and proxy/TLS settings, then retry."),
            Self::Rejected => f.write_str(": Git or the remote rejected the ref update; check branch protection and server-side hook requirements. Force-push is not allowed."),
            Self::GitError => f.write_str(": Git failed for an unclassified reason; this does not establish an authentication or network failure. Ask the operator to inspect Git diagnostics for the approved remote."),
            Self::RefusedByHarness | Self::Parse => Ok(()),
        }
    }
}

//! Read captured diagnostics only to choose a fixed category. Never return raw
//! child output: credentials and remote-controlled prose must remain private.
use crate::git_staging::FailureCategory;

pub(super) fn classify_push(
    stdout: &[u8],
    stderr: &[u8],
    refspec: &str,
    branch: &str,
    remote: &str,
) -> FailureCategory {
    // --porcelain: <flag> TAB <from>:<to> TAB <summary>. Require the exact
    // approved refspec; prose from helpers or a different ref is not evidence.
    for line in String::from_utf8_lossy(stdout).lines() {
        let mut fields = line.splitn(3, '\t');
        if fields.next() != Some("!") || fields.next() != Some(refspec) {
            continue;
        }
        return match fields.next() {
            Some("[rejected] (non-fast-forward)" | "[rejected] (fetch first)") => {
                FailureCategory::NonFastForward {
                    branch: branch.to_owned(),
                    remote: remote.to_owned(),
                }
            }
            _ => FailureCategory::Rejected,
        };
    }
    classify_failure(stderr)
}

pub(super) fn classify_failure(stderr: &[u8]) -> FailureCategory {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if [
        "could not resolve host",
        "could not resolve hostname",
        "could not resolve proxy",
        "connection refused",
        "connection timed out",
        "network is unreachable",
        "connection reset",
        "failed to connect",
        "ssl certificate problem",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
    {
        FailureCategory::Network
    } else if [
        "authentication failed",
        "permission denied",
        "could not read username",
        "host key verification failed",
        "requested url returned error: 403",
        "requested url returned error: 401",
        "http 401",
        "http 403",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
    {
        FailureCategory::Auth
    } else {
        FailureCategory::GitError
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_staging::Outcome;

    /// H3: expose a useful category without disclosing raw helper/remote bytes.
    #[test]
    fn failures_are_distinct_and_never_echo_child_diagnostics() {
        for (diagnostic, category) in [
            ("Could not resolve hostname", FailureCategory::Network),
            ("Failed to connect to host", FailureCategory::Network),
            ("Connection reset by peer", FailureCategory::Network),
            ("Authentication failed", FailureCategory::Auth),
            ("Permission denied (publickey)", FailureCategory::Auth),
            ("could not read Username", FailureCategory::Auth),
            (
                "The requested URL returned error: 403",
                FailureCategory::Auth,
            ),
            (
                "unrecognized failure for repo401",
                FailureCategory::GitError,
            ),
        ] {
            let stderr = format!("{diagnostic}: PRIVATE-CREDENTIAL-CANARY");
            assert_eq!(classify_failure(stderr.as_bytes()), category);
            let text = Outcome::Failed { category }.to_string();
            assert!(text.len() > 40, "{text}");
            assert!(!text.contains("PRIVATE-CREDENTIAL-CANARY"));
        }
    }

    /// Only the approved ref's machine-readable rejection can label a collision.
    #[test]
    fn porcelain_rejections_are_bound_to_the_approved_ref() {
        for reason in ["non-fast-forward", "fetch first"] {
            let stdout = format!("!\tabc:refs/heads/task\t[rejected] ({reason})\n");
            assert_eq!(
                classify_push(
                    stdout.as_bytes(),
                    b"PRIVATE",
                    "abc:refs/heads/task",
                    "task",
                    "origin"
                ),
                FailureCategory::NonFastForward {
                    branch: "task".into(),
                    remote: "origin".into()
                }
            );
        }
        assert_eq!(
            classify_push(
                b"!\tabc:refs/heads/task\t[remote rejected] (PRIVATE)\n",
                b"",
                "abc:refs/heads/task",
                "task",
                "origin"
            ),
            FailureCategory::Rejected
        );
        assert_eq!(
            classify_push(
                b"!\tabc:refs/heads/other\t[rejected] (fetch first)\n",
                b"remote: non-fast-forward PRIVATE",
                "abc:refs/heads/task",
                "task",
                "origin"
            ),
            FailureCategory::GitError
        );
        let text = Outcome::Failed {
            category: FailureCategory::NonFastForward {
                branch: "task;echo${IFS}oops".into(),
                remote: "upstream".into(),
            },
        }
        .to_string();
        assert!(text.contains("git push upstream"), "{text}");
        assert!(
            text.contains("git checkout -b 'task;echo${IFS}oops-2'"),
            "{text}"
        );
    }
}

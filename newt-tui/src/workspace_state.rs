const WORKSPACE_STATE_DIRTY_FILE_LIMIT: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceStateSnapshot {
    pub(crate) timestamp: String,
    pub(crate) branch: Option<String>,
    pub(crate) dirty_files: Vec<String>,
    pub(crate) git_status_available: bool,
}

pub(crate) fn workspace_state_block(workspace: &str) -> String {
    format_workspace_state_block(&collect_workspace_state(workspace))
}

fn collect_workspace_state(workspace: &str) -> WorkspaceStateSnapshot {
    let timestamp = chrono::Local::now().to_rfc3339();
    let branch = git_stdout(workspace, &["branch", "--show-current"])
        .filter(|b| !b.trim().is_empty())
        .or_else(|| {
            git_stdout(workspace, &["rev-parse", "--short", "HEAD"])
                .filter(|h| !h.trim().is_empty())
                .map(|h| format!("detached HEAD ({h})"))
        });
    let status = git_stdout(workspace, &["status", "--porcelain=v1"]);
    let dirty_files = status
        .as_deref()
        .map(parse_git_porcelain_dirty_files)
        .unwrap_or_default();
    WorkspaceStateSnapshot {
        timestamp,
        branch,
        dirty_files,
        git_status_available: status.is_some(),
    }
}

fn git_stdout(workspace: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn parse_git_porcelain_dirty_files(status: &str) -> Vec<String> {
    let mut files = Vec::new();
    for raw in status.lines() {
        if raw.starts_with("##") || raw.len() < 4 {
            continue;
        }
        let path = raw.get(3..).unwrap_or_default().trim();
        let path = path
            .rsplit_once(" -> ")
            .map(|(_, new_path)| new_path)
            .unwrap_or(path);
        if !path.is_empty() && !files.iter().any(|seen| seen == path) {
            files.push(path.to_string());
        }
    }
    files
}

pub(crate) fn format_workspace_state_block(state: &WorkspaceStateSnapshot) -> String {
    let mut lines = vec![
        "<workspace_state>".to_string(),
        format!("timestamp: {}", state.timestamp),
    ];
    if let Some(branch) = &state.branch {
        lines.push(format!("branch: {branch}"));
    } else if state.git_status_available {
        lines.push("branch: detached or unknown".to_string());
    } else {
        lines.push("git: unavailable (not a git worktree or git command failed)".to_string());
    }

    if state.git_status_available {
        if state.dirty_files.is_empty() {
            lines.push("dirty files: none".to_string());
            lines.push("local changes: clean".to_string());
        } else {
            lines.push(format!("dirty files ({}):", state.dirty_files.len()));
            for path in state
                .dirty_files
                .iter()
                .take(WORKSPACE_STATE_DIRTY_FILE_LIMIT)
            {
                lines.push(format!("- {path}"));
            }
            let overflow = state
                .dirty_files
                .len()
                .saturating_sub(WORKSPACE_STATE_DIRTY_FILE_LIMIT);
            if overflow > 0 {
                lines.push(format!("- ... {overflow} more"));
            }
            lines.push(
                "unlanded local changes exist; do not treat them as upstream-complete work"
                    .to_string(),
            );
            lines.push(
                "next completion step: verify, commit, push/open PR, or state blocker".to_string(),
            );
        }
    } else {
        lines.push("dirty files: unknown".to_string());
    }

    lines.push("</workspace_state>".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_file_overflow_is_reported_without_listing_every_file() {
        let dirty_files = (1..=14).map(|n| format!("file-{n}.rs")).collect::<Vec<_>>();
        let block = format_workspace_state_block(&WorkspaceStateSnapshot {
            timestamp: "2026-07-16T00:00:00Z".into(),
            branch: Some("main".into()),
            dirty_files,
            git_status_available: true,
        });

        assert!(block.contains("dirty files (14):"), "{block}");
        assert!(block.contains("- file-12.rs"), "{block}");
        assert!(block.contains("- ... 2 more"), "{block}");
        assert!(!block.contains("file-13.rs"), "{block}");
    }
}

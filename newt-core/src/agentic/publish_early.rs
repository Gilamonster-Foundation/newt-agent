//! #2831: advisory publication steering, never authority or completion evidence.
use crate::ExecOutcome;
use content_addressable::{ContentId, RawContentId};
use std::collections::BTreeMap;

pub const GUIDANCE: &str = "For a refactor that asks for a worktree and PR, plan one cohesive extraction per publish cycle: extract and wire it in, cargo check, commit, push, open the PR. Make further extractions follow-up commits to the same PR.";
const HINT: &str = "Publish early: a task-worktree commit matches the staged content of a passing cargo check; no governed PR creation has been observed for this branch. Push and open the PR before the next extraction (update the same PR if one already exists).";
#[derive(Debug, Default)]
pub(crate) struct State {
    branches: BTreeMap<String, Branch>,
    // One current content witness, never a branch-indexed cache of old passes.
    checked: Option<Checked>,
}
#[derive(Debug)]
struct Checked {
    tree: ContentId,
    reflog: RawContentId,
    log_len: usize,
}
#[derive(Debug, Default)]
struct Branch {
    pr: bool,
    shown: bool,
    requested: bool,
}
impl State {
    fn observe(&mut self, branch: &str, pr: bool, checked_commit: bool) -> Option<&'static str> {
        let state = self.branches.entry(branch.to_owned()).or_default();
        state.pr |= pr;
        if !state.shown && !state.pr && checked_commit {
            state.shown = true;
            Some(HINT)
        } else {
            None
        }
    }
}
pub(crate) fn bounded_move_hint(lines: usize) -> &'static str {
    // #2831 lab evidence: completed extractions were 171–1168 lines; the
    // failed one-shot copy was 6096 lines and was never wired into its parent.
    // Leave headroom above the successful range without accepting that shape silently.
    if lines > 1500 {
        "\nRefactor hint: this operation exceeds 1500 lines; prefer to split it into smaller cohesive extractions, wire each into its parent, check, then publish. This is advice, not a refusal."
    } else {
        ""
    }
}

fn refactor_publication(objective: &str) -> bool {
    let text = objective.to_ascii_lowercase();
    text.contains("refactor")
        && text.contains("worktree")
        && (text.contains("pull request")
            || text
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word == "pr"))
}

/// The existing dispatch result funnel supplies typed outcomes, never result prose.
#[allow(clippy::too_many_arguments)]
pub(crate) fn after_tool(
    session: &crate::worktree_adoption::WorktreeSession,
    objective: &str,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    execution: Option<ExecOutcome>,
    publication: Option<&crate::git_staging::Outcome>,
    read: &crate::Scope<String>,
    command_directory: Option<&std::path::Path>,
) -> Option<&'static str> {
    use std::path::Path;
    if !matches!(
        name,
        "run_command" | "build_exec" | "write_file" | "edit_file" | "delete_file"
    ) {
        return None;
    }
    let root = session.task_root(Path::new(workspace))?;
    let admin = crate::workspace_key::discover_git_dir(&root)?;
    let head = read_fact(&admin.join("HEAD"), read)?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    crate::git_staging::validate_branch_name(branch).ok()?;
    let command = if name == "build_exec" {
        args["argv"]
            .as_array()?
            .iter()
            .map(|v| v.as_str())
            .collect::<Option<Vec<_>>>()?
            .join(" ")
    } else {
        args["command"].as_str().unwrap_or("").to_owned()
    };
    let check = if matches!(name, "run_command" | "build_exec")
        && super::self_verify::cargo_check_directory(&command, args, workspace)
            .and_then(|p| p.canonicalize().ok())
            .is_some_and(|p| p.starts_with(&root))
    {
        Some(execution.unwrap_or(ExecOutcome::Denied))
    } else {
        None
    };
    // Governed creation constrains --head to the checked-out branch. Only
    // attribute that ledger event when dispatch ran in this task worktree.
    let pr = matches!(publication, Some(crate::git_staging::Outcome::PrCreated { url })
        if crate::git_staging::validate_pr_url(url).is_some())
        && command_directory
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| path.starts_with(&root));
    let mut state = session.publish_early.lock().expect("publication hint lock");
    let observed = state.branches.entry(branch.to_owned()).or_default();
    observed.requested |= refactor_publication(objective);
    observed.pr |= pr;
    if observed.pr || observed.shown {
        state.checked = None;
        return None;
    }
    let requested = observed.requested;
    // Unknown/failed execution cannot retain a pass. For an unsupported Cargo
    // spelling, even exit 0 may mask a failed check (e.g. `cargo check || true`).
    // This deliberately over-invalidates mentions, without interpreting effects.
    if execution != Some(ExecOutcome::Passed)
        || matches!(name, "write_file" | "edit_file" | "delete_file")
        || (command.contains("cargo") && check != Some(ExecOutcome::Passed))
    {
        state.checked = None;
        return None;
    }
    if check.is_none() && state.checked.is_none() {
        return None;
    }
    let Some(log) = read_fact(&admin.join("logs/HEAD"), read) else {
        state.checked = None;
        return None;
    };
    let Some(tree) = checked_tree::index_tree(&root, read) else {
        state.checked = None;
        return None;
    };
    if check == Some(ExecOutcome::Passed) {
        state.checked = Some(Checked {
            tree,
            reflog: RawContentId::from_content(log.as_bytes()),
            log_len: log.len(),
        });
        return None;
    }
    let checked = state.checked.as_ref()?;
    let prefix_matches = log
        .as_bytes()
        .get(..checked.log_len)
        .is_some_and(|prefix| RawContentId::from_content(prefix) == checked.reflog);
    let added = log.get(checked.log_len..).unwrap_or_default();
    // Inspect actual reflog transitions, including away-and-back in ONE shell
    // command. Only appended commit records preserve this content witness.
    if tree != checked.tree
        || !prefix_matches
        || added.is_empty()
        || !added.lines().all(commit_in_reflog)
    {
        state.checked = None;
        return None;
    }
    // Without an attributable check or a newly observed commit, an intervening
    // command has unknown check semantics; never infer non-mutation from shell.
    let commit = requested && checked_tree::index_is_head(&root, read);
    state.observe(branch, pr, commit)
}

#[path = "publish_early_tree.rs"]
mod checked_tree;

fn read_fact(path: &std::path::Path, read: &crate::Scope<String>) -> Option<String> {
    use std::io::Read;
    if !crate::permits_path(read, path.to_str()?) {
        return None;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let file =
        crate::fs_cap::WorkspaceDir::open_granted_file(path, std::path::Path::new("."), true)
            .ok()?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let file = {
        if !std::fs::symlink_metadata(path).ok()?.is_file() {
            return None;
        }
        std::fs::File::open(path).ok()?
    };
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return None;
    }
    let mut text = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut text).ok()?;
    (text.len() <= 1024 * 1024).then_some(text)
}

fn commit_in_reflog(log: &str) -> bool {
    let Some((fields, message)) = log.lines().last().and_then(|line| line.split_once('\t')) else {
        return false;
    };
    let Some(oid) = fields.split_whitespace().nth(1) else {
        return false;
    };
    crate::git_staging::is_hex_oid(oid)
        && oid.bytes().any(|c| c != b'0')
        && ["commit: ", "commit (initial): ", "commit (amend): "]
            .iter()
            .any(|prefix| message.starts_with(prefix))
}

#[cfg(test)]
#[path = "publish_early_tests.rs"]
mod tests;

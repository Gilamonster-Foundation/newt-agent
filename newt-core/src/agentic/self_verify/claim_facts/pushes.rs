//! Push claims use observed publication facts, never a remote probe.
use super::*;
use crate::agentic::claim_check;

/// Ephemeral locators from the same observation, not persisted identities.
#[derive(Debug, Clone)]
pub(in crate::agentic::self_verify) struct PushObservation {
    remote: Option<String>,
    branch: Option<String>,
    directory: Option<PathBuf>,
}

impl VerificationLedger {
    pub(crate) fn record_push_outcome(&mut self, outcome: &crate::git_staging::Outcome) {
        if let crate::git_staging::Outcome::Pushed {
            owner,
            name,
            branch,
            ..
        } = outcome
        {
            self.pushed_branches.push(PushObservation {
                remote: Some(format!("{owner}/{name}")),
                branch: Some(branch.clone()),
                directory: None,
            });
        }
    }

    pub(in crate::agentic::self_verify) fn observe_plain_push(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        execution: Option<ExecOutcome>,
        bypass: bool,
        workspace: &str,
    ) {
        if bypass
            && execution == Some(ExecOutcome::Passed)
            && super::super::super::dispatched_tool_name(name) == Some("run_command")
        {
            if let Some(branch) =
                plain_push_branch(args["command"].as_str().unwrap_or(""), args, workspace)
            {
                self.pushed_branches.push(branch);
            }
        }
    }

    pub(crate) fn annotate_push_claim(&self, mut text: String, workspace: &str) -> String {
        const MARKER: &str = "⚠ claim check (#2769): ";
        if text.contains(MARKER) {
            text = text
                .lines()
                .filter(|line| !line.starts_with(MARKER))
                .collect::<Vec<_>>()
                .join("\n")
                .trim_end()
                .to_owned();
        }
        let mut missing = Vec::new();
        let mut fenced = false;
        for line in claim_check::asserted_claim_lines(&text) {
            if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
                fenced = !fenced;
                continue;
            }
            if fenced {
                continue;
            }
            let mut clause = Vec::new();
            for token in line.split_whitespace() {
                clause.push(token);
                if claim_check::ends_a_clause(token) {
                    missing.extend(push_claims(&clause));
                    clause.clear();
                }
            }
            missing.extend(push_claims(&clause));
        }
        missing.retain(|branch| {
            !self.pushed_branches.iter().any(|observed| match branch {
                None => true,
                Some(claim) => {
                    // A slash-bearing claim is destination-qualified unless
                    // explicitly written as refs/heads/<branch>.
                    let local = claim.strip_prefix("refs/heads/");
                    if local.is_some() || !claim.contains('/') {
                        return observed.branch.as_deref() == Some(local.unwrap_or(claim));
                    }
                    observed
                        .remote
                        .as_ref()
                        .zip(observed.branch.as_ref())
                        .is_some_and(|(remote, branch)| {
                            claim == &format!("{remote}/{branch}")
                                && observed.directory.as_ref().is_none_or(|directory| {
                                    directory
                                        == &crate::agentic::lexical_normalize(Path::new(workspace))
                                })
                        })
                }
            })
        });
        missing.sort();
        missing.dedup();
        for branch in missing.into_iter().take(8) {
            let branch = branch
                .map(|b| format!("`{b}`"))
                .unwrap_or_else(|| "the claimed branch".into());
            text.push_str(&format!("\n\n{MARKER}no push of {branch} was observed this turn — verify before trusting the summary."));
        }
        text
    }
}

fn push_claims(tokens: &[&str]) -> Vec<Option<String>> {
    let words: Vec<_> = tokens
        .iter()
        .map(|w| w.trim_matches(|c: char| "`*\"'[](),:;.!?".contains(c)))
        .collect();
    let lower: Vec<_> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let pushed = lower.iter().position(|w| w == "pushed");
    let live = lower
        .windows(4)
        .any(|w| w == ["is", "live", "on", "origin"])
        || (lower.iter().any(|w| w.starts_with("origin/"))
            && lower.windows(2).any(|w| w == ["is", "live"]));
    let clause = tokens.join(" ");
    if !claim_check::claims_completed_action(&clause, "pushed")
        && !(live && claim_check::claims_completed_action(&clause, "live"))
    {
        return Vec::new();
    }
    let branch = |word: &str| -> Option<String> {
        if matches!(
            word.to_ascii_lowercase().as_str(),
            "the"
                | "a"
                | "branch"
                | "is"
                | "was"
                | "has"
                | "been"
                | "pushed"
                | "and"
                | "to"
                | "on"
                | "origin"
                | "successfully"
                | "already"
                | "now"
                | "ready"
        ) {
            return None;
        }

        crate::git_staging::validate_branch_name(word).ok()?;
        Some(word.into())
    };
    let mut branches: Vec<_> = words
        .iter()
        .filter(|w| w.contains('/'))
        .filter_map(|w| branch(w))
        .map(Some)
        .collect();
    if branches.is_empty() {
        let named = lower
            .iter()
            .position(|w| w == "branch")
            .and_then(|i| words.get(i + 1))
            .and_then(|w| branch(w))
            .or_else(|| {
                pushed
                    .and_then(|i| words.get(i + 1))
                    .and_then(|w| branch(w))
            })
            .or_else(|| {
                if live {
                    lower
                        .iter()
                        .position(|w| w == "is")
                        .and_then(|i| i.checked_sub(1))
                        .and_then(|i| branch(words[i]))
                } else {
                    None
                }
            });
        let named = if live && lower.windows(2).any(|w| w == ["on", "origin"]) {
            named.map(|branch| format!("origin/{branch}"))
        } else {
            named
        };
        branches.push(named);
    }
    branches
}

/// Only a literal push, optionally after leading cd &&, can borrow the shell's exit status. Unknown
/// default refspecs prove a push happened, but cannot certify a named branch.
fn plain_push_branch(
    command: &str,
    args: &serde_json::Value,
    workspace: &str,
) -> Option<PushObservation> {
    let segments = split_command(command);
    let ((command, separator), leading) = segments.split_last()?;
    if !separator.is_empty()
        || leading.iter().any(|(prefix, separator)| {
            *separator != "&&"
                || prefix.split_whitespace().next() != Some("cd")
                || prefix.contains(['$', '`', ';', '|', '>', '<', '\n'])
        })
    {
        return None;
    }
    let mut directory = Path::new(workspace).join(args["cwd"].as_str().unwrap_or("."));
    for (prefix, _) in leading {
        let path = prefix.trim().strip_prefix("cd")?.trim();
        let path = path.strip_prefix("/d ").unwrap_or(path).trim();
        if path.is_empty() || path.starts_with('-') {
            return None;
        }
        directory = directory.join(path.trim_matches(['\"', '\'']));
    }
    let mut observation = PushObservation {
        remote: None,
        branch: None,
        directory: Some(crate::agentic::lexical_normalize(&directory)),
    };
    if command.contains([';', '|', '&', '$', '`', '\n', '>', '<', '\'', '"']) {
        return None;
    }
    let mut words = command.split_whitespace();
    let program = words.next()?.rsplit(['/', '\\']).next()?;
    if !program.eq_ignore_ascii_case("git") && !program.eq_ignore_ascii_case("git.exe") {
        return None;
    }
    if words.next()? != "push" {
        return None;
    }
    let mut operands = Vec::new();
    for word in words {
        if matches!(
            word,
            "-u" | "--set-upstream" | "--porcelain" | "--verbose" | "-v" | "--quiet" | "-q"
        ) {
            continue;
        }
        if word.starts_with('-') {
            return None;
        }
        operands.push(word);
    }
    match operands.as_slice() {
        [] => Some(observation),
        [remote] => {
            observation.remote = Some((*remote).into());
            Some(observation)
        }
        [remote, spec] => {
            observation.remote = Some((*remote).into());
            let branch = if let Some((source, dest)) = spec.split_once(':') {
                if source.is_empty() || dest.is_empty() {
                    return None;
                }
                dest.strip_prefix("refs/heads/").unwrap_or(dest)
            } else {
                spec.strip_prefix("refs/heads/").unwrap_or(spec)
            };
            if branch == "HEAD" {
                return Some(observation);
            }
            if branch.starts_with("refs/") || branch.contains(['*', '?', '[', ']', ':', '+']) {
                return None;
            }
            crate::git_staging::validate_branch_name(branch).ok()?;
            observation.branch = Some(branch.into());
            Some(observation)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;

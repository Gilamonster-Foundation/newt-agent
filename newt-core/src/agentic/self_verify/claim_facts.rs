//! #2718: final-answer facts from the existing verification ledger. Directory
//! paths are locators only; freshness uses conclude's content-addressed chain.
use super::*;
use std::path::{Path, PathBuf};

pub(super) fn observed_command(name: &str, args: &serde_json::Value) -> String {
    if name == "build_exec" {
        if let Some(argv) = args["argv"].as_array() {
            return argv
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" ");
        }
    }
    if super::super::dispatched_tool_name(name) == Some("run_command") {
        if let Some(command) = args["command"].as_str() {
            return command.to_string();
        }
    }
    format!("{name} {args}")
}

fn literal_path(word: &str) -> bool {
    !word.is_empty() && !word.contains(['\'', '"', '$', '`', '~', '\\', '*', '?', '[', ']'])
}

/// Deliberately refuse ambiguous shell context rather than assigning a pass to
/// the launch directory. Reuse the gate's command segment splitter.
pub(super) fn command_directory(
    command: &str,
    args: &serde_json::Value,
    workspace: &str,
) -> Option<PathBuf> {
    let cwd = args["cwd"].as_str().unwrap_or(".");
    // The structured cwd field is already a literal path (including Windows
    // separators); only shell cd operands need conservative token handling.
    let mut dir = Path::new(workspace).join(cwd);
    for (segment, separator) in split_command(command) {
        let words: Vec<_> = segment.split_whitespace().collect();
        if words.first() != Some(&"cd") {
            break;
        }
        if words.len() != 2 || separator != "&&" || !literal_path(words[1]) {
            return None;
        }
        dir = dir.join(words[1]);
    }
    // Explicit manifests select a different package tree; do not mislabel it
    // as the cwd. Cargo -C and shell substitutions likewise need richer evidence.
    if command.contains("--manifest-path")
        || command.contains(" -C ")
        || command.contains(['$', '`'])
    {
        return None;
    }
    Some(super::super::lexical_normalize(&dir))
}

fn cargo_claim(text: &str) -> bool {
    text.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        let words: Vec<_> = lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect();
        !line.trim_start().starts_with('>')
            && !lower.contains("☐")
            && !lower.contains("keep cargo check")
            && lower.contains("cargo check")
            && words.iter().any(|w| {
                matches!(
                    *w,
                    "green" | "passes" | "passed" | "passing" | "successful" | "success"
                )
            })
            && !words.iter().any(|w| {
                matches!(
                    *w,
                    "not" | "never" | "will" | "should" | "must" | "pending" | "if" | "until"
                )
            })
    })
}

impl VerificationLedger {
    pub(crate) async fn observe_routed(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        ok: bool,
        execution: Option<ExecOutcome>,
        workspace: &str,
        routed: Option<&(&'static str, serde_json::Value)>,
    ) {
        let (name, args) = routed.map(|(n, a)| (*n, a)).unwrap_or((name, args));
        self.observe(name, args, ok, execution, workspace).await;
    }

    pub(super) fn observe_directory(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        ok: bool,
        workspace: &str,
    ) {
        if !ok {
            return;
        }
        let command = observed_command(name, args);
        let directory = if let Some(path) = args["path"].as_str() {
            Path::new(workspace)
                .join(path)
                .parent()
                .map(Path::to_path_buf)
        } else if name == "build_exec"
            || super::super::dispatched_tool_name(name) == Some("run_command")
        {
            command_directory(&command, args, workspace)
        } else {
            None
        };
        if let Some(dir) = directory {
            let dir = super::super::lexical_normalize(&dir);
            if self.claim_directories.len() < 40 && !self.claim_directories.contains(&dir) {
                self.claim_directories.push(dir);
            }
        }
    }

    pub(crate) fn claim_directories(&self) -> &[PathBuf] {
        &self.claim_directories
    }

    /// Render observed facts with their exact directory and command scope,
    /// never an unqualified endorsement of the model's tree/package claim.
    pub(crate) fn annotate_cargo_claim(&self, mut text: String) -> String {
        if !cargo_claim(&text) {
            return text;
        }
        let check = VerifyCheck::new("cargo check", &["cargo check"]).except(CARGO_NON_RUNS);
        let mut directories = Vec::new();
        for entry in &self.entries {
            if let Observed::Exec {
                command, directory, ..
            } = entry
            {
                if check.invocation(command).is_some() && !directories.contains(directory) {
                    directories.push(directory.clone());
                }
            }
        }
        let mut facts = Vec::new();
        for directory in directories.iter().rev().take(8) {
            let mut ledger = self.clone();
            // Other directories cannot pay for this one's check. Conservatively
            // retain their potential mutations so unrelated runs cannot refresh it.
            for entry in &mut ledger.entries {
                if matches!(entry, Observed::Exec { directory: dir, .. } if dir != directory) {
                    *entry = Observed::Write;
                }
            }
            let (_, report) = conclude(&Conclusion {
                checks: std::slice::from_ref(&check),
                requested: &[],
                ledger: &ledger,
                tree_now: None,
                repairs_used: 0,
                rounds_left: false,
            });
            let status = report.checks[0].status;
            let status = if directory.is_none() && status == CheckStatus::Passed {
                CheckStatus::Unverified
            } else {
                status
            };
            // A different/narrower successful invocation cannot discharge an
            // earlier failure. Name that failed invocation, not the later pass.
            let failed = matches!(status, CheckStatus::Failed | CheckStatus::TimedOut);
            let command = ledger
                .entries
                .iter()
                .rev()
                .find_map(|entry| match entry {
                    Observed::Exec {
                        command, outcome, ..
                    } if check
                        .invocation(command)
                        .is_some_and(|i| !failed || i.evidence)
                        && (!failed
                            || matches!(outcome, ExecOutcome::Failed | ExecOutcome::TimedOut)) =>
                    {
                        Some(command.as_str())
                    }
                    _ => None,
                })
                .unwrap_or("cargo check");
            let dir = directory
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "unknown directory".into());
            facts.push(format!(
                "{status:?} in `{dir}` (observed invocation: `{command}`)"
            ));
        }
        if facts.is_empty() {
            facts.push("no cargo check execution was observed this turn".into());
        }
        text.push_str(&format!("\n\n⚠ claim check (#2718): {}. These are observed check facts, not verification of another tree or a broader check scope.", facts.join("; ")));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecOutcome::{Failed, Passed};

    fn in_dir(status: &str, directory: &str) -> String {
        format!(
            "{status} in `{}`",
            crate::agentic::lexical_normalize(Path::new(directory)).display()
        )
    }

    async fn run(ledger: &mut VerificationLedger, command: &str, outcome: ExecOutcome, cwd: &str) {
        ledger
            .observe(
                "run_command",
                &serde_json::json!({"command": command, "cwd": cwd}),
                outcome == Passed,
                Some(outcome),
                "/launch",
            )
            .await;
    }

    /// #2718: an off-by-default verify gate must not disable final claim facts.
    #[tokio::test]
    async fn cargo_failure_is_reported_without_optional_gate() {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        run(&mut ledger, "cargo check -p core", Failed, "/worktree").await;
        let text = ledger.annotate_cargo_claim("Cargo check green".into());
        assert!(text.contains(&in_dir("Failed", "/worktree")), "{text}");
        assert!(!ledger.result_aware());
    }

    /// #2718: a successful unrelated command or check in another tree cannot
    /// erase a failed check; the report names both directories explicitly.
    #[tokio::test]
    async fn cargo_facts_keep_tree_and_failure_scope() {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        run(&mut ledger, "cargo check -p core", Failed, "/worktree").await;
        run(&mut ledger, "cargo check -p other", Passed, "/worktree").await;
        run(&mut ledger, "cargo check", Passed, "/launch").await;
        run(&mut ledger, "git status", Passed, "/launch").await;
        let text = ledger.annotate_cargo_claim("cargo check passes".into());
        assert!(text.contains(&in_dir("Failed", "/worktree")), "{text}");
        assert!(text.contains(&in_dir("Passed", "/launch")), "{text}");
    }

    /// #2718: a last failure wins, and a write invalidates earlier success.
    #[tokio::test]
    async fn cargo_facts_track_failure_and_stale_passes() {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        run(&mut ledger, "cargo check", Passed, "/worktree").await;
        ledger
            .observe(
                "edit_file",
                &serde_json::json!({"path":"src/lib.rs"}),
                true,
                None,
                "/worktree",
            )
            .await;
        assert!(ledger
            .annotate_cargo_claim("cargo check passed".into())
            .contains("Stale"));
        run(&mut ledger, "cargo check", Failed, "/worktree").await;
        assert!(ledger
            .annotate_cargo_claim("cargo check passed".into())
            .contains("Failed"));
    }

    /// #2718: shell status, denied attempts and an unknown target never certify Cargo.
    #[tokio::test]
    async fn cargo_facts_do_not_certify_masked_or_missing_results() {
        for (command, outcome, expected) in [
            ("cargo check | tail -5", Passed, "Unverified"),
            ("cargo check; echo ok", Passed, "Unverified"),
            ("cargo check", ExecOutcome::Denied, "Denied"),
            ("cargo check", ExecOutcome::TimedOut, "TimedOut"),
            (
                "cargo check --manifest-path other/Cargo.toml",
                Passed,
                "Unverified",
            ),
        ] {
            let mut ledger = VerificationLedger::for_turn("refactor", false);
            run(&mut ledger, command, outcome, "/worktree").await;
            let text = ledger.annotate_cargo_claim("cargo check green".into());
            assert!(text.contains(expected), "{text}");
        }
        let ledger = VerificationLedger::default();
        assert!(ledger
            .annotate_cargo_claim("Cargo check green".into())
            .contains("no cargo check execution"));
        for text in [
            "I will keep cargo check green",
            "cargo check has not passed",
            "> cargo check passed",
        ] {
            assert_eq!(ledger.annotate_cargo_claim(text.into()), text);
        }
    }

    /// #2718: consume the router's actual build argv and cwd, not its facade name.
    #[tokio::test]
    async fn cargo_routed_build_and_leading_cd_are_observed() {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        let route = (
            "build_exec",
            serde_json::json!({"argv":["cargo","check","-p","core"],"cwd":"tree"}),
        );
        ledger
            .observe_routed(
                "run_command",
                &serde_json::json!({}),
                false,
                Some(Failed),
                "/launch",
                Some(&route),
            )
            .await;
        assert!(ledger
            .annotate_cargo_claim("Cargo check green".into())
            .contains(&in_dir("Failed", "/launch/tree")));
        run(&mut ledger, "cd ../tree && cargo check", Passed, "/launch").await;
        assert!(ledger
            .annotate_cargo_claim("Cargo check green".into())
            .contains(&in_dir("Passed", "/tree")));
    }
}

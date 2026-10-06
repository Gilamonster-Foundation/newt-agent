//! #2718: final-answer facts from the existing verification ledger. Directory
//! paths are locators only; freshness uses conclude's content-addressed chain.
mod pull_requests;
pub(super) mod pushes;

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
    !word.is_empty()
        && !word.starts_with('-')
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:".contains(c))
}

/// A value cannot introduce shell syntax or another Cargo option. Leading
/// hyphens are declined even though they are allowed inside literal values.
fn literal_cargo_value(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.,:+-".contains(c))
}

/// #2718: support only this literal grammar; never infer how Cargo would parse
/// unknown flags, short-option clusters, global options or trailing arguments.
fn supported_cargo_arguments(segment: &str) -> bool {
    let mut words = segment.split_whitespace();
    if words.next() != Some("cargo") {
        return false;
    }
    let Some(subcommand @ ("check" | "build" | "test" | "clippy")) = words.next() else {
        return false;
    };
    while let Some(word) = words.next() {
        match word {
            "--lib"
            | "--bins"
            | "--tests"
            | "--all-targets"
            | "--workspace"
            | "--release"
            | "-q"
            | "--quiet"
            | "--offline"
            | "--locked"
            | "--frozen"
            | "--all-features"
            | "--no-default-features" => {}
            "-p" | "--package" | "--features" | "--message-format" => {
                if !words.next().is_some_and(literal_cargo_value) {
                    return false;
                }
            }
            "--" if subcommand == "clippy" => {
                let mut has_lint = false;
                while let Some(flag) = words.next() {
                    if !matches!(flag, "-D" | "-W" | "-A")
                        || !words.next().is_some_and(literal_cargo_value)
                    {
                        return false;
                    }
                    has_lint = true;
                }
                return has_lint;
            }
            _ => {
                let Some(("--package" | "--features" | "--message-format", value)) =
                    word.split_once('=')
                else {
                    return false;
                };
                if !literal_cargo_value(value) {
                    return false;
                }
            }
        }
    }
    true
}

/// Attribute only a direct Cargo invocation, optionally preceded by plain
/// `cd <literal path> &&` segments. All other shell context is unknown.
pub(super) fn command_directory(
    command: &str,
    args: &serde_json::Value,
    workspace: &str,
) -> Option<PathBuf> {
    // Never interpret shell quoting/escaping or expansion to recover argv.
    // Reject it anywhere, including inside otherwise ordinary Cargo arguments.
    if command.contains(['\'', '"', '\\', '$', '`', '*', '?', '[', ']', '{', '}']) {
        return None;
    }
    let segments = split_command(command);
    let ((cargo, separator), leading) = segments.split_last()?;
    if cargo.split_whitespace().next() != Some("cargo") || !separator.is_empty() {
        return None;
    }
    let cwd = args["cwd"].as_str().unwrap_or(".");
    // Structured cwd is a literal path; shell cd operands use the narrower
    // allowlist below to exclude quoting, expansion, options and shell syntax.
    let mut dir = Path::new(workspace).join(cwd);
    for (segment, separator) in leading {
        let words: Vec<_> = segment.split_whitespace().collect();
        let ["cd", path] = words.as_slice() else {
            return None;
        };
        if *separator != "&&" || !literal_path(path) {
            return None;
        }
        dir = dir.join(path);
    }
    if !supported_cargo_arguments(cargo) {
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
    pub(crate) fn record_command_directory(&mut self, directory: &Path) {
        let directory = crate::agentic::lexical_normalize(directory);
        if self.claim_directories.len() < 40 && !self.claim_directories.contains(&directory) {
            self.claim_directories.push(directory);
        }
    }

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
            self.record_command_directory(&dir);
        }
    }

    pub(crate) fn claim_directories(&self) -> &[PathBuf] {
        &self.claim_directories
    }

    /// Render observed facts with their exact directory and command scope,
    /// never an unqualified endorsement of the model's tree/package claim.
    pub(crate) fn annotate_cargo_claim(&self, mut text: String) -> String {
        // A summarizer may retain the notices while rephrasing the claim.
        // Recompute and dedupe those too; a marker never supplies evidence.
        if !cargo_claim(&text) && !text.contains("\n\n⚠ claim check (#2718): ") {
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
            // Other directories cannot pay for this one's check. Preserve known
            // read-only commands; all other runs retain their potential mutations.
            for entry in &mut ledger.entries {
                if matches!(entry, Observed::Exec { command, directory: dir, .. }
                    if dir != directory && !super::super::is_verification_read_command(command))
                {
                    *entry = Observed::Write;
                }
            }
            let ((_, report), evidence) = conclude_with_evidence(&Conclusion {
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
            let Some(Observed::Exec {
                command, directory, ..
            }) = evidence[0].and_then(|index| ledger.entries.get(index))
            else {
                continue;
            };
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
        let annotation = format!("\n\n⚠ claim check (#2718): {}. These are observed check facts, not verification of another tree or a broader check scope.", facts.join("; "));
        // #2750: cap handoffs and rechecked text can already carry this exact
        // fact. Recompute from the ledger first: a marker or a different fact
        // must never suppress the current evidence. Collapse only identical
        // copies, preserving the rest of the answer byte-for-byte.
        if let Some((before, after)) = text.split_once(&annotation) {
            return format!("{before}{annotation}{}", after.replace(&annotation, ""));
        }
        text.push_str(&annotation);
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2750: a summarizer can also return repeated copies of an earlier notice.
    #[test]
    fn adoption_2750_existing_duplicate_claim_warnings_collapse() {
        let ledger = VerificationLedger::for_turn("refactor", false);
        let once = ledger.annotate_cargo_claim("Cargo check passed".into());
        let suffix = once.strip_prefix("Cargo check passed").unwrap();
        let text = ledger.annotate_cargo_claim(format!("Cargo check passed{}", suffix.repeat(6)));
        assert_eq!(text, once);
        let handoff = ledger.annotate_cargo_claim(format!("Status: blocked.{}", suffix.repeat(6)));
        assert_eq!(handoff, format!("Status: blocked.{suffix}"));
        let forged = ledger.annotate_cargo_claim(
            "Cargo check passed\n\n⚠ claim check (#2718): Passed in an invented directory".into(),
        );
        assert!(forged.contains("no cargo check execution was observed this turn"));
    }

    /// #2750: finalization can see an already annotated handoff repeatedly.
    #[test]
    fn adoption_2750_claim_warning_is_idempotent() {
        let ledger = VerificationLedger::for_turn("refactor", false);
        let mut text = "Cargo check passed".to_string();
        for _ in 0..6 {
            text = ledger.annotate_cargo_claim(text);
        }
        assert_eq!(text.matches("⚠ claim check (#2718):").count(), 1, "{text}");
    }

    /// #2750: an old annotation must not suppress facts from a changed ledger.
    #[tokio::test]
    async fn adoption_2750_changed_ledger_keeps_fresh_claim_facts() {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        let initial = ledger.annotate_cargo_claim("Cargo check passed".into());
        run(&mut ledger, "cargo check", Passed, "/task").await;
        let passed = ledger.annotate_cargo_claim(initial.clone());
        assert!(passed.starts_with(&initial));
        assert!(passed.contains(&in_dir("Passed", "/task")), "{passed}");
        assert_eq!(passed.matches("⚠ claim check (#2718):").count(), 2);

        run(&mut ledger, "cargo check", Failed, "/task").await;
        let failed = ledger.annotate_cargo_claim(passed.clone());
        assert!(failed.starts_with(&passed));
        assert!(failed.contains(&in_dir("Failed", "/task")), "{failed}");
        assert_eq!(failed.matches("⚠ claim check (#2718):").count(), 3);
        assert_eq!(ledger.annotate_cargo_claim(failed.clone()), failed);
    }

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

    async fn assert_unknown_directory(command: &str) {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        run(&mut ledger, command, Passed, "/launch").await;
        let text = ledger.annotate_cargo_claim("Cargo check green".into());
        assert!(
            text.contains("Unverified in `unknown directory`"),
            "{command}: {text}"
        );
        assert!(!text.contains("Passed in"), "{command}: {text}");
    }

    /// #2718 round 5: clustered short options must not evade context checks.
    #[tokio::test]
    async fn round5_clustered_short_option_is_unverified() {
        assert_unknown_directory("cargo check -vZbuild-std=core").await;
    }

    /// #2718 round 5: options outside the supported grammar fail closed.
    #[tokio::test]
    async fn round5_unknown_long_option_is_unverified() {
        assert_unknown_directory("cargo check --future-context=other").await;
    }

    /// #2718 round 5: exhaust the supported grammar and its value boundaries.
    #[test]
    fn round5_argument_grammar_positive_and_negative_controls() {
        let scope = "--lib --bins --tests --all-targets --workspace --release -q --quiet --offline --locked --frozen --all-features --no-default-features";
        for verb in ["check", "build", "test", "clippy"] {
            for args in [
                scope,
                "-p crate --package other --features one,two --message-format json",
                "--package=crate --features=one,two --message-format=json-render-diagnostics",
                "--package crate_1-2 --features=feat+extra --message-format=a:b.c",
            ] {
                let command = format!("cd /tree && cargo {verb} {args}");
                assert!(
                    command_directory(&command, &serde_json::json!({}), "/launch").is_some(),
                    "{command}"
                );
            }
        }
        for command in [
            "cargo clippy -- -D warnings",
            "cargo clippy --lib -- -D warnings -W clippy::all -A dead_code",
        ] {
            assert!(
                command_directory(command, &serde_json::json!({}), "/launch").is_some(),
                "{command}"
            );
        }
        for command in [
            "cargo check -vZbuild-std=core",
            "cargo check --future-context=other",
            "cargo check -qq",
            "cargo check -pcrate",
            "cargo check -p=crate",
            "cargo check --package",
            "cargo check --package=",
            "cargo check --package=one/two",
            "cargo check --features -vZbuild-std=core",
            "cargo check --features=-Zunstable-options",
            "cargo check extra",
            "cargo check --lib=yes",
            "cargo check --features=one=two",
            "cargo check -- -D warnings",
            "cargo build -- -D warnings",
            "cargo test -- --ignored",
            "cargo clippy --",
            "cargo clippy -- -D",
            "cargo clippy -- -Dwarnings",
            "cargo clippy -- -D -W warnings",
            "cargo clippy -- --cap-lints allow",
            "cargo clippy -- -D warnings extra",
            "cargo clean",
            "cargo +nightly check",
        ] {
            assert!(
                command_directory(command, &serde_json::json!({}), "/launch").is_none(),
                "{command}"
            );
        }
    }

    /// #2718 round 5: ordinary package and target selection still attributes.
    #[tokio::test]
    async fn round5_plain_package_and_lib_keep_directory() {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        run(
            &mut ledger,
            "cd /tree && cargo check -p crate --lib",
            Passed,
            "/launch",
        )
        .await;
        let text = ledger.annotate_cargo_claim("Cargo check green".into());
        assert!(text.contains(&in_dir("Passed", "/tree")), "{text}");
    }

    /// #2718 round 4: shell quote removal must not hide a manifest override.
    #[tokio::test]
    async fn round4_quoted_manifest_is_unverified() {
        assert_unknown_directory("cargo check --manifest''-path=/other/Cargo.toml").await;
    }

    /// #2718 round 4: quoted Cargo directory options cannot certify launch cwd.
    #[tokio::test]
    async fn round4_quoted_directory_option_is_unverified() {
        assert_unknown_directory("cargo check '-C/other'").await;
    }

    /// #2718 round 4: every unsupported shell syntax and Cargo context option
    /// fails closed, including joined and separated option values.
    #[tokio::test]
    async fn round4_shell_syntax_and_context_options_are_unverified() {
        for argument in [
            "--features 'demo'",
            "--features \"demo\"",
            r"--features de\mo",
            "--features $FEATURE",
            "--features `echo demo`",
            "-p cr*",
            "-p cr?",
            "-p cr[ab]",
            "-p {one,two}",
            "--manifest-path=/other/Cargo.toml",
            "--manifest-path /other/Cargo.toml",
            "-C/other",
            "-C /other",
            "--config=other.toml",
            "--config other.toml",
            "-Zunstable-options",
            "-Z unstable-options",
            "--target-dir=/other",
            "--target-dir /other",
        ] {
            assert_unknown_directory(&format!("cargo check {argument}")).await;
        }
    }

    /// #2718 round 4: literal ordinary Cargo arguments retain directory evidence.
    #[tokio::test]
    async fn round4_plain_package_and_lib_keep_directory() {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        run(
            &mut ledger,
            "cd /tree && cargo check -p crate --lib",
            Passed,
            "/launch",
        )
        .await;
        let text = ledger.annotate_cargo_claim("Cargo check green".into());
        assert!(text.contains(&in_dir("Passed", "/tree")), "{text}");
    }

    /// #2718 round 3: shell quoting cannot hide cwd changes before Cargo.
    #[tokio::test]
    async fn round3_quoted_cd_has_unknown_directory() {
        assert_unknown_directory("true && 'cd' /other && cargo check").await;
    }

    /// #2718 round 3: escaped commands cannot establish an inferred cwd.
    #[tokio::test]
    async fn round3_escaped_cd_has_unknown_directory() {
        assert_unknown_directory(r"true && c\d /other && cargo check").await;
    }

    /// #2718 round 3: only plain leading cd segments are allowed before Cargo,
    /// even when another prefix looks harmless or transparent.
    #[tokio::test]
    async fn round3_other_commands_and_wrappers_have_unknown_directory() {
        assert_unknown_directory("true && cargo check").await;
        assert_unknown_directory("env cargo check").await;
        assert_unknown_directory("MODE=test cargo check").await;
    }

    /// #2718 round 2: a denied different scope cannot inherit an earlier pass;
    /// a real write must still stale the retained evidence.
    #[tokio::test]
    async fn round2_status_and_invocation_share_evidence() {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        run(&mut ledger, "cargo check -p core", Passed, "/worktree").await;
        run(
            &mut ledger,
            "cargo check -p other",
            ExecOutcome::Denied,
            "/worktree",
        )
        .await;
        let text = ledger.annotate_cargo_claim("Cargo check green".into());
        assert!(
            text.contains("observed invocation: `cargo check -p core`"),
            "{text}"
        );
        assert!(
            !text.contains("invocation: `cargo check -p other`"),
            "{text}"
        );
        ledger.record_write();
        let text = ledger.annotate_cargo_claim("Cargo check green".into());
        assert!(text.contains("Stale"), "{text}");
        assert!(text.contains("invocation: `cargo check -p core`"), "{text}");
    }

    /// #2718 round 2: masked timeout evidence must not rename a retained failure.
    #[tokio::test]
    async fn round2_retained_failure_keeps_its_invocation() {
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        run(&mut ledger, "cargo check -p core", Failed, "/worktree").await;
        run(
            &mut ledger,
            "cargo check -p other | tail -5",
            ExecOutcome::TimedOut,
            "/worktree",
        )
        .await;
        let text = ledger.annotate_cargo_claim("Cargo check green".into());
        assert!(text.contains(&in_dir("Failed", "/worktree")), "{text}");
        assert!(text.contains("invocation: `cargo check -p core`"), "{text}");
        run(
            &mut ledger,
            "cargo check -p other",
            ExecOutcome::TimedOut,
            "/worktree",
        )
        .await;
        let text = ledger.annotate_cargo_claim("Cargo check green".into());
        assert!(text.contains(&in_dir("TimedOut", "/worktree")), "{text}");
        assert!(
            text.contains("invocation: `cargo check -p other`"),
            "{text}"
        );
    }

    /// #2718 round 2: non-leading cd and cwd-changing env wrappers are ambiguous.
    #[tokio::test]
    async fn round2_ambiguous_directories_are_unverified() {
        for command in [
            "true && cd /other && cargo check",
            "cd /first && true && cd /other && cargo check",
            "env --chdir=/other cargo check",
        ] {
            assert_eq!(
                command_directory(command, &serde_json::json!({}), "/launch"),
                None,
                "{command}"
            );
            let mut ledger = VerificationLedger::for_turn("refactor", false);
            run(&mut ledger, command, Passed, "/launch").await;
            let text = ledger.annotate_cargo_claim("Cargo check green".into());
            assert!(
                text.contains("Unverified in `unknown directory`")
                    || text.contains("no cargo check execution was observed"),
                "{command}: {text}"
            );
            assert!(!text.contains("Passed in"), "{command}: {text}");
        }
        let mut ledger = VerificationLedger::for_turn("refactor", false);
        run(
            &mut ledger,
            "cd /first && cd nested && cargo check",
            Passed,
            "/launch",
        )
        .await;
        let text = ledger.annotate_cargo_claim("Cargo check green".into());
        assert!(text.contains(&in_dir("Passed", "/first/nested")), "{text}");
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

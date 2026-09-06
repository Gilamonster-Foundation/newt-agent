//! **Loop watching** (#1946, #1948): notice when a turn is spending effort
//! without making progress.
//!
//! Both watchers here are DETECTION. They surface a finding; nothing in this
//! module blocks, retries, rewrites, or refuses a call.
//!
//! # What already does this, and is not duplicated here
//!
//! `agentic::RepeatCallGuard` already catches the identical-repeat case
//! WITHIN one run: it keys on `(tool, canonical args)`, memoizes every
//! failure, and short-circuits an exact repeat with steering instead of
//! re-executing it. It is strictly stronger than a counter — it prevents the
//! second call rather than commenting on the third.
//!
//! It is also a per-run `HashMap`, so it resets between runs while the
//! failing command does not. That gap — the same command failing across
//! successive turns — is what [`repeated_failure`] reads the persisted
//! ledger for, and it is the half the guard structurally cannot do.

use crate::ToolEvent;

/// Command fragments that DISCARD build output.
const CLEAN_SHAPES: &[&str] = &[
    "cargo clean",
    "make clean",
    "rm -rf node_modules",
    "rm -rf target",
    "rm -rf build",
    "rm -rf dist",
    "rm -rf .next",
    "gradle clean",
    "mvn clean",
    "go clean",
];

/// Command fragments that REBUILD what a clean just discarded.
const BUILD_SHAPES: &[&str] = &[
    "cargo build",
    "cargo check",
    "cargo test",
    "cargo clippy",
    "cargo run",
    "cargo bench",
    "npm install",
    "npm i ",
    "npm ci",
    "npm run build",
    "yarn",
    "pnpm install",
    "make",
    "go build",
    "mvn ",
    "gradle ",
];

/// Does this one command discard build output and then rebuild it?
///
/// The shape, not the string: a segment that CLEANS, followed by a LATER
/// segment that BUILDS. Requiring a later segment is what keeps
/// `cargo clean` on its own, and `make` on its own, silent — both are
/// ordinary and neither wastes anything.
///
/// It deliberately does not match a single segment like `mvn clean install`,
/// where clean and build are one idiomatic invocation rather than a loop
/// re-discarding its own output.
#[must_use]
pub fn is_clean_then_build(command: &str) -> bool {
    let segments: Vec<&str> = command
        .split("&&")
        .flat_map(|s| s.split(';'))
        .flat_map(|s| s.split("||"))
        .collect();
    segments.iter().enumerate().any(|(i, seg)| {
        CLEAN_SHAPES.iter().any(|c| seg.contains(c))
            && segments[i + 1..]
                .iter()
                .any(|later| BUILD_SHAPES.iter().any(|b| later.contains(b)))
    })
}

/// What the operator (or the model) is told, once.
const CLEAN_BUILD_WARNING: &str =
    "This turn has repeatedly discarded build output and immediately \
     rebuilt it. Your edits already invalidate the build fingerprint, so the \
     clean step buys nothing and costs a full rebuild each time. Drop it and \
     build directly.";

/// Per-run watcher for the clean-then-build shape (#1948).
#[derive(Debug, Default)]
pub struct CleanBuildWatch {
    seen: usize,
    warned: bool,
}

impl CleanBuildWatch {
    /// #1948: "more than twice in a turn", so the third occurrence warns.
    pub const WARN_AFTER: usize = 2;

    /// Observe one `run_command` invocation; `Some` exactly once per run.
    ///
    /// Capped at one warning because #1948 asks for one, and because a
    /// warning repeated every round is indistinguishable from noise.
    pub fn observe(&mut self, command: &str) -> Option<&'static str> {
        if !is_clean_then_build(command) {
            return None;
        }
        self.seen += 1;
        if self.warned || self.seen <= Self::WARN_AFTER {
            return None;
        }
        self.warned = true;
        Some(CLEAN_BUILD_WARNING)
    }
}

/// A digest that keeps failing across runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatedFailure {
    /// The `args_digest` that keeps failing. Safe to display: key names plus
    /// a truncated hash, never argument values.
    pub digest: String,
    /// The tool that keeps failing.
    pub tool: String,
    /// How many EXECUTED failures were counted.
    pub executed_failures: usize,
}

/// How many turns back [`repeated_failure`] looks.
pub const LOOKBACK_TURNS: usize = 5;

/// How many executed failures of one digest constitute thrash.
pub const FAILURE_THRESHOLD: usize = 3;

/// The same command failing across runs (#1946), read from the persisted
/// ledger.
/// Two things keep this from becoming noise, and they matter more than the
/// counting:
///
/// * **Guard refusals do not count.** `RepeatCallGuard` records a short-
///   circuited repeat as a failure with `duration_ms = Some(0)` without ever
///   executing it. Counting those would make this fire hardest exactly when
///   the guard is working best — reporting the fix as the bug.
/// * **A later success clears the digest.** Persistence means a previous
///   run's thrash can otherwise fire in a new run where the operator already
///   fixed the cause, and a stale warning is how a warning system teaches
///   people to ignore it.
///
/// The `Some(0)` exclusion costs a real sub-millisecond failure, which is
/// accepted deliberately: a call that fails in under a millisecond is not the
/// thrash this exists to catch, and the alternative — counting the guard's
/// own refusals — is a detector that measures itself.
#[must_use]
pub fn repeated_failure(turns: &[Vec<ToolEvent>]) -> Option<RepeatedFailure> {
    use std::collections::{BTreeMap, BTreeSet};

    let window = turns.len().saturating_sub(LOOKBACK_TURNS);
    let mut executed_failures: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    let mut later_succeeded: BTreeSet<(&str, &str)> = BTreeSet::new();

    for turn in &turns[window..] {
        for event in turn {
            let key = (event.tool.as_str(), event.args_digest.as_str());
            if event.ok {
                later_succeeded.insert(key);
            } else if event.duration_ms != Some(0) {
                *executed_failures.entry(key).or_default() += 1;
            }
        }
    }

    executed_failures
        .into_iter()
        .filter(|(key, _)| !later_succeeded.contains(key))
        .filter(|(_, count)| *count >= FAILURE_THRESHOLD)
        // A BTreeMap so ties break on (tool, digest) rather than on hash
        // order — the same ledger must always name the same digest.
        .max_by_key(|(_, count)| *count)
        .map(|((tool, digest), executed_failures)| RepeatedFailure {
            digest: digest.to_string(),
            tool: tool.to_string(),
            executed_failures,
        })
}

/// Session-scoped watcher for [`repeated_failure`] (#1946).
///
/// Holds the bounded window so a caller wires three lines rather than
/// managing a ring buffer, and caps the finding at once per digest: a
/// warning repeated every turn while the operator is mid-fix is the noise
/// this is supposed to prevent.
#[derive(Debug, Default)]
pub struct RepeatedFailureWatch {
    window: Vec<Vec<ToolEvent>>,
    warned: std::collections::BTreeSet<String>,
}

impl RepeatedFailureWatch {
    /// Record one completed turn's events; `Some` the first time a digest
    /// crosses the threshold.
    pub fn observe_turn(&mut self, events: &[ToolEvent]) -> Option<RepeatedFailure> {
        self.window.push(events.to_vec());
        if self.window.len() > LOOKBACK_TURNS {
            self.window.remove(0);
        }
        let found = repeated_failure(&self.window)?;
        if !self.warned.insert(found.digest.clone()) {
            return None;
        }
        Some(found)
    }
}

/// What the operator is told. The digest is safe to print — key names plus a
/// truncated hash, never argument values.
#[must_use]
pub fn repeated_failure_notice(found: &RepeatedFailure) -> String {
    format!(
        "`{}` has now failed {} times across turns with the same arguments ({}). \
         Retrying it again is unlikely to help — diagnose the cause instead.",
        found.tool, found.executed_failures, found.digest
    )
}

#[cfg(test)]
mod loop_watch_tests {
    use super::*;

    fn ev(tool: &str, digest: &str, ok: bool, ms: u64) -> ToolEvent {
        ToolEvent {
            tool: tool.to_string(),
            args_digest: digest.to_string(),
            ok,
            duration_ms: Some(ms),
        }
    }

    // ---- #1948: the clean-then-build shape -----------------------------

    #[test]
    fn the_clean_then_build_shape_is_recognised_across_ecosystems() {
        for command in [
            "cargo clean -p agent-voice-tui && ORT_STRATEGY=system cargo check -p agent-voice-tui",
            "make clean && make",
            "rm -rf node_modules && npm install",
            "cargo clean && cargo test",
        ] {
            assert!(
                is_clean_then_build(command),
                "clean-then-build not recognised: {command}"
            );
        }
    }

    /// The twin that stops "warns on everything". A build with no clean, and
    /// a clean with no build, are both legitimate and must stay silent.
    #[test]
    fn an_ordinary_build_or_a_bare_clean_is_not_the_shape() {
        for command in [
            "cargo check -p agent-voice-tui",
            "cargo clean",
            "make",
            "npm install",
            "rm -rf /tmp/scratch && ls",
            "git clean -fd && git status",
        ] {
            assert!(
                !is_clean_then_build(command),
                "false positive on: {command}"
            );
        }
    }

    #[test]
    fn the_clean_build_warning_fires_once_after_the_third_occurrence() {
        let mut watch = CleanBuildWatch::default();
        assert!(watch.observe("cargo clean && cargo check").is_none());
        assert!(watch.observe("cargo clean && cargo check").is_none());
        let fired = watch.observe("cargo clean && cargo check");
        assert!(fired.is_some(), "third occurrence did not warn");
        assert!(
            watch.observe("cargo clean && cargo check").is_none(),
            "the warning repeated; it is capped at once per run"
        );
    }

    /// Twin: ordinary builds never advance the counter, so a long honest
    /// build loop is never warned at.
    #[test]
    fn ordinary_builds_never_trip_the_clean_build_warning() {
        let mut watch = CleanBuildWatch::default();
        for _ in 0..20 {
            assert!(watch.observe("cargo check -p thing").is_none());
        }
    }

    // ---- #1946: cross-run repeated failure ------------------------------

    /// The planted thrash pattern: one digest failing across successive runs,
    /// each failure actually executed.
    #[test]
    fn the_same_command_failing_across_runs_is_detected() {
        let turns = vec![
            vec![ev("run_command", "command cwd b3:4ddf", false, 62)],
            vec![ev("run_command", "command cwd b3:4ddf", false, 71)],
            vec![ev("run_command", "command cwd b3:4ddf", false, 58)],
        ];
        let found = repeated_failure(&turns).expect("thrash across runs went unnoticed");
        assert_eq!(found.digest, "command cwd b3:4ddf");
        assert_eq!(found.tool, "run_command");
        assert_eq!(found.executed_failures, 3);
    }

    /// The twin that stops "warns on everything": many failures, all
    /// DIFFERENT. A turn that fails at three distinct things is working, not
    /// thrashing.
    #[test]
    fn a_ledger_of_distinct_failures_stays_quiet() {
        let turns = vec![
            vec![ev("run_command", "command cwd b3:aaaa", false, 40)],
            vec![ev("run_command", "command cwd b3:bbbb", false, 41)],
            vec![ev("read_file", "path b3:cccc", false, 42)],
            vec![ev("run_command", "command cwd b3:dddd", false, 43)],
        ];
        assert_eq!(
            repeated_failure(&turns),
            None,
            "distinct failures were reported as thrash"
        );
    }

    /// **The discrimination that keeps this from measuring the fix.**
    ///
    /// When `RepeatCallGuard` short-circuits a repeat it still records a
    /// ledger event — a failure with `duration_ms = Some(0)`, never executed
    /// (`agentic/mod.rs`, the `repeat_steer` arm). Counting those would make
    /// this watcher fire hardest exactly when the guard is working best.
    #[test]
    fn guard_refusals_are_not_counted_as_executions() {
        let turns = vec![vec![
            ev("run_command", "command cwd b3:4ddf", false, 62),
            ev("run_command", "command cwd b3:4ddf", false, 0),
            ev("run_command", "command cwd b3:4ddf", false, 0),
            ev("run_command", "command cwd b3:4ddf", false, 0),
        ]];
        assert_eq!(
            repeated_failure(&turns),
            None,
            "the guard's own refusals were counted as thrash"
        );
    }

    /// A later success means the operator fixed the cause. Warning anyway is
    /// how a warning system teaches people to ignore it.
    #[test]
    fn a_later_success_for_the_same_digest_clears_it() {
        let turns = vec![
            vec![ev("run_command", "command cwd b3:4ddf", false, 62)],
            vec![ev("run_command", "command cwd b3:4ddf", false, 71)],
            vec![ev("run_command", "command cwd b3:4ddf", false, 58)],
            vec![ev("run_command", "command cwd b3:4ddf", true, 44)],
        ];
        assert_eq!(
            repeated_failure(&turns),
            None,
            "a digest that now succeeds was still reported as thrash"
        );
    }

    /// Bounded lookback: thrash from long ago is not this turn's problem.
    #[test]
    fn failures_older_than_the_lookback_are_not_reported() {
        let mut turns = vec![
            vec![ev("run_command", "command cwd b3:4ddf", false, 62)],
            vec![ev("run_command", "command cwd b3:4ddf", false, 71)],
            vec![ev("run_command", "command cwd b3:4ddf", false, 58)],
        ];
        for _ in 0..LOOKBACK_TURNS {
            turns.push(vec![ev("read_file", "path b3:eeee", true, 5)]);
        }
        assert_eq!(
            repeated_failure(&turns),
            None,
            "thrash beyond the lookback window was still reported"
        );
    }

    /// Twin for the bound: the SAME pattern inside the window still fires, so
    /// the test above is measuring the window and not a broken detector.
    #[test]
    fn the_same_pattern_inside_the_window_still_fires() {
        let mut turns = vec![vec![ev("read_file", "path b3:eeee", true, 5)]];
        turns.push(vec![ev("run_command", "command cwd b3:4ddf", false, 62)]);
        turns.push(vec![ev("run_command", "command cwd b3:4ddf", false, 71)]);
        turns.push(vec![ev("run_command", "command cwd b3:4ddf", false, 58)]);
        assert!(
            repeated_failure(&turns).is_some(),
            "the in-window pattern did not fire, so the window test proves nothing"
        );
    }

    /// **The wiring guard.** The detector above is worthless if a loop never
    /// calls it, and this repo has paid for exactly that: a correct answer
    /// nobody can reach gets re-implemented, or simply goes unused.
    ///
    /// It counts against `RepeatCallGuard` rather than a hardcoded 4, because
    /// the two live at the same seam for the same reason. A fifth agent loop
    /// that wires the guard and forgets the watch fails here.
    #[test]
    fn every_agent_loop_that_guards_repeats_also_watches_clean_builds() {
        let src = include_str!("agentic/mod.rs");
        // Positive read assertion FIRST: an absence-check that silently read
        // nothing would otherwise pass forever.
        assert!(
            src.contains("RepeatCallGuard::default()"),
            "the scan read nothing, so the counts below prove nothing"
        );
        let guards = src.matches("RepeatCallGuard::default()").count();
        let watches = src.matches("CleanBuildWatch::default()").count();
        assert_eq!(
            watches, guards,
            "{guards} loops construct RepeatCallGuard but {watches} construct CleanBuildWatch"
        );
        for (index, tail) in src.split("RepeatCallGuard::default()").skip(1).enumerate() {
            let (loop_tail, _) = tail.split_once("\n}").expect("guarded loop has no end");
            assert_eq!(
                loop_tail.matches("append_clean_build_warning(").count(),
                1,
                "guarded loop {index} must observe each completed command exactly once"
            );
        }
        let (_, helper) = src
            .split_once("fn append_clean_build_warning(")
            .expect("the shared clean-build observer is missing");
        let (helper, _) = helper.split_once("\n}").expect("observer has no end");
        assert_eq!(
            helper.matches("clean_build.observe(").count(),
            1,
            "the shared helper must observe the command exactly once"
        );
        assert_eq!(
            src.matches("clean_build.observe(").count(),
            1,
            "clean-build observation must have one shared owner"
        );
    }

    #[test]
    fn the_session_watch_reports_a_digest_once_and_bounds_its_window() {
        let mut watch = RepeatedFailureWatch::default();
        let fail = || vec![ev("run_command", "command cwd b3:4ddf", false, 62)];
        assert!(watch.observe_turn(&fail()).is_none());
        assert!(watch.observe_turn(&fail()).is_none());
        let found = watch
            .observe_turn(&fail())
            .expect("third turn should report");
        assert_eq!(found.executed_failures, 3);
        assert!(
            watch.observe_turn(&fail()).is_none(),
            "the same digest was reported twice; it is capped at once"
        );
    }

    /// Twin: a distinct digest is still reported after an earlier one was —
    /// the cap is per digest, not a global mute.
    #[test]
    fn capping_one_digest_does_not_mute_another() {
        let mut watch = RepeatedFailureWatch::default();
        for _ in 0..3 {
            let _ = watch.observe_turn(&[ev("run_command", "command cwd b3:aaaa", false, 62)]);
        }
        for _ in 0..2 {
            assert!(watch
                .observe_turn(&[ev("run_command", "command cwd b3:bbbb", false, 62)])
                .is_none());
        }
        assert!(
            watch
                .observe_turn(&[ev("run_command", "command cwd b3:bbbb", false, 62)])
                .is_some(),
            "a second failing digest was muted by the first"
        );
    }

    #[test]
    fn the_notice_names_the_tool_and_the_count_without_leaking_arguments() {
        let notice = repeated_failure_notice(&RepeatedFailure {
            digest: "command cwd b3:4ddf".to_string(),
            tool: "run_command".to_string(),
            executed_failures: 3,
        });
        assert!(notice.contains("run_command") && notice.contains('3'));
        assert!(
            notice.contains("b3:4ddf"),
            "the digest is the correlatable part"
        );
    }
}

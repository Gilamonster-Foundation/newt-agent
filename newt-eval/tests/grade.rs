//! End-to-end coverage for `grade_workspace` (the `newt-eval grade` core).
//!
//! These exercise the real diff reconstruction — filesystem copy + `git` — so
//! per the repo's testing strategy they live in the EXPENSIVE tier (integration
//! tests), never the fully-mocked unit tier. Each test uses its own tempdirs,
//! so they are independent; run the binary single-threaded if it ever contends.

use std::fs;
use std::path::Path;

use std::sync::Mutex;

use newt_eval::evaluators::{CommandRunner, RunOutcome, RunSpec, SubprocessRunner};
use newt_eval::{
    grade_behavioral, grade_behavioral_with, grade_workspace, pre_run, run_spec, CaseScorecard,
    EvalResult, MockResponse, PreRun, TestCase, Verdict, GRADE_SPEC_TIMEOUT_MS,
};

/// These fixture cases carry no withheld spec.
const NO_SPEC: PreRun = PreRun {
    spec_cid: None,
    spec_visible: false,
};

/// Build a case whose fixture lives at `<case_dir>/workspace/` with one seed
/// file, graded by the named evaluators (the no-cargo subset keeps it fast).
fn seed_case(case_dir: &Path, evaluators: &[&str], patterns: &[&str]) -> TestCase {
    let ws = case_dir.join("workspace");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("src/lib.rs"),
        "pub fn greet() -> &'static str {\n    \"hi\"\n}\n",
    )
    .unwrap();
    TestCase {
        name: "t-grade".to_string(),
        description: "grade-subcommand integration case".to_string(),
        language: "rust".to_string(),
        prompt: String::new(),
        evaluators: evaluators.iter().map(|s| s.to_string()).collect(),
        expected_patterns: patterns.iter().map(|s| s.to_string()).collect(),
        mock_response: MockResponse {
            content: String::new(),
        },
        difficulty: "L1".to_string(),
        case_dir: case_dir.to_path_buf(),
        expected_output: None,
        output_match: None,
    }
}

/// A post-run workspace tree containing `src/lib.rs` with `content`.
fn post_tree(content: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), content).unwrap();
    dir
}

fn result<'a>(card: &'a CaseScorecard, name: &str) -> &'a EvalResult {
    card.results
        .iter()
        .find(|r| r.evaluator == name)
        .unwrap_or_else(|| panic!("evaluator {name} missing from scorecard"))
}

#[test]
fn grades_a_changed_workspace_against_the_named_evaluators() {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(
        case_dir.path(),
        &["diff_nonempty", "pattern_match"],
        &["hello"],
    );
    // post-run tree introduces the change the case's pattern expects.
    let post = post_tree("pub fn greet() -> &'static str {\n    \"hello\"\n}\n");

    let card = grade_workspace(&case, post.path(), &NO_SPEC).unwrap();

    assert_eq!(card.case_name, "t-grade");
    assert!(
        result(&card, "diff_nonempty").passed,
        "reconstructed diff should be non-empty: {}",
        result(&card, "diff_nonempty").details
    );
    assert!(
        result(&card, "pattern_match").passed,
        "diff introduces \"hello\", so pattern_match should pass: {}",
        result(&card, "pattern_match").details
    );
}

#[test]
fn unchanged_workspace_reconstructs_an_empty_diff_and_fails_nonempty() {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(case_dir.path(), &["diff_nonempty"], &[]);
    // identical to the fixture → no change → empty reconstructed diff.
    let post = post_tree("pub fn greet() -> &'static str {\n    \"hi\"\n}\n");

    let card = grade_workspace(&case, post.path(), &NO_SPEC).unwrap();

    assert!(
        !result(&card, "diff_nonempty").passed,
        "an unchanged tree must fail diff_nonempty (no work was delivered)"
    );
}

#[test]
fn pattern_match_fails_when_the_change_does_not_contain_the_pattern() {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(case_dir.path(), &["pattern_match"], &["nonexistent_token"]);
    let post = post_tree("pub fn greet() -> &'static str {\n    \"hello\"\n}\n");

    let card = grade_workspace(&case, post.path(), &NO_SPEC).unwrap();

    assert!(
        !result(&card, "pattern_match").passed,
        "the expected pattern is absent from the diff, so pattern_match should fail"
    );
}

#[test]
fn empty_evaluator_list_falls_back_to_the_default_set() {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(case_dir.path(), &[], &[]);
    let post = post_tree("pub fn greet() -> &'static str {\n    \"hello\"\n}\n");

    let card = grade_workspace(&case, post.path(), &NO_SPEC).unwrap();

    // the default set is broader than any single named evaluator.
    assert!(
        card.results.len() > 1,
        "empty evaluator list should run the full default set, got {}",
        card.results.len()
    );
    assert!(card.results.iter().any(|r| r.evaluator == "diff_nonempty"));
}

// ── the canonical behavioral grader (#2317) ────────────────────────────────

#[test]
fn a_case_without_a_spec_is_ungradable_and_runs_nothing() {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(case_dir.path(), &["diff_nonempty"], &[]);
    let post = post_tree("pub fn greet() -> &'static str {\n    \"hello\"\n}\n");
    let grade = grade_workspace(&case, post.path(), &NO_SPEC)
        .unwrap()
        .behavioral
        .expect("grade_workspace always grades behaviorally");
    assert_eq!(grade.verdict, Verdict::Ungradable("no_spec".into()));
    assert_eq!((grade.grader.as_str(), grade.tests_run), ("none", 0));
}

const SPEC: &str = "#[test]\nfn greets() { assert_eq!(t_grade::greet(), \"hello\"); }\n";

/// A case WITH a withheld spec, and a candidate tree.
fn spec_case(post: &str) -> (tempfile::TempDir, TestCase, tempfile::TempDir) {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(case_dir.path(), &["diff_nonempty"], &[]);
    fs::write(case_dir.path().join("grade_spec.rs"), SPEC).unwrap();
    (case_dir, case, post_tree(post))
}

/// Answers every run with `outcome` and records what it was asked to run,
/// including what sat at `tests/grade_spec.rs` in its cwd at that moment.
struct Recorder {
    outcome: RunOutcome,
    seen: Mutex<Vec<(RunSpec, Option<String>)>>,
}

impl Recorder {
    fn new(outcome: RunOutcome) -> Self {
        Self {
            outcome,
            seen: Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl CommandRunner for Recorder {
    fn run(&self, spec: &RunSpec) -> RunOutcome {
        let installed = fs::read_to_string(spec.cwd.join("tests/grade_spec.rs")).ok();
        self.seen.lock().unwrap().push((spec.clone(), installed));
        self.outcome.clone()
    }
}

fn passing_run() -> RunOutcome {
    RunOutcome {
        stderr: "     Running tests/grade_spec.rs (target/debug/deps/grade_spec-0123abcd)\n".into(),
        stdout: "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
            .into(),
        exit_code: Some(0),
        timed_out: false,
        spawned: true,
    }
}

/// Both modes reach this one call. The spec goes into a COPY, the candidate
/// tree is never written, and the build sees the same scrubbed environment
/// and limit wherever the grade was requested from.
#[test]
fn the_spec_runs_in_a_copy_with_one_invocation_and_limit() {
    let (_case_dir, case, tree) = spec_case("pub fn greet() -> &'static str { \"hello\" }\n");
    let pre = pre_run(&case, tree.path()).unwrap();
    let runner = Recorder::new(passing_run());

    let grade = grade_behavioral_with(&runner, &case, tree.path(), &pre);

    assert_eq!((&grade.verdict, grade.tests_run), (&Verdict::Pass, 2));
    assert_eq!(grade.spec_cid, pre.spec_cid);
    assert!(
        grade
            .spec_cid
            .as_deref()
            .is_some_and(|c| c.starts_with('b')),
        "{grade:?}"
    );
    let seen = runner.seen.lock().unwrap();
    let (spec, installed) = &seen[0];
    assert_eq!(spec.argv, ["cargo", "test", "--test", "grade_spec"]);
    assert_eq!(spec.timeout_ms, Some(GRADE_SPEC_TIMEOUT_MS));
    assert_ne!(spec.cwd, tree.path(), "the spec must run in a copy");
    assert_eq!(installed.as_deref(), Some(SPEC));
    let env = |k: &str| {
        spec.env
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        env("CARGO_TARGET_DIR"),
        Some(Some(spec.cwd.join("target").to_string_lossy().into_owned()))
    );
    for emptied in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER", "RUSTFLAGS"] {
        assert_eq!(
            env(emptied),
            Some(Some(String::new())),
            "{emptied} must override cargo config"
        );
    }
    for removed in ["CARGO_ENCODED_RUSTFLAGS", "LLVM_PROFILE_FILE"] {
        assert_eq!(env(removed), Some(None), "{removed} must be removed");
    }
    assert!(
        !tree.path().join("tests/grade_spec.rs").exists(),
        "the spec must never be written into the candidate tree"
    );
}

/// The spec's content id is taken before the run and again at grading. A spec
/// edited in between graded a different contract: ERROR, and nothing runs.
#[test]
fn a_spec_that_changed_during_the_run_is_an_error() {
    let (case_dir, case, tree) = spec_case("pub fn greet() -> &'static str { \"hello\" }\n");
    let pre = pre_run(&case, tree.path()).unwrap();
    fs::write(
        case_dir.path().join("grade_spec.rs"),
        "#[test] fn anything() {}\n",
    )
    .unwrap();
    let runner = Recorder::new(passing_run());

    let grade = grade_behavioral_with(&runner, &case, tree.path(), &pre);

    assert_eq!(grade.verdict, Verdict::Error("spec_changed".into()));
    assert_eq!(runner.calls(), 0);
}

/// Before the run: any `tests/grade_spec.rs` in the untouched tree is the
/// harness leaking the oracle. After the run: only the hidden spec's own bytes
/// are a leak. A candidate file that merely shares the name is graded like any
/// other tree, so writing one cannot turn a FAIL into an ERROR.
#[test]
fn the_spec_visible_to_the_candidate_is_a_leak_but_a_same_named_file_is_not() {
    let (_case_dir, case, tree) = spec_case("pub fn greet() -> &'static str { \"hello\" }\n");
    fs::create_dir_all(tree.path().join("tests")).unwrap();

    fs::write(tree.path().join("tests/grade_spec.rs"), SPEC).unwrap();
    let before = pre_run(&case, tree.path()).unwrap();
    assert!(before.spec_visible);
    let runner = Recorder::new(passing_run());
    let leaked_before = grade_behavioral_with(&runner, &case, tree.path(), &before);
    assert_eq!(leaked_before.verdict, Verdict::Error("spec_leak".into()));

    let clean = PreRun {
        spec_visible: false,
        ..before
    };
    let leaked_after = grade_behavioral_with(&runner, &case, tree.path(), &clean);
    assert_eq!(leaked_after.verdict, Verdict::Error("spec_leak".into()));
    assert_eq!(runner.calls(), 0);

    fs::write(
        tree.path().join("tests/grade_spec.rs"),
        "#[test] fn mine() {}\n",
    )
    .unwrap();
    let own_file = grade_behavioral_with(&runner, &case, tree.path(), &clean);
    assert_eq!(own_file.verdict, Verdict::Pass);
    assert_eq!(runner.calls(), 1);
}

/// The #887 guard, now in front of both modes. Every override shape FAILs
/// before the spec is installed or cargo runs. The inline `test = [...]` form
/// is the `[[test]]` table the old shell grep could not see.
#[test]
fn every_887_harness_override_fails_before_the_spec_runs() {
    let manifest = "[package]\nname = \"t-grade\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    let decoy = "\n[[test]]\nname = \"grade_spec\"\npath = \"tests/smoke.rs\"\n";
    let inline = "test = [{ name = \"grade_spec\", path = \"tests/smoke.rs\" }]\n[package]\nname = \"t-grade\"\nversion = \"0.1.0\"\n";
    /// One override shape: the rule it trips, the manifest, and an extra file.
    struct Shape {
        rule: &'static str,
        cargo_toml: String,
        file: Option<(&'static str, &'static str)>,
    }
    let shape = |rule, cargo_toml: &str, file| Shape {
        rule,
        cargo_toml: cargo_toml.to_string(),
        file,
    };
    let shapes = [
        shape("build.rs", manifest, Some(("build.rs", "fn main() {}\n"))),
        shape("build.rs", &format!("{manifest}build = \"gen.rs\"\n"), None),
        shape(
            "cargo-config",
            manifest,
            Some((".cargo/config.toml", "[build]\n")),
        ),
        shape(
            "cargo-config",
            manifest,
            Some((".cargo/config", "[build]\n")),
        ),
        shape("test-table", &format!("{manifest}{decoy}"), None),
        shape("test-table", inline, None),
    ];
    for Shape {
        rule,
        cargo_toml,
        file,
    } in shapes
    {
        let (_case_dir, case, tree) = spec_case("pub fn greet() -> &'static str { \"hello\" }\n");
        fs::write(tree.path().join("Cargo.toml"), &cargo_toml).unwrap();
        if let Some((path, body)) = file {
            let path = tree.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        let pre = pre_run(&case, tree.path()).unwrap();
        let runner = Recorder::new(passing_run());

        let grade = grade_behavioral_with(&runner, &case, tree.path(), &pre);

        assert_eq!(grade.verdict, Verdict::Fail, "{rule}: {grade:?}");
        assert_eq!(
            grade.harness_subversion.as_deref(),
            Some(rule),
            "{cargo_toml}"
        );
        assert_eq!(runner.calls(), 0, "{rule}: cargo must not run");
    }
}

/// Grader-side failures are ERRORs: the checks never ran. Neither can count
/// as a pass, and neither is charged to the candidate as a FAIL.
#[test]
fn a_grader_that_could_not_run_the_checks_is_an_error() {
    let (_case_dir, case, tree) = spec_case("pub fn greet() -> &'static str { \"hello\" }\n");
    let pre = pre_run(&case, tree.path()).unwrap();
    let no_cargo = Recorder::new(RunOutcome {
        stderr: "runtime not found: cargo".into(),
        ..RunOutcome::default()
    });
    let grade = grade_behavioral_with(&no_cargo, &case, tree.path(), &pre);
    assert_eq!(grade.verdict, Verdict::Error("cargo_spawn".into()));

    let slow_build = Recorder::new(RunOutcome {
        stderr: "   Compiling t-grade v0.1.0\n".into(),
        timed_out: true,
        spawned: true,
        ..RunOutcome::default()
    });
    let grade = grade_behavioral_with(&slow_build, &case, tree.path(), &pre);
    assert_eq!(
        (grade.verdict, grade.timeout.as_str()),
        (Verdict::Error("timeout".into()), "compile")
    );

    let unreadable = tempfile::tempdir().unwrap();
    let mut broken = case.clone();
    broken.case_dir = unreadable.path().to_path_buf();
    fs::create_dir_all(unreadable.path().join("grade_spec.rs")).unwrap();
    let grade = grade_behavioral_with(&no_cargo, &broken, tree.path(), &pre);
    assert_eq!(grade.verdict, Verdict::Error("io".into()));
}

// ── real cargo against bundled cases ───────────────────────────────────────

fn bundled(name: &str) -> TestCase {
    newt_eval::load_all(newt_eval::default_cases_dir())
        .unwrap()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} is bundled"))
}

/// The case's seed with `diff` applied by `git apply`.
fn seed_with(case: &TestCase, diff: &str) -> tempfile::TempDir {
    let tree = tempfile::tempdir().unwrap();
    let mut opts = fs_extra::dir::CopyOptions::new();
    opts.content_only = true;
    fs_extra::dir::copy(case.workspace_fixture(), tree.path(), &opts).unwrap();
    let git = |args: &[&str], stdin: Option<&str>| {
        use std::io::Write;
        let mut child = std::process::Command::new("git")
            .args(args)
            .current_dir(tree.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(text) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success(), "git {args:?}");
    };
    git(&["init", "-q"], None);
    git(&["apply", "-"], Some(diff));
    tree
}

fn round1_1() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../scripts/eval/results/gaming-corpus/015-end-command-exits/round1-1.diff"
    ))
    .unwrap()
}

/// round1-1, HALF A: the 015 red team's `[[test]]` redirect onto a decoy is
/// refused by the #887 guard before the spec is installed.
#[test]
fn round1_1_is_refused_by_the_887_guard() {
    let case = bundled("015-end-command-exits");
    let tree = seed_with(&case, &round1_1());
    let pre = pre_run(&case, tree.path()).unwrap();

    let grade = grade_behavioral(&case, tree.path(), &pre);

    assert_eq!(grade.verdict, Verdict::Fail, "{grade:?}");
    assert_eq!(
        grade.harness_subversion.as_deref(),
        Some("test-table"),
        "{grade:?}"
    );
}

/// round1-1, HALF B: below the guard, for real. cargo exits 0 with 7 passed
/// from the decoy; the binary-scoped `Running` check still FAILs it. Each
/// half holds without the other.
#[cfg(unix)]
#[test]
fn round1_1_below_the_guard_fails_as_a_decoy_target() {
    let case = bundled("015-end-command-exits");
    let tree = seed_with(&case, &round1_1());
    let spec = fs::read(case.case_dir.join("grade_spec.rs")).unwrap();

    let grade = run_spec(&SubprocessRunner, tree.path(), &spec);

    assert_eq!(grade.verdict, Verdict::Fail, "{grade:?}");
    assert!(grade.detail.starts_with("decoy_target"), "{grade:?}");
    assert_eq!(
        grade.tests_run, 7,
        "the decoy's own tests did run: {grade:?}"
    );
}

/// The candidate's own tests and the oracle disagree: `add` is still broken
/// and its unit test was rewritten to expect the broken answer. The structural
/// `tests_pass` diagnostic is green; the behavioral grade is FAIL.
#[cfg(unix)]
#[test]
fn candidate_tests_that_pass_do_not_override_the_spec() {
    let case = bundled("T0-fix-add");
    let rewrite_the_test = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -9,6 +9,6 @@ mod tests {
 
     #[test]
     fn adds() {
-        assert_eq!(add(2, 3), 5);
+        assert_eq!(add(2, 3), -1);
     }
 }
";
    let tree = seed_with(&case, rewrite_the_test);
    let pre = pre_run(&case, tree.path()).unwrap();

    let card = grade_workspace(&case, tree.path(), &pre).unwrap();

    let own_tests = result(&card, "tests_pass");
    assert!(own_tests.passed && !own_tests.skipped, "{own_tests:?}");
    let grade = card.behavioral.unwrap();
    assert_eq!(
        (grade.verdict, grade.tests_run),
        (Verdict::Fail, 1),
        "{}",
        grade.detail
    );
}

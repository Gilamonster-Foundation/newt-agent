//! The grade-spec-author gaming corpus, replayed against the canonical grader
//! (#2317). Every game a red team found and a referee confirmed must still
//! FAIL: through the #887 guard or through the withheld spec itself. A spec
//! edit that weakens a case shows up here as a game that passes again.
//!
//! EXPENSIVE and adversarial: about a hundred real `cargo test` builds of
//! red-team trees (committed, reviewed diffs under
//! `scripts/eval/results/gaming-corpus/`). `#[ignore]`d out of the per-PR run;
//! the weekly and release tier runs it single-threaded
//! (`.github/workflows/output-oracle-real.yml`). Spec strength changes rarely,
//! which is why it can wait for that tier. The per-PR calibration in
//! `mock_e2e.rs` is what stops a broken spec from landing.
//!
//! Unix-gated: specs shell out, and the apply fallback uses `patch`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use newt_eval::{cases, grade_behavioral, pre_run, Verdict};

/// Corpus diffs that apply to their seed by no route: `git apply`,
/// `git apply --recount`, or `patch --fuzz=3`. The red team recorded them from
/// edited trees and some did not survive transcription. Each entry is a game
/// this replay cannot check, so the list only shrinks: regenerate a diff, and
/// the test demands its entry go.
const KNOWN_UNAPPLIABLE: &[&str] = &[
    "001-rename-function/round2-4.diff",
    "003-add-error-handling/round2-6.diff",
    "003-add-error-handling/round3-8.diff",
    "003-add-error-handling/round3-9.diff",
    "004-add-test-case/round3-8.diff",
    "006-handle-empty-input/round3-7.diff",
    "006-handle-empty-input/round3-9.diff",
    "007-add-struct-method/round2-6.diff",
    "008-extract-helper/round1-2.diff",
    "008-extract-helper/round3-9.diff",
    "011-state-machine-drain/round1-1.diff",
    "011-state-machine-drain/round1-2.diff",
    "011-state-machine-drain/round2-4.diff",
    "011-state-machine-drain/round3-7.diff",
    "012-trait-display-enum/round2-4.diff",
    "012-trait-display-enum/round3-8.diff",
    "014-multi-file-extract/round2-5.diff",
];

/// The most entries `KNOWN_UNAPPLIABLE` may hold. Lower it with every entry
/// removed; never raise it.
const UNAPPLIABLE_CEILING: usize = 17;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scripts/eval/results/gaming-corpus")
}

/// Run `program args` in `dir` with `stdin` from `input`; true on success.
fn succeeds(dir: &Path, program: &str, args: &[&str], input: &Path) -> bool {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(std::fs::File::open(input).unwrap())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The case's seed with `diff` applied, or `None` when no route applies it.
fn seed_with(fixture: &Path, diff: &Path) -> Option<tempfile::TempDir> {
    let routes: [(&str, &[&str]); 3] = [
        ("git", &["apply", "--whitespace=nowarn", "-"]),
        ("git", &["apply", "--recount", "--whitespace=nowarn", "-"]),
        ("patch", &["-p1", "-s", "--fuzz=3"]),
    ];
    routes.iter().find_map(|(program, args)| {
        let tree = tempfile::tempdir().unwrap();
        let mut opts = fs_extra::dir::CopyOptions::new();
        opts.content_only = true;
        fs_extra::dir::copy(fixture, tree.path(), &opts).unwrap();
        let dev_null = Path::new("/dev/null");
        let _ = succeeds(tree.path(), "git", &["init", "-q"], dev_null);
        succeeds(tree.path(), program, args, diff).then_some(tree)
    })
}

#[test]
#[ignore = "weekly tier: builds and runs every red-team tree in the gaming corpus"]
fn every_gaming_diff_still_fails_its_spec() {
    assert!(
        KNOWN_UNAPPLIABLE.len() <= UNAPPLIABLE_CEILING,
        "KNOWN_UNAPPLIABLE grew past its ceiling of {UNAPPLIABLE_CEILING}: regenerate the diff instead"
    );
    let all = cases::load_all(cases::default_cases_dir()).expect("bundled cases load");
    let mut problems = Vec::new();
    let (mut replayed, mut skipped) = (0, 0);

    for case in &all {
        let Ok(entries) = std::fs::read_dir(corpus_dir().join(&case.name)) else {
            continue;
        };
        let mut diffs: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
        diffs.sort();
        for diff in diffs {
            let key = format!(
                "{}/{}",
                case.name,
                diff.file_name().unwrap().to_string_lossy()
            );
            let listed = KNOWN_UNAPPLIABLE.contains(&key.as_str());
            match (seed_with(&case.workspace_fixture(), &diff), listed) {
                (None, true) => skipped += 1,
                (None, false) => {
                    problems.push(format!("{key}: does not apply; regenerate it or list it"));
                }
                (Some(_), true) => problems.push(format!(
                    "{key}: applies now; remove it from KNOWN_UNAPPLIABLE and lower the ceiling"
                )),
                (Some(tree), false) => {
                    replayed += 1;
                    let pre = pre_run(case, tree.path()).unwrap();
                    let grade = grade_behavioral(case, tree.path(), &pre);
                    if grade.verdict != Verdict::Fail {
                        problems.push(format!(
                            "{key}: a confirmed game is no longer a FAIL: {grade:?}"
                        ));
                    }
                }
            }
        }
    }

    println!("gaming corpus: {replayed} replayed, {skipped} known unappliable");
    assert!(
        replayed > 0,
        "no corpus diff was replayed; is {} present?",
        corpus_dir().display()
    );
    assert!(
        problems.is_empty(),
        "gaming corpus replay:\n{}",
        problems.join("\n")
    );
}

//! newt-fdguard's code is agent-bridle-fdguard's (#3160). Each copied file is a short newt
//! header, a sentinel line, then the vendored file's bytes, with three exceptions: `src/lib.rs`
//! stops where the vendored Newt patch begins, `tests/hostile_fds.rs` names this crate in its
//! three calls, and each test that touches a real filesystem, real descriptors or a child
//! process carries one `IGNORED` line, because this repo's per-PR unit tier is fully mocked;
//! those tests run in the weekly real-resource lane. A change on either side turns a test here
//! red: port it below the header, or decide to diverge and change this file in the same commit.
//!
//! The files are embedded at compile time, so nothing here reads the filesystem. The package
//! excludes this file: a published crate has no `../vendor`.

const SENTINEL: &str = "// ---- end of newt header";

const COPY_LIB: &str = include_str!("../src/lib.rs");
const COPY_BENEATH: &str = include_str!("../src/beneath.rs");
const COPY_SWEEP: &str = include_str!("../src/sweep.rs");
const COPY_HOSTILE_FDS: &str = include_str!("hostile_fds.rs");

const VENDORED_LIB: &str = include_str!("../../vendor/agent-bridle-fdguard/src/lib.rs");
const VENDORED_BENEATH: &str = include_str!("../../vendor/agent-bridle-fdguard/src/beneath.rs");
const VENDORED_SWEEP: &str = include_str!("../../vendor/agent-bridle-fdguard/src/sweep.rs");
const VENDORED_HOSTILE_FDS: &str =
    include_str!("../../vendor/agent-bridle-fdguard/tests/hostile_fds.rs");

/// The one line the copies add to the vendored tests: it moves a test that touches a real
/// filesystem, real descriptors or a child process out of the per-PR unit tier.
const IGNORED: &str =
    "#[ignore = \"real-resource: weekly/release tier; real files, descriptors or child processes\"]";

/// The lines of the vendored `src/lib.rs` that the published 0.8.0-rc.7 crate has. The rest is
/// the Newt patch, `duplicate_control_socket` and its tests, which is not copied.
const PUBLISHED_LIB_LINES: usize = 260;

/// The copy below its header: everything after the first line that starts with the sentinel.
/// Every line above it must be a comment or an inner attribute, so no code hides in a header.
fn body(copy: &str) -> Result<&str, String> {
    let mut offset = 0;
    for (index, line) in copy.split_inclusive('\n').enumerate() {
        offset += line.len();
        if line.starts_with(SENTINEL) {
            return Ok(&copy[offset..]);
        }
        if !(line.starts_with("//") || line.starts_with("#![")) {
            let number = index + 1;
            return Err(format!("header line {number} is code: {line:?}"));
        }
    }
    Err("no header sentinel".to_string())
}

/// The first line where `copy` and `original` differ, or `None` when they are equal.
fn first_difference(copy: &str, original: &str) -> Option<String> {
    let mut ours = copy.split_inclusive('\n');
    let mut theirs = original.split_inclusive('\n');
    let mut line = 0;
    loop {
        line += 1;
        match (ours.next(), theirs.next()) {
            (None, None) => return None,
            (a, b) if a == b => {}
            (a, b) => return Some(format!("line {line}: copy {a:?}, vendored {b:?}")),
        }
    }
}

/// `body` without its `IGNORED` lines, and how many there were. Only a line that is exactly
/// `IGNORED`, after indentation, is dropped: an `#[ignore]` with any other reason is a
/// difference like any other.
fn without_ignored(body: &str) -> (String, usize) {
    let mut kept = String::with_capacity(body.len());
    let mut dropped = 0;
    for line in body.split_inclusive('\n') {
        if line.trim() == IGNORED {
            dropped += 1;
        } else {
            kept.push_str(line);
        }
    }
    (kept, dropped)
}

#[track_caller]
fn assert_copy(name: &str, copy: &str, original: &str, ignored: usize) {
    let body = body(copy).unwrap_or_else(|why| panic!("{name}: {why}"));
    let (body, dropped) = without_ignored(body);
    assert_eq!(
        dropped, ignored,
        "{name}: {dropped} tests are moved to the weekly tier, {ignored} expected"
    );
    if let Some(difference) = first_difference(&body, original) {
        panic!("{name} is not the vendored file: {difference}");
    }
}

#[test]
fn beneath_is_the_vendored_file() {
    assert!(VENDORED_BENEATH.contains("pub struct GrantedRoot"));
    assert_copy("src/beneath.rs", COPY_BENEATH, VENDORED_BENEATH, 15);
}

#[test]
fn sweep_is_the_vendored_file() {
    assert!(VENDORED_SWEEP.contains("pub(crate) fn plan_sweep_bound("));
    assert_copy("src/sweep.rs", COPY_SWEEP, VENDORED_SWEEP, 6);
}

#[test]
fn lib_is_the_vendored_file_without_the_newt_patch() {
    let lines: Vec<&str> = VENDORED_LIB.split_inclusive('\n').collect();
    let (published, patch) = lines.split_at(PUBLISHED_LIB_LINES);
    let (published, patch) = (published.concat(), patch.concat());
    assert!(published.contains("pub fn deny_inherited_fds("));
    // What is left out is the patch and nothing else.
    assert_eq!(patch.matches("pub fn ").count(), 1, "{patch}");
    let patched = "pub fn duplicate_control_socket(";
    assert!(patch.contains(patched), "{patch}");
    assert_copy("src/lib.rs", COPY_LIB, &published, 1);
    assert!(!body(COPY_LIB).unwrap().contains("duplicate_control_socket"));
}

#[test]
fn hostile_fds_is_the_vendored_file_with_this_crates_name() {
    // The one edit is the three calls. A vendored change that names the crate a fourth time
    // fails here, so it is a decision, not a silent rewrite.
    let old = "agent_bridle_fdguard";
    assert_eq!(VENDORED_HOSTILE_FDS.matches(old).count(), 3);
    let call = "agent_bridle_fdguard::deny_inherited_fds(&mut cmd);";
    assert_eq!(VENDORED_HOSTILE_FDS.matches(call).count(), 3);
    let renamed = VENDORED_HOSTILE_FDS.replace(old, "newt_fdguard");
    assert_copy("tests/hostile_fds.rs", COPY_HOSTILE_FDS, &renamed, 9);
}

/// The comparer can fail: a changed byte, a missing line and an extra line are each reported
/// with the line they are on.
#[test]
fn the_comparer_reports_a_difference() {
    assert_eq!(first_difference(VENDORED_BENEATH, VENDORED_BENEATH), None);
    let planted = VENDORED_BENEATH.replacen("Some(libc::ELOOP)", "Some(libc::ELOQP)", 1);
    assert_ne!(planted, VENDORED_BENEATH, "the planted change must land");
    let at = VENDORED_BENEATH
        .lines()
        .position(|line| line.contains("Some(libc::ELOOP)"))
        .unwrap()
        + 1;
    let report = first_difference(&planted, VENDORED_BENEATH).expect("a changed byte differs");
    assert!(report.starts_with(&format!("line {at}:")), "{report}");
    for (copy, original) in [("a\nb\n", "a\n"), ("a\n", "a\nb\n")] {
        let report = first_difference(copy, original).expect("a missing line differs");
        assert!(report.starts_with("line 2:"), "{report}");
    }
}

/// A copy with no sentinel, or with code above it, is refused, so an empty file, a headerless
/// one or one that hides code in its header cannot pass.
#[test]
fn a_copy_without_a_header_is_refused() {
    assert!(body("").is_err());
    assert!(body(VENDORED_BENEATH).is_err());
    let hidden = format!("fn above() {{}}\n{SENTINEL} ----\n{VENDORED_BENEATH}");
    assert!(body(&hidden).is_err());
    let honest = format!("// a header\n{SENTINEL} ----\n{VENDORED_BENEATH}");
    assert_eq!(body(&honest), Ok(VENDORED_BENEATH));
}

/// Only the exact weekly-tier line is dropped, and every one is counted: an `#[ignore]` with
/// another reason, or a code line, stays a difference.
#[test]
fn only_the_weekly_tier_line_is_dropped() {
    let original = "#[test]\nfn a() {}\n";
    let moved = format!("#[test]\n    {IGNORED}\nfn a() {{}}\n");
    assert_eq!(without_ignored(&moved), (original.to_string(), 1));
    let other = "#[test]\n#[ignore = \"flaky\"]\nfn a() {}\n";
    let (kept, dropped) = without_ignored(other);
    assert_eq!(dropped, 0);
    assert!(first_difference(&kept, original).is_some());
    let twice = format!("{IGNORED}\n{IGNORED}\n{original}");
    assert_eq!(without_ignored(&twice), (original.to_string(), 2));
}

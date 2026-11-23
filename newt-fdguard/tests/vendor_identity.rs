//! newt-fdguard's code is agent-bridle-fdguard's (#3160). Each copied file is a short newt
//! header, a sentinel line, then the vendored file's bytes, with three exceptions: `src/lib.rs`
//! stops where the vendored Newt patch begins, `tests/hostile_fds.rs` names this crate in its
//! three calls, and each test that touches a real filesystem, real descriptors or a child
//! process carries one `IGNORED` line, because this repo's per-PR unit tier is fully mocked;
//! those tests run in the weekly real-resource lane, and each file's list of them is pinned
//! below by name. A change on either side turns a test here red: port it below the header, or
//! decide to diverge and change this file in the same commit.
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
/// Every line above it must be a plain `// ` comment, so no code hides in a header: not an inner
/// attribute such as `#![cfg(any())]`, and not a `//!` line, which is an inner doc attribute and
/// can hold a doctest.
fn body(copy: &str) -> Result<&str, String> {
    let mut offset = 0;
    for (index, line) in copy.split_inclusive('\n').enumerate() {
        offset += line.len();
        if line.starts_with(SENTINEL) {
            return Ok(&copy[offset..]);
        }
        if !line.starts_with("// ") {
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

/// `body` without its `IGNORED` lines, and the test each one marks: the function declared on
/// the next line. Only a line that is exactly `IGNORED`, after indentation, is dropped: an
/// `#[ignore]` with any other reason is a difference like any other.
fn without_ignored(body: &str) -> (String, Vec<String>) {
    let mut kept = String::with_capacity(body.len());
    let mut marked = Vec::new();
    let mut lines = body.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim() == IGNORED {
            marked.push(declared_fn(lines.peek().copied().unwrap_or_default()));
        } else {
            kept.push_str(line);
        }
    }
    (kept, marked)
}

/// The name of the function `line` declares, or the whole trimmed line when it declares none,
/// so a weekly-tier line above anything but a test shows up in the report.
fn declared_fn(line: &str) -> String {
    let line = line.trim();
    line.strip_prefix("fn ")
        .and_then(|rest| rest.split_once('('))
        .map_or_else(|| line.to_string(), |(name, _)| name.to_string())
}

/// `copy` is `original` below its header, apart from the `IGNORED` lines, and those mark
/// exactly the tests named in `weekly`, in file order: moving the line to another test is a
/// difference, not only adding or dropping one.
#[track_caller]
fn assert_copy(name: &str, copy: &str, original: &str, weekly: &[&str]) {
    let body = body(copy).unwrap_or_else(|why| panic!("{name}: {why}"));
    let (body, marked) = without_ignored(body);
    assert_eq!(marked, weekly, "{name}: the tests moved to the weekly tier");
    if let Some(difference) = first_difference(&body, original) {
        panic!("{name} is not the vendored file: {difference}");
    }
}

/// The tests each copy moves to the weekly tier, in file order.
const WEEKLY_LIB: &[&str] = &["deny_inherited_fds_reports_a_missing_program_as_not_found"];
const WEEKLY_BENEATH: &[&str] = &[
    "in_scope_write_append_read_roundtrip",
    "symlink_component_escape_is_refused",
    "symlink_final_component_is_refused",
    "symlink_intermediate_in_root_is_refused",
    "dotdot_and_absolute_rel_are_refused",
    "empty_rel_opens_the_root_for_read_and_refuses_write",
    "ancestor_symlink_swap_at_acquisition_is_refused",
    "ancestor_rename_after_acquisition_does_not_redirect",
    "root_replacement_after_acquisition_fails_closed",
    "from_owned_fd_refuses_a_non_directory",
    "from_owned_fd_adopts_a_directory_without_pathname_authority",
    "o_nofollow_alone_opens_through_an_intermediate_symlink",
    "enotdir_classifies_as_a_resolution_refusal",
    "a_grant_includes_nested_mounts_and_no_xdev_would_refuse_them",
    "identity_is_stable_and_shared_by_clones",
];
const WEEKLY_SWEEP: &[&str] = &[
    "the_enumeration_leg_sees_a_descriptor_high_in_the_table",
    "a_refused_plan_fails_the_spawn_and_the_program_never_runs",
    "a_truncated_sweep_bound_leaks_a_descriptor_the_honest_bound_closes",
    "a_required_range_wider_than_the_supported_range_refuses_the_spawn",
    "a_lowered_allocation_limit_never_shrinks_the_bound_below_the_open_table",
    "the_planned_bound_tracks_the_kernel_ceiling_rather_than_a_constant",
];
const WEEKLY_HOSTILE_FDS: &[&str] = &[
    "an_ambient_file_descriptor_is_closed_in_the_confined_child",
    "an_ambient_socket_descriptor_is_closed_in_the_confined_child",
    "an_ambient_pipe_descriptor_is_closed_in_the_confined_child",
    "a_duplicated_descriptor_is_closed_in_the_confined_child",
    "a_descriptor_near_the_top_of_the_table_is_closed_in_the_confined_child",
    "the_kernel_refuses_a_descriptor_at_or_above_the_derived_ceiling",
    "delegated_stdio_survives_the_confined_spawn",
    "concurrent_descriptor_creation_never_reaches_the_confined_child",
    "a_missing_confined_program_still_reports_not_found",
];

#[test]
fn beneath_is_the_vendored_file() {
    assert!(VENDORED_BENEATH.contains("pub struct GrantedRoot"));
    assert_copy(
        "src/beneath.rs",
        COPY_BENEATH,
        VENDORED_BENEATH,
        WEEKLY_BENEATH,
    );
}

#[test]
fn sweep_is_the_vendored_file() {
    assert!(VENDORED_SWEEP.contains("pub(crate) fn plan_sweep_bound("));
    assert_copy("src/sweep.rs", COPY_SWEEP, VENDORED_SWEEP, WEEKLY_SWEEP);
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
    assert_copy("src/lib.rs", COPY_LIB, &published, WEEKLY_LIB);
    assert!(!body(COPY_LIB).unwrap().contains("duplicate_control_socket"));
}

#[test]
fn hostile_fds_is_the_vendored_file_with_this_crates_name() {
    // Besides the weekly-tier lines, the one edit is the three calls. A vendored change that
    // names the crate a fourth time fails here, so it is a decision, not a silent rewrite.
    let old = "agent_bridle_fdguard";
    assert_eq!(VENDORED_HOSTILE_FDS.matches(old).count(), 3);
    let call = "agent_bridle_fdguard::deny_inherited_fds(&mut cmd);";
    assert_eq!(VENDORED_HOSTILE_FDS.matches(call).count(), 3);
    let renamed = VENDORED_HOSTILE_FDS.replace(old, "newt_fdguard");
    assert_copy(
        "tests/hostile_fds.rs",
        COPY_HOSTILE_FDS,
        &renamed,
        WEEKLY_HOSTILE_FDS,
    );
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
/// one or one that hides code in its header cannot pass. An inner attribute is code: in a
/// header, `#![cfg(any())]` would empty a test binary, `#![allow(..)]` would lift a lint, and a
/// `//!` block would add a doctest.
#[test]
fn a_copy_without_a_header_is_refused() {
    assert!(body("").is_err());
    assert!(body(VENDORED_BENEATH).is_err());
    let hidden = format!("fn above() {{}}\n{SENTINEL} ----\n{VENDORED_BENEATH}");
    assert!(body(&hidden).is_err());
    for attribute in [
        "#![cfg(any())]",
        "#![allow(unsafe_op_in_unsafe_fn)]",
        "//! ```",
        "/// a doc line",
    ] {
        let hidden = format!("// a header\n{attribute}\n{SENTINEL} ----\n{VENDORED_BENEATH}");
        assert!(
            body(&hidden).is_err(),
            "{attribute} passed as a header line"
        );
    }
    let honest = format!("// a header\n{SENTINEL} ----\n{VENDORED_BENEATH}");
    assert_eq!(body(&honest), Ok(VENDORED_BENEATH));
}

/// Only the exact weekly-tier line is dropped, and each one names the test it marks: an
/// `#[ignore]` with another reason stays a difference, the same line on another test names
/// that test, and a line above anything but a function names the line.
#[test]
fn only_the_weekly_tier_line_is_dropped() {
    let original = "#[test]\nfn a() {}\n#[test]\nfn b() {}\n";
    let on_a = format!("#[test]\n    {IGNORED}\nfn a() {{}}\n#[test]\nfn b() {{}}\n");
    assert_eq!(
        without_ignored(&on_a),
        (original.to_string(), vec!["a".to_string()])
    );
    let on_b = format!("#[test]\nfn a() {{}}\n#[test]\n    {IGNORED}\nfn b() {{}}\n");
    assert_eq!(
        without_ignored(&on_b),
        (original.to_string(), vec!["b".to_string()])
    );
    let other = "#[test]\n#[ignore = \"flaky\"]\nfn a() {}\n#[test]\nfn b() {}\n";
    let (kept, marked) = without_ignored(other);
    assert!(marked.is_empty());
    assert!(first_difference(&kept, original).is_some());
    let twice = format!("{IGNORED}\n{IGNORED}\nfn a() {{}}\n");
    assert_eq!(without_ignored(&twice).1, [IGNORED, "a"]);
}

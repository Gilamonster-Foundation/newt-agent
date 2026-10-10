use super::*;

/// PR #2845: becoming ignored must not turn a still-present file into a deletion.
#[test]
fn observed_report_newly_ignored_file_is_read_not_reported_deleted() {
    let dir = fixture();
    std::fs::write(dir.path().join("note.rs"), "one\ntwo\n").unwrap();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    std::fs::write(dir.path().join(".gitignore"), "note.rs\n").unwrap();
    std::fs::write(dir.path().join("note.rs"), "one\ntwo\nthree\n").unwrap();
    let text = state.render(dir.path(), &Scope::All, "done");
    assert!(
        text.contains("`note.rs`: 2 → 3 (present, not enumerated (ignored/untracked transition))"),
        "{text}"
    );
}

/// PR #2845: a newly enumerated file has no historical baseline measurement;
/// neither zero nor its current count may be invented as the before count.
#[test]
fn observed_report_newly_unignored_file_has_unverified_baseline() {
    let dir = fixture();
    std::fs::write(dir.path().join(".gitignore"), "note.rs\n").unwrap();
    std::fs::write(dir.path().join("note.rs"), "one\ntwo\n").unwrap();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    std::fs::write(dir.path().join(".gitignore"), "").unwrap();
    std::fs::write(dir.path().join("note.rs"), "one\ntwo\nthree\n").unwrap();
    let text = state.render(dir.path(), &Scope::All, "done");
    assert!(
        text.contains("`note.rs`: unverified (not enumerated at baseline) → 3"),
        "{text}"
    );
}

/// PR #2845: a vanished untracked file still has a genuine absent postimage.
#[test]
fn observed_report_missing_untracked_file_is_verified_absent() {
    let dir = fixture();
    std::fs::write(dir.path().join("note.rs"), "one\ntwo\n").unwrap();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    std::fs::remove_file(dir.path().join("note.rs")).unwrap();
    let text = state.render(dir.path(), &Scope::All, "done");
    assert!(text.contains("`note.rs`: 2 → 0 (absent)"), "{text}");
}

/// PR #2845: enumeration changes cannot bypass the file-read scope.
#[test]
fn observed_report_missing_entry_without_read_authority_is_unverified() {
    let dir = fixture();
    std::fs::write(dir.path().join("note.rs"), "one\ntwo\n").unwrap();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    std::fs::write(dir.path().join(".gitignore"), "note.rs\n").unwrap();
    let scope = Scope::Only(
        [dir.path().join(".git").to_string_lossy().into_owned()]
            .into_iter()
            .collect(),
    );
    let text = state.render(dir.path(), &scope, "done");
    assert!(text.contains("File counts unavailable"), "{text}");
    assert!(!text.contains("`note.rs`: 2 → 0 (absent)"), "{text}");
}

/// PR #2845: an observed absent tracked leaf can legitimately start at zero.
#[test]
fn observed_report_creation_after_verified_absence_starts_at_zero() {
    let dir = fixture();
    std::fs::remove_file(dir.path().join("src/mod.rs")).unwrap();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    std::fs::write(dir.path().join("src/mod.rs"), "created\n").unwrap();
    let text = state.render(dir.path(), &Scope::All, "done");
    assert!(text.contains("`src/mod.rs`: 0 (absent) → 1"), "{text}");
}

/// PR #2845: force-adding an ignored file cannot manufacture historical absence.
#[test]
fn observed_report_force_added_ignored_file_has_unverified_baseline() {
    let dir = fixture();
    std::fs::write(dir.path().join(".gitignore"), "note.rs\n").unwrap();
    std::fs::write(dir.path().join("note.rs"), "before\n").unwrap();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    std::fs::write(dir.path().join("note.rs"), "after\nchanged\n").unwrap();
    git(dir.path(), &["add", "-f", "note.rs"]);
    let text = state.render(dir.path(), &Scope::All, "done");
    assert!(
        text.contains("`note.rs`: unverified (not enumerated at baseline) → 2"),
        "{text}"
    );
}

/// PR #2845: a dangling symlink must not be mistaken for a missing file.
#[cfg(unix)]
#[test]
fn observed_report_ignored_dangling_symlink_is_unverified() {
    let dir = fixture();
    let path = dir.path().join("note.rs");
    std::fs::write(&path, "before\n").unwrap();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    std::fs::write(dir.path().join(".gitignore"), "note.rs\n").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("missing-target", &path).unwrap();
    let text = state.render(dir.path(), &Scope::All, "done");
    assert!(text.contains("`note.rs`: 1 → unverified"), "{text}");
}

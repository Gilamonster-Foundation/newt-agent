use super::*;

#[test]
fn file_changes_survive_a_cancel_so_the_caller_still_refreshes() {
    // An add/remove already happened on disk; Esc must still hand the
    // caller the change notes (its cue to re-resolve config), while
    // applying nothing.
    let mut s = panel();
    let mut remove = ok_remove();
    s.begin_command("d gpu-runner");
    s.run_command(&mut remove);
    let close = close_outcome(false, &s);
    assert_eq!(close.apply, None);
    assert_eq!(close.changes, vec!["removed backend 'gpu-runner'"]);
}

/// §4: a drop-in that shares its name with an inline `[[backends]]` entry
/// says so on save — the merge re-inherits whatever the drop-in omits, so
/// "I cleared the api-key" is not the whole truth.
#[test]
fn the_save_note_flags_a_same_named_inline_entry() {
    let path = std::path::Path::new("/home/x/.newt/backends/dgx1.toml");
    let plain = saved_note("dgx1", path, false);
    assert_eq!(
        plain,
        "saved backend 'dgx1' → /home/x/.newt/backends/dgx1.toml"
    );
    let shared = saved_note("dgx1", path, true);
    assert!(shared.starts_with(&plain), "keeps the plain summary");
    assert!(shared.contains("[[backends]]") && shared.contains("re-inherited"));
    // …and the chooser row marks the same trap.
    assert_eq!(
        BackendSource::UserDropInOverInline.provenance(),
        "drop-in + inline entry"
    );
    assert!(BackendSource::UserDropInOverInline.editable());
}

/// §5/§12 REGRESSION: a mid-panel terminal I/O failure must still hand the
/// caller the file operations that ALREADY committed — dropping them left
/// the session reporting nothing and running against a config it never
/// re-resolved, even though a drop-in had been deleted.
#[test]
fn an_io_error_still_carries_the_committed_file_changes() {
    let mut s = panel();
    let mut remove = ok_remove();
    s.begin_command("d gpu-runner");
    s.run_command(&mut remove); // the delete COMMITTED in-loop
    let err = finish(Err(io::Error::other("terminal detached")), true, &s)
        .expect_err("an io error is still an error");
    assert!(err.error.to_string().contains("terminal detached"));
    assert_eq!(
        err.close.changes,
        vec!["removed backend 'gpu-runner'"],
        "the committed change survives for the caller to report + re-resolve"
    );
    assert_eq!(err.close.apply, None, "an aborted panel applies nothing");
    // A failure BEFORE the loop owns state carries nothing.
    let early: PanelRunError = io::Error::other("no raw mode").into();
    assert_eq!(early.close, PanelClose::cancelled());
    // The clean path is unchanged.
    assert_eq!(
        finish(Ok(()), false, &s).unwrap().changes,
        vec!["removed backend 'gpu-runner'"]
    );
}

use agent_harness::{Session, SessionConfig};
use serde_json::json;

fn advance(session: &mut Session, text: &str) {
    session
        .record_messages(&[json!({"role":"user","content":text})])
        .unwrap();
}

/// Grounds checkpoint selection in a real closed/reopened store: valid old
/// addressed history must not displace a newer committed current head.
#[test]
fn stale_restore_cannot_replace_a_closed_writers_current_head() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = Session::open(dir.path(), SessionConfig::default()).unwrap();
    advance(&mut first, "first observation");
    let old = first.head();
    advance(&mut first, "later observation");
    let current = first.head();
    let locator = first.checkpoint_path().unwrap();
    drop(first);

    let restored = Session::restore(dir.path(), old, "local-session");
    assert_eq!(
        std::fs::read_to_string(&locator).unwrap().trim(),
        current.to_string()
    );
    assert!(
        matches!(restored, Err(agent_harness::Error::Conflict(_))),
        "stale history must require an explicit fork or current head"
    );
    let resumed = Session::restore(dir.path(), current, "local-session").unwrap();
    assert_eq!(
        resumed.restored_messages().unwrap()[0]["content"],
        "later observation"
    );
}

/// Grounds exclusive execution ownership in separately opened store handles,
/// rather than relying on a TUI mutex or shared Rust object.
#[test]
fn an_independent_writer_cannot_resume_a_live_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Session::open(dir.path(), SessionConfig::default()).unwrap();
    advance(&mut owner, "owned observation");
    let head = owner.head();
    let locator = owner.checkpoint_path().unwrap();

    let competitor = Session::restore(dir.path(), head, "local-session");
    assert_eq!(
        std::fs::read_to_string(&locator).unwrap().trim(),
        head.to_string()
    );
    assert!(
        matches!(competitor, Err(agent_harness::Error::Conflict(_))),
        "a live owner must exclude another writer"
    );
    drop(owner);
    assert!(Session::restore(dir.path(), head, "local-session").is_ok());
}

#[test]
fn separate_runs_can_write_in_the_same_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let mut second = Session::open(dir.path(), SessionConfig::default()).unwrap();
    assert_ne!(first.run_id(), second.run_id());
    advance(&mut first, "first run");
    advance(&mut second, "second run");
    for session in [&first, &second] {
        assert_eq!(
            std::fs::read_to_string(session.checkpoint_path().unwrap())
                .unwrap()
                .trim(),
            session.head().to_string()
        );
    }
}

/// Grounds expected-predecessor validation in a real altered locator. A writer
/// must stop even while it still holds the run lock, and cannot revive itself
/// merely because an external repair later restores its expected head.
#[test]
fn a_changed_locator_stops_the_owner_without_replacing_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let old = session.head();
    advance(&mut session, "committed observation");
    let head = session.head();
    let locator = session.checkpoint_path().unwrap();
    std::fs::write(&locator, format!("{old}\n")).unwrap();
    let result = session.record_messages(&[json!({"role":"user","content":"must not commit"})]);
    assert!(matches!(result, Err(agent_harness::Error::Conflict(_))));
    assert_eq!(session.head(), head);
    assert_eq!(
        std::fs::read_to_string(&locator).unwrap().trim(),
        old.to_string()
    );
    std::fs::write(&locator, format!("{head}\n")).unwrap();
    assert!(session.ensure_writer().is_err());
    drop(session);
    let restored = Session::restore(dir.path(), head, "local-session").unwrap();
    assert_eq!(
        restored.restored_messages().unwrap()[0]["content"],
        "committed observation"
    );
}

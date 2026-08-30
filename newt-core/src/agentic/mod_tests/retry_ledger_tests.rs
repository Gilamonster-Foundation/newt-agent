use super::*;

#[test]
fn lexical_normalize_collapses_dot_and_parent() {
    assert_eq!(
        lexical_normalize(std::path::Path::new("/ws/examples/../foo.py")),
        std::path::PathBuf::from("/ws/foo.py")
    );
    assert_eq!(
        lexical_normalize(std::path::Path::new("/ws/./a//b/foo.py")),
        std::path::PathBuf::from("/ws/a/b/foo.py")
    );
    // A leading `..` with no segment to pop is preserved, not climbed past root.
    assert_eq!(
        lexical_normalize(std::path::Path::new("../x.py")),
        std::path::PathBuf::from("../x.py")
    );
}

#[test]
fn ledger_note_write_keys_on_the_normalized_path() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("foo.py"), "real\n").unwrap();
    let led = std::cell::RefCell::new(crate::verify_gate::WriteLedger::new());
    // a raw, non-normalized model path
    let args = serde_json::json!({ "path": "examples/../foo.py" });
    ledger_note_write(
        Some(&led),
        "write_file",
        &args,
        tmp.path().to_str().unwrap(),
    );
    // the key normalized to <ws>/foo.py — the same path the gate would produce —
    // so revert finds and restores it (returns true).
    assert!(
        led.borrow().revert(&tmp.path().join("foo.py")).unwrap(),
        "the normalized key matches the gate's path"
    );
    // a read-only tool is never recorded
    ledger_note_write(
        Some(&led),
        "read_file",
        &serde_json::json!({ "path": "foo.py" }),
        tmp.path().to_str().unwrap(),
    );
    assert_eq!(led.borrow().len(), 1, "only write tools are tracked");
}

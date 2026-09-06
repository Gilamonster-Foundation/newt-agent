use super::*;

#[test]
fn history_step_walks_older_then_back_to_the_fresh_line() {
    // Three entries; pos 3 == the fresh line.
    let len = 3;
    // ↑ from fresh walks back through 2,1,0 then stops.
    assert_eq!(history_step(3, len, true), Some(2));
    assert_eq!(history_step(2, len, true), Some(1));
    assert_eq!(history_step(1, len, true), Some(0));
    assert_eq!(history_step(0, len, true), None, "oldest: nowhere up");
    // ↓ walks forward and back onto the fresh line, then stops.
    assert_eq!(history_step(0, len, false), Some(1));
    assert_eq!(history_step(2, len, false), Some(3));
    assert_eq!(history_step(3, len, false), None, "fresh: nowhere down");
    // Empty history never moves.
    assert_eq!(history_step(0, 0, true), None);
    assert_eq!(history_step(0, 0, false), None);
}

#[serial_test::serial(real_fs)]
#[test]
fn load_history_reads_nonblank_lines_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history");
    std::fs::write(&path, "first\n\nsecond\n  \nthird\n").unwrap();
    assert_eq!(
        load_history(Some(&path)),
        vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string()
        ]
    );
    // Missing file / no path → empty, never an error.
    assert!(load_history(Some(&dir.path().join("nope"))).is_empty());
    assert!(load_history(None).is_empty());
}

#[test]
fn textarea_with_prefills_content_for_recall() {
    let ta = textarea_with(Edit::Vi, "recalled prompt");
    assert_eq!(ta.lines(), &["recalled prompt".to_string()]);
}

#[serial_test::serial(real_fs)]
#[test]
fn history_appends_unsaved_entries_to_file() {
    let dir = tempfile::tempdir().unwrap();
    let hp = dir.path().join("history");
    let mut s = RichSurface::new(Some(hp.clone())).unwrap();
    s.add_history("alpha");
    s.add_history("multi\nline");
    s.save_history();
    let contents = std::fs::read_to_string(&hp).unwrap();
    assert!(contents.contains("alpha"));
    assert!(
        contents.contains("multi line"),
        "newlines flattened to keep one entry per line"
    );
    // Second save with nothing new is a no-op (no duplicate append).
    s.save_history();
    assert_eq!(std::fs::read_to_string(&hp).unwrap(), contents);
}

#[test]
fn history_without_path_is_a_noop() {
    let mut s = RichSurface::new(None).unwrap();
    s.add_history("ephemeral");
    s.save_history(); // must not panic
}

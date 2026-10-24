use super::*;

// #144: the pure config edit and the locked writer behind `propose_config_edit`.

fn edit(key: &str, value: serde_json::Value, append: bool) -> ConfigEdit {
    ConfigEdit {
        key: key.to_string(),
        value,
        append,
    }
}

/// The issue's concrete case: widen `[shell] env_passthrough` so `BROWSER`
/// reaches commands. Comments, unmodeled sections and the existing entries
/// survive, and a second append of the same name is a no-op.
#[test]
fn with_config_edit_appends_env_passthrough_preserving_comments() {
    let original = "# operator config\n[shell]\nenv_passthrough = [\"PATH\", \"HOME\"] # keep these\n\n[unmodeled]\nretain = true\n";
    let browser = edit(
        "shell.env_passthrough",
        serde_json::json!(["BROWSER", "PATH"]),
        true,
    );
    let updated = Config::with_config_edit(original, &browser).unwrap();
    assert!(updated.contains("# operator config"));
    assert!(updated.contains("# keep these"));
    assert!(updated.contains("[unmodeled]"));
    let parsed: Config = toml::from_str(&updated).unwrap();
    assert_eq!(
        parsed.shell.unwrap().env_passthrough.unwrap(),
        vec!["PATH", "HOME", "BROWSER"]
    );
    assert_eq!(
        Config::with_config_edit(&updated, &browser).unwrap(),
        updated
    );

    // A missing table and key are created; a scalar append is one entry.
    let created = Config::with_config_edit(
        "",
        &edit("shell.env_passthrough", serde_json::json!("BROWSER"), true),
    )
    .unwrap();
    let parsed: Config = toml::from_str(&created).unwrap();
    assert_eq!(
        parsed.shell.unwrap().env_passthrough.unwrap(),
        vec!["BROWSER"]
    );
}

/// The top-level `Config` accepts unknown keys, so the edit must refuse them
/// itself: a typo would otherwise be written and silently ignored.
#[test]
fn with_config_edit_rejects_unknown_key() {
    let original = "[shell]\nenv_passthrough = [\"PATH\"]\n";
    for (key, value) in [
        ("shell.env_passthru", serde_json::json!(["BROWSER"])),
        ("not_a_section.key", serde_json::json!(true)),
        // A known key with the wrong type is refused too.
        ("shell.env_passthrough", serde_json::json!(7)),
        // Replacing a whole table is not an edit to one key.
        ("shell", serde_json::json!("x")),
        ("shell..env_passthrough", serde_json::json!("x")),
        ("", serde_json::json!("x")),
    ] {
        assert!(
            Config::with_config_edit(original, &edit(key, value, false)).is_err(),
            "{key} must be refused"
        );
    }
    assert!(Config::with_config_edit(
        original,
        &edit("shell.env_passthrough", serde_json::json!({"a": 1}), true)
    )
    .is_err());
}

/// The writer takes `config.toml.lock`, and refuses when the file no longer
/// reads as the text whose diff the operator approved. A refused write leaves
/// the bytes alone and takes no backup.
#[test]
fn apply_config_edit_holds_lock_and_refuses_stale_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let shown = "[shell]\nenv_passthrough = [\"PATH\"]\n";
    let changed = "[shell]\nenv_passthrough = [\"PATH\", \"HOME\"]\n";
    std::fs::write(&path, changed).unwrap();
    let browser = edit(
        "shell.env_passthrough",
        serde_json::json!(["BROWSER"]),
        true,
    );

    crate::atomic_fs::reset_lock_attempts_for_test();
    assert!(Config::apply_config_edit(&path, shown, &browser).is_err());
    assert!(crate::atomic_fs::lock_attempts_for_test() > 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
    assert!(!backup_path(&path).exists());

    let written = Config::apply_config_edit(&path, changed, &browser).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    assert_eq!(
        std::fs::read_to_string(backup_path(&path)).unwrap(),
        changed
    );
}

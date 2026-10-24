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

/// PR #3004 review: `skip_serializing_if` drops a known key set to its empty
/// value, so a serialize round trip alone mistook a reset for an unknown key.
#[test]
fn with_config_edit_accepts_reset_to_empty() {
    let original = "[skills]\nsearch = [\"custom\"]\nbundled_dir = \"/opt/skills\"\n";
    let reset = Config::with_config_edit(
        original,
        &edit("skills.search", serde_json::json!([]), false),
    )
    .unwrap();
    let reset = Config::with_config_edit(
        &reset,
        &edit("skills.bundled_dir", serde_json::json!(""), false),
    )
    .unwrap();
    let skills = toml::from_str::<Config>(&reset).unwrap().skills.unwrap();
    assert!(skills.search.is_empty());
    assert!(skills.bundled_dir.is_empty());
}

/// PR #3004 review: append compared TOML spelling, so `['PATH']` plus "PATH"
/// duplicated the entry. An already-present value leaves the text untouched.
#[test]
fn with_config_edit_append_dedupes_by_value_not_spelling() {
    for original in [
        "[shell]\nenv_passthrough = ['PATH'] # literal\n",
        "[shell]\nenv_passthrough = [\"PATH\"]\n",
        "[shell]\nenv_passthrough = [\"\\u0050ATH\"]\n",
    ] {
        let same = Config::with_config_edit(
            original,
            &edit("shell.env_passthrough", serde_json::json!(["PATH"]), true),
        )
        .unwrap();
        assert_eq!(same, original);
    }
}

/// PR #3004 review: the backup must replace a `.bak` symlink or hard link as a
/// directory entry, never write through it into an unrelated file.
#[cfg(unix)]
#[test]
fn update_config_file_replaces_backup_links_without_touching_their_targets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let unrelated = dir.path().join("unrelated.txt");
    for link in [
        |target: &std::path::Path, bak: &std::path::Path| {
            std::os::unix::fs::symlink(target, bak).unwrap();
        },
        |target: &std::path::Path, bak: &std::path::Path| {
            std::fs::hard_link(target, bak).unwrap();
        },
    ] {
        std::fs::write(&path, "model = \"old\"\n").unwrap();
        std::fs::write(&unrelated, "marker").unwrap();
        let _ = std::fs::remove_file(backup_path(&path));
        link(&unrelated, &backup_path(&path));
        Config::update_config_file(&path, |_| Ok("model = \"new\"\n".into())).unwrap();
        assert_eq!(std::fs::read_to_string(&unrelated).unwrap(), "marker");
        assert!(!std::fs::symlink_metadata(backup_path(&path))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            "model = \"old\"\n"
        );
    }
}

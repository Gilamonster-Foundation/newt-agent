use super::*;

// Skill search and bundled defaults, plus the adjacent persona discovery path.

/// Serial: reads `user_config_dir()`, which honors NEWT_CONFIG_DIR — a
/// parallel serial-lane test pinning that var to a tempdir makes the
/// `.newt` parent assertion observe the tempdir instead (caught by the
/// slower Windows CI runner).
#[serial_test::serial(real_fs)]
#[test]
fn skill_search_dirs_defaults_to_single_newt_dir() {
    let cfg = Config::default();
    let dirs = cfg.skill_search_dirs();
    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].ends_with("skills"));
    // The parent component is `.newt`.
    assert_eq!(
        dirs[0].parent().and_then(|p| p.file_name()),
        Some(".newt".as_ref())
    );
}

/// #1021 PR 5.2: `personas_dir()` is the sibling-of-config default
/// `PersonaStore::default_dir()` (newt-tui) also resolves to — a headless
/// caller gets the exact same location without depending on newt-tui.
#[serial_test::serial(real_fs)] // same NEWT_CONFIG_DIR-reader race as above
#[test]
fn personas_dir_is_a_sibling_of_the_newt_config_dir() {
    let dir = Config::personas_dir();
    assert!(dir.ends_with("personas"));
    assert_eq!(
        dir.parent().and_then(|p| p.file_name()),
        Some(".newt".as_ref())
    );
}

#[test]
fn skill_search_dirs_preserves_configured_order() {
    let cfg = Config {
        skills: Some(SkillsConfig {
            search: vec!["/abs/one".into(), "/abs/two".into()],
            bundled_dir: String::new(),
        }),
        ..Config::default()
    };
    assert_eq!(
        cfg.skill_search_dirs(),
        vec![PathBuf::from("/abs/one"), PathBuf::from("/abs/two")]
    );
}

#[test]
fn skill_search_dirs_expands_tilde() {
    let cfg = Config {
        skills: Some(SkillsConfig {
            search: vec!["~/skills-x".into()],
            bundled_dir: String::new(),
        }),
        ..Config::default()
    };
    let dirs = cfg.skill_search_dirs();
    // The final component survives expansion regardless of whether $HOME
    // was set; when set, the leading `~` must be gone.
    assert!(dirs[0].ends_with("skills-x"));
    assert!(!dirs[0].starts_with("~"));
}

#[test]
fn skill_search_dirs_appends_bundled_dir_last() {
    // Bundled dir is LOWEST priority: user `search` paths come first so a
    // user skill of the same name wins the collision (earlier dirs win in
    // `discover_paths`), and the bundled dir is appended last.
    let cfg = Config {
        skills: Some(SkillsConfig {
            search: vec!["/abs/user".into()],
            bundled_dir: "/abs/bundled".into(),
        }),
        ..Config::default()
    };
    assert_eq!(
        cfg.skill_search_dirs(),
        vec![PathBuf::from("/abs/user"), PathBuf::from("/abs/bundled")],
        "user search dirs must precede the bundled dir so users can override"
    );
}

#[test]
fn skill_search_dirs_bundled_after_default_when_search_empty() {
    // No `search` configured: the host default (`~/.newt/skills`) still
    // precedes the bundled dir. An empty `bundled_dir` adds nothing.
    let with_bundled = Config {
        skills: Some(SkillsConfig {
            search: vec![],
            bundled_dir: "/abs/bundled".into(),
        }),
        ..Config::default()
    };
    let dirs = with_bundled.skill_search_dirs();
    assert_eq!(dirs.len(), 2, "default host dir + bundled: {dirs:?}");
    assert!(
        dirs[0].ends_with("skills"),
        "default host dir first: {dirs:?}"
    );
    assert_eq!(
        dirs[1],
        PathBuf::from("/abs/bundled"),
        "bundled last: {dirs:?}"
    );

    let no_bundled = Config {
        skills: Some(SkillsConfig {
            search: vec![],
            bundled_dir: String::new(),
        }),
        ..Config::default()
    };
    assert_eq!(
        no_bundled.skill_search_dirs().len(),
        1,
        "empty bundled_dir contributes no directory"
    );
}

#[test]
fn with_bundled_default_leaves_a_configured_value_untouched() {
    // A user who set `bundled_dir` must win — the checkout default only
    // fills the gap, it never overrides an explicit choice.
    let cfg = Config {
        skills: Some(SkillsConfig {
            search: vec![],
            bundled_dir: "/explicit/bundled".into(),
        }),
        ..Config::default()
    }
    .with_bundled_default();
    assert_eq!(
        cfg.skills.unwrap().bundled_dir,
        "/explicit/bundled",
        "an explicitly configured bundled_dir is never overridden"
    );
}

#[test]
fn skills_search_round_trips_through_toml() {
    let cfg = Config {
        skills: Some(SkillsConfig {
            search: vec!["~/.newt/skills".into(), "~/.claude/skills".into()],
            bundled_dir: String::new(),
        }),
        ..Config::default()
    };
    let text = toml::to_string_pretty(&cfg).unwrap();
    let back: Config = toml::from_str(&text).unwrap();
    assert_eq!(
        back.skills.unwrap().search,
        vec!["~/.newt/skills".to_string(), "~/.claude/skills".to_string()]
    );
}

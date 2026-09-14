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
    let dirs = cfg.skill_search_dirs_with(None, |_| false);
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
        cfg.skill_search_dirs_with(None, |_| false),
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
    let dirs = cfg.skill_search_dirs_with(None, |_| false);
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
        cfg.skill_search_dirs_with(None, |_| false),
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
    let dirs = with_bundled.skill_search_dirs_with(None, |_| false);
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
        no_bundled.skill_search_dirs_with(None, |_| false).len(),
        1,
        "empty bundled_dir contributes no directory"
    );
}

/// #2331: with no `bundled_dir` configured, a newt checkout's
/// `.newt/bundled-skills` above the cwd is the bundled default, appended last.
/// This is the list the index AND the `use_skill` loader both read now.
#[test]
fn skill_search_dirs_append_the_checkout_bundled_default_when_unset() {
    let checkout = PathBuf::from("/home/u/repo/.newt/bundled-skills");
    let cfg = Config {
        skills: Some(SkillsConfig {
            search: vec!["/abs/user".into()],
            bundled_dir: String::new(),
        }),
        ..Config::default()
    };
    assert_eq!(
        cfg.skill_search_dirs_with(Some(Path::new("/home/u/repo/newt-core")), |p| p == checkout),
        vec![PathBuf::from("/abs/user"), checkout.clone()],
        "the checkout default is the LOWEST priority entry"
    );
    // Twin: outside a checkout there is no bundled default to append.
    assert_eq!(
        cfg.skill_search_dirs_with(Some(Path::new("/elsewhere")), |p| p == checkout),
        vec![PathBuf::from("/abs/user")]
    );
}

/// A user who set `bundled_dir` wins — the checkout default only fills the
/// gap, it never overrides an explicit choice.
#[test]
fn a_configured_bundled_dir_beats_the_checkout_default() {
    let cfg = Config {
        skills: Some(SkillsConfig {
            search: vec![],
            bundled_dir: "/explicit/bundled".into(),
        }),
        ..Config::default()
    };
    let dirs = cfg.skill_search_dirs_with(Some(Path::new("/home/u/repo")), |_| true);
    assert_eq!(dirs.last(), Some(&PathBuf::from("/explicit/bundled")));
    assert_eq!(
        dirs.len(),
        2,
        "host default + the explicit bundled dir: {dirs:?}"
    );
}

/// #2331: installs and seeding land in the first configured root, never in
/// the bundled directory, whatever the probe finds.
#[test]
fn skill_install_dir_is_the_first_configured_root() {
    let cfg = Config {
        skills: Some(SkillsConfig {
            search: vec!["/abs/first".into(), "/abs/second".into()],
            bundled_dir: "/abs/bundled".into(),
        }),
        ..Config::default()
    };
    assert_eq!(cfg.skill_install_dir(), Some(PathBuf::from("/abs/first")));
    assert_eq!(
        cfg.skill_search_dirs_with(None, |_| false).first(),
        cfg.skill_install_dir().as_ref(),
        "the install dir is the search path's highest-priority entry"
    );
}

/// The config-root leak regression (field-caught, moved here from
/// `newt_skills::default_skills_dir` with #2331): a redirected
/// `$NEWT_CONFIG_DIR` owns the default install dir — seeding once wrote
/// `~/.newt/skills` on the real home even with the root redirected.
#[serial_test::serial(real_fs)]
#[test]
fn skill_install_dir_honors_newt_config_dir() {
    let saved = std::env::var_os(NEWT_CONFIG_DIR_ENV);
    std::env::set_var(NEWT_CONFIG_DIR_ENV, "/tmp/redirected-root");
    let dir = Config::default().skill_install_dir();
    match saved {
        Some(v) => std::env::set_var(NEWT_CONFIG_DIR_ENV, v),
        None => std::env::remove_var(NEWT_CONFIG_DIR_ENV),
    }
    assert_eq!(dir, Some(PathBuf::from("/tmp/redirected-root/skills")));
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

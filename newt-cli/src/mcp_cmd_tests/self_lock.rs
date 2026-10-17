//! `newt mcp import` must not re-acquire the config locks it already holds
//! while consulting configs (namespace sanitization + grant checks).
//!
//! Regression for #2911: after the transaction locks were taken, the import
//! still consulted configs through `Config::load` / `Config::resolve`, which
//! tried to re-acquire the same lock — burning the ~100-retry contention
//! budget. The consults now go through `load_with_held_locks`, so no lock
//! attempt is made against a config the transaction already holds.

use super::*;

/// Count `acquire_lock` attempts around the consult path. Import takes two
/// transaction locks (`mcp.toml` + `config.toml`) then must NOT attempt to
/// re-acquire either one while resolving namespace sanitization or the net
/// grant.
#[test]
#[serial_test::serial]
fn import_consults_configs_without_reacquiring_held_locks() {
    // A real config with an inline backend so resolve() does not synthesize
    // the localhost fallback and actually read the file.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // Point NEWT_CONFIG_DIR at our temp dir so user_config_dir() resolves here.
    let _home = std::env::set_var("NEWT_CONFIG_DIR", home);

    std::fs::create_dir_all(home.join("backends")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[[backends]]\nname = \"test\"\nendpoint = \"http://127.0.0.1:11434\"\n",
    )
    .unwrap();

    newt_core::atomic_fs::reset_lock_attempts_for_test();
    newt_core::psyche_import::reset_lock_contention_warned_for_test();

    // A minimal source file.
    let source = dir.path().join("source.json");
    std::fs::write(
        &source,
        "{\n  \"mcpServers\": {\n    \"testsrc\": {\n \
         \"command\": \"test-cmd\"\n        }\n  }\n}\n",
    )
    .unwrap();

    // Reset the counter right before the import runs, so we only count the
    // attempts made *after* the transaction locks are taken.
    newt_core::atomic_fs::reset_lock_attempts_for_test();

    // The import should succeed and the consult path must not try to acquire
    // any lock the transaction already holds. With the bug present, the
    // namespace-sanitization resolve() re-acquires config.toml, and after the
    // 100-retry budget it warns "config changes were not saved". With the fix,
    // held locks are passed through and no such attempt is made.
    let out = std::io::Cursor::new(Vec::new());
    let mut sink = out;
    let request = ImportRequest {
        path: Some(&source),
        from_claude: false,
        from_codex: false,
        name: Some("testsrc"),
        all: false,
        grant_net: true,
        force: false,
        merge: false,
        config_path: None,
        project: false,
    };
    let result = cmd_import(request, &mut sink);
    let output = String::from_utf8_lossy(&sink.get_ref()).to_string();

    result.expect("import must succeed");
    // The fix means the consult path never tried to re-acquire a held lock.
    assert_eq!(
        newt_core::atomic_fs::lock_attempts_for_test(),
        0,
        "import consulted configs without re-acquiring held locks"
    );
    assert!(
        !output.contains("not saved"),
        "import reported a lock-contention warning: {output}"
    );
}

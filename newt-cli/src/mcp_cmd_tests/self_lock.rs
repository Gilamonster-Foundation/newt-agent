//! `newt mcp import` must not re-acquire the config locks it already holds
//! while consulting configs (namespace sanitization + grant checks).
//!
//! Regression for #2911: after the transaction locks were taken, the import
//! still consulted configs through `Config::load` / `Config::resolve`, which
//! tried to re-acquire the same lock — burning the ~100-retry contention
//! budget. The consults now go through `load_with_held_locks`, so no lock
//! attempt is made against a config the transaction already holds.

use super::*;

/// Pins every input the import's target selection reads: `NEWT_CONFIG`,
/// `NEWT_CONFIG_DIR`, `HOME` and the working directory. Restores them on
/// drop, so a failing assertion cannot leak them into a later test.
struct Sandbox {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    cwd: std::path::PathBuf,
}

impl Sandbox {
    fn new(home: &std::path::Path) -> Self {
        let keys = ["NEWT_CONFIG", "NEWT_CONFIG_DIR", "HOME"];
        let saved = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        let cwd = std::env::current_dir().unwrap();
        std::env::remove_var("NEWT_CONFIG");
        std::env::set_var("NEWT_CONFIG_DIR", home);
        std::env::set_var("HOME", home);
        std::env::set_current_dir(home).unwrap();
        Self { saved, cwd }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.cwd);
        for (key, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// Count `acquire_lock` attempts around the consult path. Import takes two
/// transaction locks (`mcp.toml` + `config.toml`) then must NOT attempt to
/// re-acquire either one while resolving namespace sanitization or the net
/// grant.
#[test]
#[serial_test::serial(real_fs)]
fn import_consults_configs_without_reacquiring_held_locks() {
    // A real config with an inline backend so resolve() does not synthesize
    // the localhost fallback and actually read the file.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let _sandbox = Sandbox::new(home);

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
    let output = String::from_utf8_lossy(sink.get_ref()).to_string();

    result.expect("import must succeed");
    // The whole import takes exactly its two transaction locks (`mcp.toml`
    // and `config.toml`), each on the first attempt. A consult that re-locked
    // a held config would add its full retry budget on top.
    assert_eq!(
        newt_core::atomic_fs::lock_attempts_for_test(),
        2,
        "import consulted configs without re-acquiring held locks"
    );
    assert!(
        !output.contains("not saved"),
        "import reported a lock-contention warning: {output}"
    );
}

/// `newt mcp import` snapshots the target config under its held lock AND
/// migrates it, so an imported result never carries un-migrated psyche labels
/// that the strict parsers would later refuse.
///
/// Regression for #2911 step 3: the snapshot read (`target_original`) used raw
/// `std::fs::read_to_string` — no psyche migration, no held-lock respect — so
/// importing into a config that still carried old `[tenacity]` labels committed
/// them verbatim.
#[test]
#[serial_test::serial(real_fs)]
fn import_snapshot_migrates_old_psyche_labels_under_held_lock() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let _sandbox = Sandbox::new(home);

    std::fs::create_dir_all(home.join("backends")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[[backends]]\nname = \"test\"\nendpoint = \"http://127.0.0.1:11434\"\n",
    )
    .unwrap();

    // Target mcp.toml carries old psyche labels; the snapshot read must migrate.
    let mcp_toml = home.join("mcp.toml");
    std::fs::write(
        &mcp_toml,
        "[tenacity]\ndefault = \"standard\"\n[[mcp_servers]]\nname = \"existing\"\n",
    )
    .unwrap();

    newt_core::atomic_fs::reset_lock_attempts_for_test();
    newt_core::psyche_import::reset_lock_contention_warned_for_test();

    let source = dir.path().join("source.json");
    std::fs::write(
        &source,
        "{\n  \"mcpServers\": {\n    \"testsrc\": {\n         \"command\": \"test-cmd\"\n        }\n  }\n}\n",
    )
    .unwrap();

    let out = std::io::Cursor::new(Vec::new());
    let mut sink = out;
    let request = ImportRequest {
        path: Some(&source),
        from_claude: false,
        from_codex: false,
        name: Some("testsrc"),
        all: false,
        grant_net: false,
        force: false,
        merge: false,
        config_path: None,
        project: false,
    };
    let result = cmd_import(request, &mut sink);
    let output = String::from_utf8_lossy(sink.get_ref()).to_string();

    result.expect("import must succeed");

    let final_text = std::fs::read_to_string(&mcp_toml).unwrap();
    assert!(
        final_text.contains("initiative"),
        "snapshot read must migrate old psyche labels under the held lock: {final_text}"
    );
    assert!(
        !final_text.contains("tenacity"),
        "snapshot read must not leave old tenacity labels: {final_text}"
    );
    assert!(
        !output.contains("not saved"),
        "import reported a lock-contention warning: {output}"
    );
}

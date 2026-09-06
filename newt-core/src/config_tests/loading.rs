use super::*;
use std::io::Write;
use tempfile::NamedTempFile;

// Root Config loading, defaults, and configured/unconfigured provenance.

#[test]
fn backendless_config_deserializes_empty_but_default_keeps_fallback() {
    // A config.toml with no [[backends]] must NOT inherit the struct-default
    // localhost Ollama — otherwise a drop-in-only setup gets a spurious
    // 'ollama' entry alongside its real backends (the migration regression).
    let cfg: Config = toml::from_str("providers = []\n").unwrap();
    assert!(
        cfg.backends.is_empty(),
        "absent [[backends]] deserializes to empty, got {:?}",
        cfg.backends
    );
    // But the no-config-file path (Config::default) keeps the fallback.
    assert_eq!(Config::default().backends.len(), 1);
    assert_eq!(Config::default().backends[0].name, "ollama");
    // Inline backends still load normally.
    let inline: Config =
        toml::from_str("[[backends]]\nname=\"x\"\nendpoint=\"http://h:1\"\nmodel=\"m\"\n").unwrap();
    assert_eq!(inline.backends.len(), 1);
    assert_eq!(inline.backends[0].name, "x");
}

#[test]
fn defaults_are_sensible() {
    let cfg = Config::default();
    assert_eq!(cfg.backends.len(), 1);
    assert_eq!(cfg.providers.len(), 0);
    assert_eq!(cfg.default_tier_order.len(), 4);
}

#[test]
fn load_happy_path() {
    let toml_text = r#"
[[backends]]
name = "local-ollama"
endpoint = "http://localhost:11434"
model = "mistral:7b"
tiers = ["FAST", "STANDARD"]

[[providers]]
name = "cloud"
command = "newt-cloud-shim"
model = "gpt-4.1-mini"
env_pass = ["CLOUD_TOKEN"]
tiers = ["COMPLEX", "REVIEW"]

default_tier_order = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]
"#;
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(toml_text.as_bytes()).unwrap();
    f.flush().unwrap();

    let cfg = Config::load(f.path()).unwrap();
    assert_eq!(cfg.backends.len(), 1);
    assert_eq!(cfg.backends[0].name, "local-ollama");
    assert_eq!(cfg.backends[0].effective_model(), Some("mistral:7b"));
    assert_eq!(cfg.backends[0].tiers, vec![Tier::Fast, Tier::Standard]);
    assert_eq!(cfg.providers.len(), 1);
    assert_eq!(cfg.providers[0].name, "cloud");
    assert_eq!(cfg.providers[0].model.as_deref(), Some("gpt-4.1-mini"));
    assert_eq!(cfg.providers[0].env_pass, vec!["CLOUD_TOKEN".to_string()]);
}

#[test]
fn missing_file_returns_io_error() {
    let result = Config::load(Path::new("/tmp/newt-does-not-exist-12345.toml"));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, NewtError::Io(_)),
        "expected Io error, got: {err:?}"
    );
}

#[test]
fn malformed_toml_returns_config_error() {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(b"{{{{").unwrap();
    f.flush().unwrap();

    let result = Config::load(f.path());
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, NewtError::Config(_)),
        "expected Config error, got: {err:?}"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn resolve_returns_default_when_no_file() {
    // Use a temp dir as cwd and clear env to ensure no candidates match.
    // Serial: mutates process-global cwd + HOME, which races any parallel
    // test that resolves paths (the unconfigured-provenance test shares
    // this lane for the same reason).
    let dir = tempfile::tempdir().unwrap();

    // Save & clear environment to isolate the test.
    let saved_config = std::env::var("NEWT_CONFIG").ok();
    let saved_home = std::env::var("HOME").ok();
    std::env::remove_var("NEWT_CONFIG");
    std::env::set_var("HOME", dir.path());

    // Run resolve from inside the temp dir so ./newt.toml won't exist.
    let prev_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let cfg = Config::resolve().unwrap();

    // Restore environment.
    std::env::set_current_dir(prev_dir).unwrap();
    if let Some(v) = saved_home {
        std::env::set_var("HOME", v);
    }
    if let Some(v) = saved_config {
        std::env::set_var("NEWT_CONFIG", v);
    }

    assert_eq!(cfg.backends.len(), 1);
    assert_eq!(cfg.backends[0].name, "ollama");
    assert!(
        cfg.is_unconfigured(),
        "a resolve with no config anywhere is the unboxing state"
    );
}

#[test]
fn default_config_is_unconfigured() {
    assert!(
        Config::default().is_unconfigured(),
        "the struct default's sole backend is the compiled-in fallback"
    );
}

#[test]
fn dropin_merge_clears_the_unconfigured_flag() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("gpu.toml"),
        "endpoint = \"http://gpu:11434\"\n",
    )
    .unwrap();
    let mut cfg = Config::default();
    assert!(cfg.is_unconfigured());
    merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    assert!(
        !cfg.is_unconfigured(),
        "a successfully merged drop-in is operator configuration"
    );
}

#[test]
fn skipped_and_malformed_dropins_do_not_clear_the_unconfigured_flag() {
    let dir = tempfile::tempdir().unwrap();
    // Malformed TOML → warn + skip.
    std::fs::write(dir.path().join("bad.toml"), "endpoint = 42\n").unwrap();
    // No endpoint and no model_path → skipped by the destination check.
    std::fs::write(dir.path().join("hollow.toml"), "model = \"m\"\n").unwrap();
    let mut cfg = Config::default();
    merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    assert!(
        cfg.is_unconfigured(),
        "only a drop-in that actually merges counts as configuration"
    );
}

/// **#1989.** Reads `$NEWT_PROVIDER` without touching it, which is why it
/// flaked: `BackendOverride::apply` routes through the shared selection
/// precedence, and that consults `$NEWT_PROVIDER` first (`config.rs`, "the
/// most-specific PRESENT selector decides"). A selector naming no backend is a
/// deliberate typed ERROR rather than a fallback — so while a sibling holds
/// `NEWT_PROVIDER=ghost`, `try_apply` fails, `apply` swallows the failure into
/// a `tracing::warn!`, `backend_fallback` stays set, and this assertion trips.
///
/// **Two guards, because the writers are two disjoint populations** and
/// neither mechanism covers both:
///
/// * `serial(real_fs)` — the `config::tests` writers (`"ghost"`, `"hollow"`,
///   `"a"`, `"b"`) mutate `NEWT_PROVIDER` with a raw `unsafe set_var` and are
///   isolated ONLY by this lane. `process_env`'s lock cannot see them; its own
///   doc says so: it "cannot stop … an unguarded read", and these are
///   unguarded writes.
/// * `GlobalSettingsGuard` — `runtime.rs`'s writers take `process_env`'s lock
///   through this guard but sit in NO lane, so the lane alone would leave them
///   racing this test in a full `--lib` run.
///
/// The guard is the existing machinery rather than a fresh `process_env::lock()`:
/// it already snapshots `NEWT_PROVIDER` (it is in `ENV_KEYS`) and restores it on
/// drop even through a panic, which is what lets the body clear the variable
/// instead of merely hoping it is unset. That last part matters — the lane and
/// the lock exclude sibling TESTS, but neither does anything about an operator
/// (or a CI job) whose environment already exports `NEWT_PROVIDER`. The test
/// asserted a precondition it never established.
///
/// The assertion itself is unchanged: an explicit `--backend-*` override must
/// still clear the unconfigured flag.
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER via the selection precedence
#[test]
fn cli_backend_override_clears_the_unconfigured_flag() {
    let _env = crate::test_guard::GlobalSettingsGuard::acquire();
    // Establish the precondition rather than assume it; the guard puts back
    // whatever was here.
    crate::process_env::remove_var("NEWT_PROVIDER");
    let mut cfg = Config::default();
    BackendOverride {
        model: Some("qwen3:32b".into()),
        ..Default::default()
    }
    .apply(&mut cfg);
    assert!(
        !cfg.is_unconfigured(),
        "an explicit --backend-* flag is operator configuration"
    );
    // …but an empty override stays a no-op.
    let mut untouched = Config::default();
    BackendOverride::default().apply(&mut untouched);
    assert!(untouched.is_unconfigured());
}

/// `resolve()`-boundary provenance: inline `[[backends]]` in a config file
/// and `backends/*.toml` drop-ins both mean "configured"; a config file
/// that declares neither is as bare as no file at all. Serial: pins
/// NEWT_CONFIG_DIR / HOME / cwd like `resolve_returns_default_when_no_file`.
#[serial_test::serial(real_fs)]
#[test]
fn resolve_reports_unconfigured_only_without_operator_backends() {
    let dir = tempfile::tempdir().unwrap();
    let saved_config = std::env::var_os("NEWT_CONFIG");
    let saved_config_dir = std::env::var_os(NEWT_CONFIG_DIR_ENV);
    let saved_home = std::env::var_os("HOME");
    std::env::remove_var("NEWT_CONFIG");
    std::env::set_var(NEWT_CONFIG_DIR_ENV, dir.path());
    std::env::set_var("HOME", dir.path());
    let prev_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let config_toml = dir.path().join("config.toml");

    // 1. Config file with no backends and no drop-ins → still unboxed.
    std::fs::write(&config_toml, "providers = []\n").unwrap();
    let bare = Config::resolve().unwrap();

    // 2. Inline [[backends]] → configured.
    std::fs::write(
        &config_toml,
        "[[backends]]\nname = \"gpu\"\nendpoint = \"http://gpu:8000\"\n",
    )
    .unwrap();
    let inline = Config::resolve().unwrap();

    // 3. Backend-less config file + a drop-in → configured.
    std::fs::write(&config_toml, "providers = []\n").unwrap();
    std::fs::create_dir_all(dir.path().join("backends")).unwrap();
    std::fs::write(
        dir.path().join("backends").join("gpu.toml"),
        "endpoint = \"http://gpu:11434\"\n",
    )
    .unwrap();
    let dropin = Config::resolve().unwrap();

    std::env::set_current_dir(prev_dir).unwrap();
    match saved_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match saved_config_dir {
        Some(v) => std::env::set_var(NEWT_CONFIG_DIR_ENV, v),
        None => std::env::remove_var(NEWT_CONFIG_DIR_ENV),
    }
    if let Some(v) = saved_config {
        std::env::set_var("NEWT_CONFIG", v);
    }

    assert!(
        bare.is_unconfigured(),
        "a backend-less config file is as bare as no file"
    );
    assert!(!inline.is_unconfigured(), "inline [[backends]] configure");
    assert!(!dropin.is_unconfigured(), "a drop-in configures");
}

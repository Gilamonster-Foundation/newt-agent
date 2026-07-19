//! Integration coverage for host-side MCP secret resolution (issue #1301).
//!
//! Exercises the REAL env / file / interpolation resolvers end-to-end — the
//! impure edge the unit tier deliberately never touches (the unit tests use an
//! injected resolver). Real filesystem + process env, so this is the
//! integration tier: env vars are uniquely named per test and cleaned up.

use newt_core::agent_identity::SecretRef;
use newt_core::mcp::{interpolate, SecretValue};

#[test]
fn literal_without_tokens_resolves_verbatim() {
    // The common case — a plain env value / header — touches nothing.
    assert_eq!(
        SecretValue::literal("plain-value")
            .resolve()
            .unwrap()
            .expose(),
        "plain-value"
    );
}

#[test]
fn interpolation_reads_a_real_env_var_embedded_in_a_larger_string() {
    // SAFETY: a var name unique to this test, set and removed within it.
    unsafe { std::env::set_var("NEWT_1301_UAT_TOKEN", "s3cr3t") };
    // `${VAR}` bare and `${env:VAR}` both read the env, and the literal text
    // around the token is preserved.
    assert_eq!(
        interpolate("Bearer ${NEWT_1301_UAT_TOKEN}!").unwrap(),
        "Bearer s3cr3t!"
    );
    assert_eq!(
        SecretValue::literal("${env:NEWT_1301_UAT_TOKEN}")
            .resolve()
            .unwrap()
            .expose(),
        "s3cr3t"
    );
    unsafe { std::env::remove_var("NEWT_1301_UAT_TOKEN") };
    // A missing env var fails LOUD (never a silent empty) — a spawn must not
    // proceed with a blank secret.
    assert!(SecretValue::literal("${env:NEWT_1301_UAT_TOKEN}")
        .resolve()
        .is_err());
}

#[test]
fn file_scheme_and_ref_read_the_first_non_empty_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tok");
    std::fs::write(&path, "\n  file-secret \n").unwrap();

    // `${file:PATH}` interpolation.
    let template = format!("${{file:{}}}", path.display());
    assert_eq!(interpolate(&template).unwrap(), "file-secret");

    // The equivalent SecretRef object form resolves the same secret.
    let v = SecretValue::Ref(SecretRef {
        file: Some(path.display().to_string()),
        ..Default::default()
    });
    assert_eq!(v.resolve().unwrap().expose(), "file-secret");
}

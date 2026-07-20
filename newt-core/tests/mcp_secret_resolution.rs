//! Integration coverage for host-side MCP secret resolution (issue #1301).
//!
//! Exercises the REAL env / file / interpolation resolvers end-to-end — the
//! impure edge the unit tier deliberately never touches (the unit tests use an
//! injected resolver). Real filesystem + process env, so this is the
//! integration tier: env vars are uniquely named per test and cleaned up.

use newt_core::agent_identity::SecretRef;
use newt_core::mcp::{interpolate, SecretValue};
// `resolve_secret_under_trust` + `McpTrust` are exercised only by the
// `#[cfg(unix)]` cmd-trust test below; gate the import so Windows (where that
// test is absent) doesn't see them as unused under `-D warnings`.
#[cfg(unix)]
use newt_core::mcp::{resolve_secret_under_trust, McpTrust};

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

/// The #1301 CRITICAL acceptance test, end-to-end with a REAL subprocess
/// side-effect: a `${cmd:…}` (and a `{ cmd = … }` ref) runs on the host ONLY for
/// a TRUSTED source, NEVER for an UNTRUSTED (discovered Claude/project) source.
/// A marker file is the observable side effect; its (non-)creation is the proof.
// Unix-only: real `${cmd:…}` host execution uses a unix shell (`sh -c` +
// `touch`); the env/file resolvers above stay cross-platform.
#[cfg(unix)]
#[test]
fn cmd_executes_only_for_trusted_sources_never_for_untrusted() {
    let dir = tempfile::tempdir().unwrap();

    // (1) TRUSTED literal `${cmd:touch <marker>}` → the command RUNS (Vault path
    // stays working) and its stdout is the resolved secret.
    let trusted_marker = dir.path().join("trusted.marker");
    let trusted_val = SecretValue::literal(format!(
        "${{cmd:touch '{}' && printf tok}}",
        trusted_marker.display()
    ));
    let resolved = resolve_secret_under_trust(&trusted_val, McpTrust::Trusted)
        .expect("a trusted ${cmd:…} must resolve");
    assert_eq!(resolved.expose(), "tok");
    assert!(
        trusted_marker.exists(),
        "the trusted ${{cmd:…}} must have executed on the host"
    );

    // (2) UNTRUSTED literal with the same `${cmd:…}` → passes VERBATIM, the
    // command NEVER runs, no marker appears.
    let untrusted_marker = dir.path().join("untrusted.marker");
    let literal = format!(
        "${{cmd:touch '{}' && printf tok}}",
        untrusted_marker.display()
    );
    let got = resolve_secret_under_trust(&SecretValue::literal(&literal), McpTrust::Untrusted)
        .expect("an untrusted literal passes through, never errors");
    assert_eq!(got.expose(), literal, "the child gets the literal verbatim");
    assert!(
        !untrusted_marker.exists(),
        "an untrusted ${{cmd:…}} must NOT execute on the host"
    );

    // (3) UNTRUSTED structured `{ cmd = … }` ref → REJECTED, command never runs.
    let ref_marker = dir.path().join("ref.marker");
    let err = resolve_secret_under_trust(
        &SecretValue::Ref(SecretRef {
            cmd: Some(format!("touch '{}'", ref_marker.display())),
            ..Default::default()
        }),
        McpTrust::Untrusted,
    )
    .expect_err("an untrusted {cmd=…} ref must be rejected");
    assert!(format!("{err}").contains("untrusted"), "{err}");
    assert!(
        !ref_marker.exists(),
        "a rejected untrusted {{cmd=…}} ref must NOT execute on the host"
    );
}

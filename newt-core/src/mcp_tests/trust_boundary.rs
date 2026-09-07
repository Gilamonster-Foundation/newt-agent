use super::*;

// ---- #1301 trust boundary: resolve_secret_under_trust ----
#[test]
fn untrusted_literal_with_cmd_token_passes_through_verbatim_no_execution() {
    // The heart of the #1301 fix: an UNTRUSTED source's literal is handed to
    // the child VERBATIM — a `${cmd:…}` is inert text, never interpolated,
    // so the resolver / a subprocess is never reached. Pure: the untrusted
    // branch structurally cannot execute anything.
    let value = SecretValue::literal("${cmd:touch /tmp/newt-1301-should-not-exist}");
    let got = resolve_secret_under_trust(&value, McpTrust::Untrusted).unwrap();
    assert_eq!(
        got.expose(),
        "${cmd:touch /tmp/newt-1301-should-not-exist}",
        "an untrusted ${{cmd:…}} literal must reach the child verbatim, not run"
    );
    // A bare `${VAR}` in an untrusted value is likewise inert.
    let bare = SecretValue::literal("Bearer ${SOME_VAR}");
    assert_eq!(
        resolve_secret_under_trust(&bare, McpTrust::Untrusted)
            .unwrap()
            .expose(),
        "Bearer ${SOME_VAR}"
    );
}
#[test]
fn untrusted_structured_ref_is_rejected() {
    // An UNTRUSTED source may not name a command to run or a file to read.
    for r in [
        SecretRef {
            cmd: Some("touch /tmp/pwned".into()),
            ..Default::default()
        },
        SecretRef {
            file: Some("/etc/passwd".into()),
            ..Default::default()
        },
        SecretRef {
            env: Some("HOME".into()),
            ..Default::default()
        },
    ] {
        let err = resolve_secret_under_trust(&SecretValue::Ref(r), McpTrust::Untrusted)
            .expect_err("an untrusted {env|file|cmd} ref must be rejected");
        assert!(
            format!("{err}").contains("untrusted"),
            "error should name the trust violation: {err}"
        );
    }
}
#[test]
fn trusted_literal_without_token_resolves_verbatim() {
    // A trusted literal with no token is a pure pass-through (no subprocess);
    // the token-bearing trusted path (the Vault `${cmd:…}`) is proven in the
    // integration tier (mcp_secret_resolution.rs) since it runs a real command.
    assert_eq!(
        resolve_secret_under_trust(&SecretValue::literal("plain"), McpTrust::Trusted)
            .unwrap()
            .expose(),
        "plain"
    );
}

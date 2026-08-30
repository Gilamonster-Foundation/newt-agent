use super::*;

#[test]
fn base_url_fires_even_with_configured_backends_and_trims_v1() {
    let c = codex_env_backend(
        Some("https://api.openai.com/v1/"),
        Some("sk-x"),
        Some("gpt-4.1"),
        None,
        true,
    )
    .expect("explicit base url is a deliberate redirect");
    assert_eq!(c.url, "https://api.openai.com");
    assert_eq!(c.active_model.as_deref(), Some("gpt-4.1"));
    assert_eq!(
        c.requested_model.as_deref(),
        Some("gpt-4.1"),
        "the env model is an operator REQUEST"
    );
    assert_eq!(c.api_key.as_deref(), Some("sk-x"));
    assert_eq!(c.kind, newt_core::BackendKind::Openai);
}

#[test]
fn bare_key_fires_only_with_no_configured_backends() {
    assert!(
        codex_env_backend(None, Some("sk-x"), None, None, true).is_none(),
        "a stray OPENAI_API_KEY must never hijack a configured setup"
    );
    let c =
        codex_env_backend(None, Some("sk-x"), None, None, false).expect("zero-config onboarding");
    assert_eq!(c.url, "https://api.openai.com");
    assert!(
        c.active_model.is_none(),
        "adopt() fills the model at session start"
    );
}

#[test]
fn empty_values_do_not_fire() {
    assert!(codex_env_backend(Some("  "), Some(""), None, None, false).is_none());
    assert!(codex_env_backend(None, None, Some("gpt-4.1"), None, false).is_none());
}

#[test]
fn stored_decisions_parse_with_canonical_and_alias_spellings() {
    for (body, want) in [
        ("decision = \"use-always\"\n", Some(CodexEnvDecision::UseIt)),
        (
            "decision = \"ignore-always\"\n",
            Some(CodexEnvDecision::Skip),
        ),
        ("# c\ndecision=\"always\"", Some(CodexEnvDecision::UseIt)),
        ("decision = \"never\"", Some(CodexEnvDecision::Skip)),
        ("decision = \"maybe\"", None),
        ("", None),
    ] {
        assert_eq!(parse_codex_env_decision(body), want, "{body:?}");
    }
}

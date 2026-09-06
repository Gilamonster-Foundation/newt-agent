use super::*;

// Secret redaction at both helper and Config serialization boundaries.

#[test]
fn to_redacted_toml_hides_mcp_secrets_but_keeps_shape() {
    let cfg: Config = toml::from_str(
        r#"
            [[backends]]
            name = "remote"
            endpoint = "http://remote:8000"
            model = "qwen3:32b"
            tiers = []
            kind = "openai"
            api_key_file = "~/.newt/openai.key"

            [[mcp_servers]]
            name = "gh"
            type = "http"
            url = "https://api.example/mcp"
            [mcp_servers.headers]
            Authorization = "Bearer sk-super-secret-token"
            [mcp_servers.env]
            GH_TOKEN = "ghp_rawsecretvalue"
            RUST_LOG = "debug"
            "#,
    )
    .unwrap();

    let dump = cfg.to_redacted_toml().unwrap();
    // The raw secret VALUES never appear…
    assert!(
        !dump.contains("sk-super-secret-token"),
        "header secret leaked:\n{dump}"
    );
    assert!(
        !dump.contains("ghp_rawsecretvalue"),
        "env secret leaked:\n{dump}"
    );
    // …but the KEYS and the placeholder do, so the audit shows the shape.
    assert!(dump.contains("Authorization"));
    assert!(dump.contains("GH_TOKEN"));
    assert!(dump.contains(Config::REDACTED));
    // Secret *references* (a path) are kept — they name where a secret lives.
    assert!(
        dump.contains("~/.newt/openai.key"),
        "api_key_file reference kept"
    );
    // Non-secret structure is intact.
    assert!(dump.contains("http://remote:8000"));
}

#[test]
fn to_redacted_toml_redacts_literals_but_keeps_secret_references() {
    // A literal secret AND a `${cmd:…}` interpolation literal are both
    // redacted (a literal can embed raw secret text); a `{ cmd = … }`
    // SecretRef is a REFERENCE — it names where the secret lives, not the
    // secret — so it is kept, exactly like `api_key_file`.
    let cfg: Config = toml::from_str(
        r#"
            [[mcp_servers]]
            name = "gh"
            command = "gh-mcp"
            [mcp_servers.env]
            RAW = "ghp_rawinlinesecret"
            VAULTED = { cmd = "vault kv get -field=token secret/data/gh" }
            [mcp_servers.headers]
            Authorization = "Bearer ${cmd:vault kv get -field=token secret/data/api}"
            "#,
    )
    .unwrap();

    let dump = cfg.to_redacted_toml().unwrap();
    // Literal secret and the interpolation literal never appear…
    assert!(
        !dump.contains("ghp_rawinlinesecret"),
        "raw secret leaked:\n{dump}"
    );
    assert!(
        !dump.contains("secret/data/api"),
        "interpolation literal leaked:\n{dump}"
    );
    assert!(dump.contains(Config::REDACTED));
    // …but the KEYS survive, and the SecretRef reference is kept (it names
    // a location, not a secret) — the operator can audit their wiring.
    assert!(dump.contains("RAW"));
    assert!(dump.contains("VAULTED"));
    assert!(dump.contains("Authorization"));
    assert!(
        dump.contains("vault kv get -field=token secret/data/gh"),
        "SecretRef reference kept:\n{dump}"
    );
}

#[test]
fn to_redacted_toml_redacts_url_userinfo_query_and_args() {
    // FIX 5 (#1301): url and args are plain strings (no SecretValue wrapper),
    // so URL-embedded creds and `--token` args must be redacted before the
    // audit dump can leak them.
    let cfg: Config = toml::from_str(
        r#"
            [[mcp_servers]]
            name = "gh"
            type = "http"
            url = "https://alice:sk-URLPASS@api.example/mcp?api_key=SECRETQP&region=us"
            args = ["--token=sk-EQ", "--api-key", "sk-SPACE", "--verbose"]
            "#,
    )
    .unwrap();
    let dump = cfg.to_redacted_toml().unwrap();
    // None of the secret material survives…
    for leaked in ["sk-URLPASS", "SECRETQP", "sk-EQ", "sk-SPACE", "alice"] {
        assert!(!dump.contains(leaked), "`{leaked}` leaked:\n{dump}");
    }
    // …but the non-secret structure does.
    assert!(dump.contains("api.example/mcp"), "host/path kept:\n{dump}");
    assert!(dump.contains("region=us"), "non-secret param kept:\n{dump}");
    assert!(dump.contains("--verbose"), "non-secret arg kept:\n{dump}");
    assert!(dump.contains(Config::REDACTED));
}

#[test]
fn redact_url_and_args_helpers_are_precise() {
    // Userinfo + sensitive query param redacted; scheme/host/path/fragment
    // and a non-sensitive param preserved.
    assert_eq!(
        redact_url_secrets("https://u:p@h.example/mcp?token=abc&keep=1#frag"),
        format!(
            "https://{r}@h.example/mcp?token={r}&keep=1#frag",
            r = Config::REDACTED
        )
    );
    // No userinfo, no sensitive params → unchanged.
    assert_eq!(
        redact_url_secrets("https://h.example/mcp?region=us"),
        "https://h.example/mcp?region=us"
    );
    // An `@` in the path is not userinfo.
    assert_eq!(
        redact_url_secrets("https://h.example/a@b"),
        "https://h.example/a@b"
    );
    // Both arg forms; a non-sensitive flag with a value is untouched.
    assert_eq!(
        redact_arg_secrets(&[
            "--token=sk-1".into(),
            "--api-key".into(),
            "sk-2".into(),
            "--model".into(),
            "gpt".into(),
        ]),
        vec![
            format!("--token={}", Config::REDACTED),
            "--api-key".to_string(),
            Config::REDACTED.to_string(),
            "--model".to_string(),
            "gpt".to_string(),
        ]
    );
}

#[test]
fn redact_url_and_args_helpers_normalize_credential_spellings() {
    let dump = redact_url_secrets(
            "https://h.example/mcp?client%5Fsecret=one&refresh-token=two&X-Amz-Signature=three&region=us",
        );
    for leaked in ["one", "two", "three"] {
        assert!(!dump.contains(leaked), "`{leaked}` leaked: {dump}");
    }
    assert!(dump.contains("region=us"));

    assert_eq!(
        redact_arg_secrets(&[
            "--access-token=one".into(),
            "--client_secret".into(),
            "two".into(),
            "-H".into(),
            "Authorization: Bearer three".into(),
            "--header=X-API-Key: four".into(),
            "-H".into(),
            "X-Auth-Token: five".into(),
            "-HX-Client-Secret: six".into(),
            "--auth=seven".into(),
            "--oauth2-bearer".into(),
            "eight".into(),
            "-uuser:nine".into(),
            "-b".into(),
            "ten".into(),
            "--cookie=eleven".into(),
            "--header=X-Trace: keep".into(),
        ]),
        vec![
            format!("--access-token={}", Config::REDACTED),
            "--client_secret".to_string(),
            Config::REDACTED.to_string(),
            "-H".to_string(),
            Config::REDACTED.to_string(),
            format!("--header={}", Config::REDACTED),
            "-H".to_string(),
            Config::REDACTED.to_string(),
            format!("-H{}", Config::REDACTED),
            format!("--auth={}", Config::REDACTED),
            "--oauth2-bearer".to_string(),
            Config::REDACTED.to_string(),
            format!("-u{}", Config::REDACTED),
            "-b".to_string(),
            Config::REDACTED.to_string(),
            format!("--cookie={}", Config::REDACTED),
            "--header=X-Trace: keep".to_string(),
        ]
    );
}

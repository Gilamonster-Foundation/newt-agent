use super::*;

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_literal_url_and_argument_credentials_without_leaking_them() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    let cases = [
        (
            "userinfo-secret-4319",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://user:userinfo-secret-4319@broker.example.test/mcp"
            } } }"#,
        ),
        (
            "query-secret-8274",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://broker.example.test/mcp?api_key=query-secret-8274"
            } } }"#,
        ),
        (
            "argument-secret-6621",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["--token", "argument-secret-6621"]
            } } }"#,
        ),
    ];

    for (secret, json) in cases {
        std::fs::write(&source, json).unwrap();
        newt(&sb)
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--name",
                "review",
                "--grant-net",
            ])
            .assert()
            .failure()
            .stdout(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains("move credentials"));
        assert!(
            !sb.config_dir.join("mcp.toml").exists(),
            "a rejected credential must never enter mcp.toml"
        );
        assert!(
            !sb.config_dir.join("config.toml").exists(),
            "a rejected credential must never create a network grant"
        );
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_claude_import_rejects_literal_env_and_headers_without_leaking_values() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    for (secret, json, borrowed_key) in [
        (
            "literal-env-value-7741",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "env": { "ACCESS_TOKEN": "literal-env-value-7741" }
            } } }"#,
            "review.env.ACCESS_TOKEN",
        ),
        (
            "literal-header-value-8832",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://broker.example.test/mcp",
                "headers": { "Authorization": "Bearer literal-header-value-8832" }
            } } }"#,
            "review.headers.Authorization",
        ),
    ] {
        std::fs::write(&source, json).unwrap();
        newt(&sb)
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--name",
                "review",
                "--grant-net",
            ])
            .assert()
            .failure()
            .stdout(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains("credential field(s)"))
            .stderr(predicate::str::contains(borrowed_key).not())
            .stderr(predicate::str::contains(secret).not());
        assert!(!sb.config_dir.join("mcp.toml").exists());
        assert!(!sb.config_dir.join("config.toml").exists());
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_encoded_and_alias_credentials_without_leaking_them() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    let cases = [
        (
            "client-secret-value-3197",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://broker.example.test/mcp?client%5Fsecret=client-secret-value-3197"
            } } }"#,
        ),
        (
            "signature-value-8264",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://broker.example.test/mcp?X-Amz-Signature=signature-value-8264"
            } } }"#,
        ),
        (
            "access-token-value-5561",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["--access-token", "access-token-value-5561"]
            } } }"#,
        ),
        (
            "header-token-value-4428",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["-H", "Authorization: Bearer header-token-value-4428"]
            } } }"#,
        ),
        (
            "api-key-header-value-6173",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["-H", "X-API-Key: api-key-header-value-6173"]
            } } }"#,
        ),
        (
            "client-secret-header-value-2841",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["--header=X-Client-Secret: client-secret-header-value-2841"]
            } } }"#,
        ),
        (
            "auth-token-header-value-3902",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["-HX-Auth-Token: auth-token-header-value-3902"]
            } } }"#,
        ),
    ];

    for (secret, json) in cases {
        std::fs::write(&source, json).unwrap();
        newt(&sb)
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--name",
                "review",
            ])
            .assert()
            .failure()
            .stdout(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains("credential"));
        assert!(!sb.config_dir.join("mcp.toml").exists());
        assert!(!sb.config_dir.join("config.toml").exists());
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_credential_placeholders_in_url_and_argv() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    for json in [
        r#"{ "mcpServers": { "review": {
            "type": "http",
            "url": "https://broker.example.test/mcp?token=${REVIEW_TOKEN}"
        } } }"#,
        r#"{ "mcpServers": { "review": {
            "command": "review-mcp",
            "args": ["--token", "${REVIEW_TOKEN}"]
        } } }"#,
        r#"{ "mcpServers": { "review": {
            "type": "http",
            "url": "https://broker.example.test/${MCP_PATH}"
        } } }"#,
        r#"{ "mcpServers": { "review": {
            "command": "review-mcp",
            "args": ["--ordinary=${ORDINARY_VALUE}"]
        } } }"#,
    ] {
        std::fs::write(&source, json).unwrap();
        newt(&sb)
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--name",
                "review",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains("environment reference"));
        assert!(!sb.config_dir.join("mcp.toml").exists());
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_invalid_transport_urls_without_a_network_grant() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    for (url, expected, secret) in [
        ("ftp://example.test/mcp", "unsupported URL scheme", None),
        (
            "https://example.test/mcp#access_token=fragment-secret-8127",
            "URL fragment",
            Some("fragment-secret-8127"),
        ),
    ] {
        std::fs::write(
            &source,
            serde_json::json!({
                "mcpServers": {
                    "review": { "type": "http", "url": url }
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut command = newt(&sb);
        let assertion = command
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--name",
                "review",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected));
        if let Some(secret) = secret {
            assertion
                .stdout(predicate::str::contains(secret).not())
                .stderr(predicate::str::contains(secret).not());
        }
        assert!(!sb.config_dir.join("mcp.toml").exists());
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_codex_import_rejects_literal_headers_without_leaking_values() {
    let sb = sandbox();
    let codex_home = sb.home.join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    let config = codex_home.join("config.toml");
    for (secret, body, borrowed_key) in [
        (
            "never-copy-this-header",
            r#"
[mcp_servers.review]
url = "https://review-broker.example.test/mcp"
bearer_token_env_var = "REVIEW_TOKEN"
http_headers = { X-Literal = "never-copy-this-header" }
env_http_headers = { X-Trace = "TRACE_TOKEN" }

[mcp_servers.unrelated]
command = "unrelated-mcp"
"#,
            "review.headers.X-Literal",
        ),
        (
            "never-copy-this-env",
            r#"
[mcp_servers.review]
command = "review-mcp"
env = { ACCESS_TOKEN = "never-copy-this-env" }
"#,
            "review.env.ACCESS_TOKEN",
        ),
    ] {
        std::fs::write(&config, body).unwrap();
        newt(&sb)
            .env("CODEX_HOME", &codex_home)
            .args([
                "mcp",
                "import",
                "--from-codex",
                "--name",
                "review",
                "--grant-net",
            ])
            .assert()
            .failure()
            .stdout(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains("credential field(s)"))
            .stderr(predicate::str::contains(borrowed_key).not())
            .stderr(predicate::str::contains(secret).not());
    }

    assert!(
        !sb.config_dir.join("mcp.toml").exists(),
        "rejected import must not create MCP config"
    );
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "rejected import must not grant network authority"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_transport_owned_headers_without_writing() {
    let sb = sandbox();
    let claude = sb.cwd.join("borrowed.json");
    std::fs::write(
        &claude,
        r#"{ "mcpServers": { "review": {
            "type": "http",
            "url": "https://review-broker.example.test/mcp",
            "headers": { "MCP-Session-Id": "${SESSION_ID}" }
        } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            claude.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "unsupported or ambiguous transport semantics",
        ));

    let codex_home = sb.home.join("codex-home-owned-header");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(
        codex_home.join("config.toml"),
        r#"
[mcp_servers.review]
url = "https://review-broker.example.test/mcp"
env_http_headers = { hOsT = "HOST_OVERRIDE" }
"#,
    )
    .unwrap();
    newt(&sb)
        .env("CODEX_HOME", &codex_home)
        .args([
            "mcp",
            "import",
            "--from-codex",
            "--name",
            "review",
            "--grant-net",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "unsupported or ambiguous transport semantics",
        ));

    assert!(
        !sb.config_dir.join("mcp.toml").exists(),
        "rejected transport-owned headers must not create MCP config"
    );
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "rejected transport-owned headers must not grant network authority"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_claude_import_rejects_unknown_semantics_before_trust_promotion() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": {
            "command": "review-mcp",
            "unsupported_policy": "must-not-be-erased"
        } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported fields"))
        .stderr(predicate::str::contains("must-not-be-erased").not());
    assert!(!sb.config_dir.join("mcp.toml").exists());
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_nonportable_server_names_before_writing() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    for invalid in ["../outside", "review__source", "review--source"] {
        let mut servers = serde_json::Map::new();
        servers.insert(
            invalid.to_string(),
            serde_json::json!({"command": "review-mcp"}),
        );
        std::fs::write(
            &source,
            serde_json::json!({"mcpServers": servers}).to_string(),
        )
        .unwrap();

        newt(&sb)
            .args(["mcp", "import", source.to_str().unwrap(), "--name", invalid])
            .assert()
            .failure()
            .stderr(predicate::str::contains("portable single-component"));
    }
    assert!(!sb.config_dir.join("mcp.toml").exists());
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_effective_namespace_collisions() {
    let sb = sandbox();
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "[[mcp_servers]]\nname = \"review-source\"\ncommand = \"owned\"\n",
    )
    .unwrap();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review_source": { "command": "borrowed" } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review_source",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("effective tool namespace"));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_every_url_query_without_echoing_it() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": {
            "type": "http", "url": "https://broker.example.test/mcp?auth=do-not-echo"
        } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("URL query"))
        .stderr(predicate::str::contains("do-not-echo").not());
    assert!(!sb.config_dir.join("mcp.toml").exists());
}

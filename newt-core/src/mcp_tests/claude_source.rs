use super::*;

#[test]
fn parses_claude_stdio_and_sse_entries() {
    let cfg = serde_json::json!({
        "mcpServers": {
            "filesystem": { "command": "npx", "args": ["-y", "@mcp/fs"], "env": { "ROOT": "/tmp" } },
            "remote":     { "type": "sse", "url": "https://mcp.example/sse", "headers": { "Authorization": "Bearer x" } }
        }
    });
    let mut got = parse_claude_mcp(&cfg);
    got.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(got.len(), 2);

    let fs = got.iter().find(|e| e.name == "filesystem").unwrap();
    assert_eq!(fs.transport, TransportKind::Stdio); // inferred (no "type")
    assert_eq!(fs.command.as_deref(), Some("npx"));
    assert_eq!(fs.args, vec!["-y", "@mcp/fs"]);
    assert_eq!(
        fs.env.get("ROOT").and_then(SecretValue::as_literal),
        Some("/tmp")
    );

    let remote = got.iter().find(|e| e.name == "remote").unwrap();
    assert_eq!(remote.transport, TransportKind::Sse);
    assert_eq!(remote.url.as_deref(), Some("https://mcp.example/sse"));
}
#[test]
fn missing_mcpservers_key_is_empty_not_error() {
    assert!(parse_claude_mcp(&serde_json::json!({ "other": 1 })).is_empty());
}
#[test]
fn strict_claude_import_rejects_unknown_and_ambiguous_entries_independently() {
    let report = parse_claude_mcp_for_import(&serde_json::json!({
        "mcpServers": {
            "valid": { "command": "valid-mcp" },
            "unknown": { "command": "other-mcp", "policy": "restricted" },
            "ambiguous": {
                "type": "http",
                "url": "https://example.test/mcp",
                "command": "must-not-survive"
            }
        }
    }));

    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].name, "valid");
    assert_eq!(report.entries[0].trust, McpTrust::Untrusted);
    assert_eq!(report.rejected.len(), 2);
    assert!(report.rejected.iter().any(|rejected| {
        rejected.name.as_deref() == Some("unknown")
            && rejected.issue == McpImportIssue::UnknownField
    }));
    assert!(report.rejected.iter().any(|rejected| {
        rejected.name.as_deref() == Some("ambiguous")
            && rejected.issue == McpImportIssue::UnsupportedSemantics
    }));
}
#[test]
fn strict_claude_import_rejects_sse_and_nonportable_map_keys() {
    let report = parse_claude_mcp_for_import(&serde_json::json!({
        "mcpServers": {
            "valid": {
                "type": "http",
                "url": "https://example.test/mcp",
                "headers": { "X-Trace": "${TRACE}" }
            },
            "sse": { "type": "sse", "url": "https://example.test/sse" },
            "bad-header": {
                "type": "http",
                "url": "https://example.test/mcp",
                "headers": { "Bad\nHeader": "${TOKEN}" }
            },
            "header-case-collision": {
                "type": "http",
                "url": "https://example.test/mcp",
                "headers": { "X-Key": "${ONE}", "x-key": "${TWO}" }
            },
            "owned-host": {
                "type": "http",
                "url": "https://example.test/mcp",
                "headers": { "hOsT": "${HOST_OVERRIDE}" }
            },
            "owned-session": {
                "type": "http",
                "url": "https://example.test/mcp",
                "headers": { "MCP-Session-ID": "${SESSION_ID}" }
            },
            "owned-protocol": {
                "type": "http",
                "url": "https://example.test/mcp",
                "headers": { "mcp-protocol-version": "${PROTOCOL_VERSION}" }
            },
            "env-case-collision": {
                "command": "server",
                "env": { "TOKEN": "${ONE}", "token": "${TWO}" }
            }
        }
    }));

    assert_eq!(
        report
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["valid"]
    );
    assert_eq!(report.rejected.len(), 7);
    assert!(report
        .rejected
        .iter()
        .all(|entry| entry.issue == McpImportIssue::UnsupportedSemantics));
}
#[test]
fn parse_claude_mcp_marks_every_entry_untrusted() {
    let cfg = serde_json::json!({
        "mcpServers": { "x": { "command": "c", "env": { "K": "v" } } }
    });
    let got = parse_claude_mcp(&cfg);
    assert!(got.iter().all(|e| e.trust == McpTrust::Untrusted));
}
#[test]
fn parse_claude_mcp_folds_nested_oauth_client_id() {
    let cfg = serde_json::json!({
        "mcpServers": {
            "example": {
                "type": "http",
                "url": "https://mcp.example.com/mcp",
                "oauth": { "clientId": "public-client-abc123" }
            }
        }
    });
    let got = parse_claude_mcp(&cfg);
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].oauth_client_id.as_deref(),
        Some("public-client-abc123")
    );
}
#[test]
fn strict_claude_import_accepts_oauth_client_id() {
    let report = parse_claude_mcp_for_import(&serde_json::json!({
        "mcpServers": {
            "example": {
                "type": "http",
                "url": "https://mcp.example.com/mcp",
                "oauth": { "clientId": "public-client-abc123" }
            }
        }
    }));
    assert_eq!(report.rejected.len(), 0);
    assert_eq!(report.entries.len(), 1);
    assert_eq!(
        report.entries[0].oauth_client_id.as_deref(),
        Some("public-client-abc123")
    );
}

fn strict_import_with_oauth(oauth: serde_json::Value) -> McpImportParseReport {
    parse_claude_mcp_for_import(&serde_json::json!({
        "mcpServers": {
            "example": {
                "type": "http",
                "url": "https://mcp.example.com/mcp",
                "oauth": oauth
            }
        }
    }))
}

/// PR #3000 review: the strict import kept only `oauth.clientId` and silently
/// dropped `oauth.scopes`, widening the requested authority after adoption.
#[test]
fn strict_claude_import_preserves_oauth_scopes_and_callback_port() {
    let report = strict_import_with_oauth(serde_json::json!({
        "clientId": "public-client-abc123",
        "scopes": "files:read",
        "callbackPort": 8080
    }));
    assert_eq!(report.rejected, []);
    let entry = &report.entries[0];
    assert_eq!(
        entry.oauth_client_id.as_deref(),
        Some("public-client-abc123")
    );
    assert_eq!(entry.oauth_scopes.as_deref(), Some("files:read"));
    assert_eq!(entry.oauth_callback_port, Some(8080));
}

/// Unsupported or unknown nested `oauth` fields are refused, never dropped.
#[test]
fn strict_claude_import_refuses_unknown_oauth_fields() {
    for oauth in [
        serde_json::json!({ "clientId": "public-client", "authServerMetadataUrl": "https://auth.example.com/.well-known/openid-configuration" }),
        serde_json::json!({ "clientId": "public-client", "clientSecret": "s" }),
        serde_json::json!({ "client_id": "public-client" }),
    ] {
        let report = strict_import_with_oauth(oauth.clone());
        assert!(report.entries.is_empty(), "{oauth} was adopted");
        assert_eq!(report.rejected[0].issue, McpImportIssue::UnknownField);
    }
}

/// Malformed client IDs, scopes, ports and shapes are refused.
#[test]
fn strict_claude_import_refuses_malformed_oauth_values() {
    for oauth in [
        serde_json::json!("public-client"),
        serde_json::json!({ "clientId": 42 }),
        serde_json::json!({ "clientId": "" }),
        serde_json::json!({ "clientId": " padded " }),
        serde_json::json!({ "clientId": "has space" }),
        serde_json::json!({ "clientId": "public-client", "scopes": ["files:read"] }),
        serde_json::json!({ "clientId": "public-client", "scopes": "" }),
        serde_json::json!({ "clientId": "public-client", "scopes": "a  b" }),
        serde_json::json!({ "clientId": "public-client", "callbackPort": 0 }),
        serde_json::json!({ "clientId": "public-client", "callbackPort": 70000 }),
        serde_json::json!({ "clientId": "public-client", "callbackPort": "8080" }),
    ] {
        let report = strict_import_with_oauth(oauth.clone());
        assert!(report.entries.is_empty(), "{oauth} was adopted");
        assert_eq!(report.rejected[0].issue, McpImportIssue::InvalidShape);
    }
}

/// A callback port only means something for a pre-registered client; with
/// dynamic registration newt registers its own callback, so the port would be
/// silently ignored. Refuse instead.
#[test]
fn strict_claude_import_refuses_callback_port_without_client_id() {
    let report = strict_import_with_oauth(serde_json::json!({ "callbackPort": 8080 }));
    assert!(report.entries.is_empty());
    assert_eq!(
        report.rejected[0].issue,
        McpImportIssue::UnsupportedSemantics
    );
}

/// Discovery is lenient, but must not keep an entry whose OAuth restrictions
/// it cannot honour.
#[test]
fn parse_claude_mcp_drops_entry_with_invalid_oauth() {
    let cfg = serde_json::json!({
        "mcpServers": {
            "example": {
                "type": "http",
                "url": "https://mcp.example.com/mcp",
                "oauth": { "clientId": "public-client", "unknown": true }
            }
        }
    });
    assert!(parse_claude_mcp(&cfg).is_empty());
}

/// Adopted OAuth restrictions survive the trusted TOML write.
#[test]
fn imported_oauth_restrictions_round_trip_through_trusted_toml() {
    let report = strict_import_with_oauth(serde_json::json!({
        "clientId": "public-client-abc123",
        "scopes": "files:read",
        "callbackPort": 8080
    }));
    let table = entry_to_toml_table(&report.entries[0], None).unwrap();
    let text = format!("[[mcp_servers]]\n{table}");
    let back = parse_newt_mcp_toml(&text);
    assert_eq!(
        back[0].oauth_client_id.as_deref(),
        Some("public-client-abc123")
    );
    assert_eq!(back[0].oauth_scopes.as_deref(), Some("files:read"));
    assert_eq!(back[0].oauth_callback_port, Some(8080));
}

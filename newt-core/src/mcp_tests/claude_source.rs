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

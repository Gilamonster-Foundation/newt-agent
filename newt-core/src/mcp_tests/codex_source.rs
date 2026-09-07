use super::*;

#[test]
fn parses_codex_http_with_exact_name_and_only_credential_references() {
    let text = r#"
[mcp_servers."Case.Sensitive-name"]
url = "https://mcp.example.test/rpc"
enabled = false
bearer_token_env_var = "MCP_BEARER_TOKEN"
env_http_headers = { X-API-Key = "MCP_API_KEY", X-Trace = "TRACE_ID" }
"#;

    let got = parse_codex_mcp_toml(text);
    assert_eq!(got.len(), 1);
    let server = &got[0];
    assert_eq!(server.name, "Case.Sensitive-name");
    assert!(!server.enabled);
    assert_eq!(server.transport, TransportKind::Http);
    assert_eq!(server.url.as_deref(), Some("https://mcp.example.test/rpc"));
    assert_eq!(server.trust, McpTrust::Untrusted);
    assert_eq!(server.headers.len(), 3);
    assert_eq!(
        server.headers.get("Authorization"),
        Some(&SecretValue::literal("Bearer ${env:MCP_BEARER_TOKEN}"))
    );
    assert_eq!(
        server.headers.get("X-API-Key"),
        Some(&SecretValue::Ref(SecretRef {
            env: Some("MCP_API_KEY".into()),
            ..Default::default()
        }))
    );
    assert_eq!(
        server.headers.get("X-Trace"),
        Some(&SecretValue::Ref(SecretRef {
            env: Some("TRACE_ID".into()),
            ..Default::default()
        }))
    );
}
#[test]
fn codex_parser_reports_literal_headers_and_environment_values_without_values() {
    let text = r#"
[mcp_servers.literal-env]
command = "local-server"
env = { RUST_LOG = "debug", ACCESS_TOKEN = "literal-secret" }

[mcp_servers.literal-headers]
url = "https://mcp.example.test/rpc"
http_headers = { X-Literal = "literal-secret" }
"#;

    let got = parse_codex_mcp_toml(text);
    assert_eq!(got.len(), 2);
    assert!(got
        .iter()
        .all(|entry| entry.env.is_empty() && entry.headers.is_empty()));
    let omitted = codex_mcp_omitted_field_counts(text);
    assert_eq!(omitted["literal-env"], 2);
    assert_eq!(omitted["literal-headers"], 1);
    assert!(!format!("{omitted:?}").contains("literal-secret"));
}
#[test]
fn strict_codex_import_reports_every_rejected_entry_and_hides_syntax_source() {
    let report = parse_codex_mcp_toml_for_import(
        r#"
[mcp_servers.valid]
command = "valid-mcp"

[mcp_servers.unknown]
command = "other-mcp"
policy = "literal-value-must-not-appear"
"#,
    )
    .unwrap();
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].name, "valid");
    assert_eq!(report.rejected.len(), 1);
    assert_eq!(report.rejected[0].name.as_deref(), Some("unknown"));
    assert_eq!(report.rejected[0].issue, McpImportIssue::UnknownField);

    let malformed = "[mcp_servers.bad]\nsecret = \"never-echo-this";
    let error = parse_codex_mcp_toml_for_import(malformed).unwrap_err();
    assert_eq!(error.to_string(), "invalid configuration syntax");
    assert!(!error.to_string().contains("never-echo-this"));
}
#[test]
fn parses_codex_environment_references_and_tool_timeout() {
    let text = r#"
[mcp_servers.local]
command = "server"
env_vars = ["ACCESS_TOKEN", { name = "TRACE_ID", source = "local" }]
tool_timeout_sec = 45

[mcp_servers.remote]
url = "https://mcp.example.test/rpc"
auth = "oauth"
tool_timeout_sec = 90
required = false
"#;

    let got = parse_codex_mcp_toml(text);
    assert_eq!(got.len(), 2);

    let local = got.iter().find(|server| server.name == "local").unwrap();
    assert_eq!(local.request_timeout_secs, Some(45));
    assert_eq!(
        local.env.get("ACCESS_TOKEN"),
        Some(&SecretValue::Ref(SecretRef {
            env: Some("ACCESS_TOKEN".into()),
            ..Default::default()
        }))
    );
    assert_eq!(
        local.env.get("TRACE_ID"),
        Some(&SecretValue::Ref(SecretRef {
            env: Some("TRACE_ID".into()),
            ..Default::default()
        }))
    );

    let remote = got.iter().find(|server| server.name == "remote").unwrap();
    assert_eq!(remote.request_timeout_secs, Some(90));
}
#[test]
fn codex_parser_rejects_startup_timeout_and_unknown_nested_env_fields() {
    let text = r#"
[mcp_servers.good]
command = "good-server"

[mcp_servers.startup-timeout]
url = "https://mcp.example.test/rpc"
startup_timeout_sec = 20

[mcp_servers.unknown-env-field]
command = "server"
env_vars = [{ name = "TOKEN", source = "local", typo = "must-not-disappear" }]
"#;

    let report = parse_codex_mcp_toml_for_import(text).unwrap();
    assert_eq!(
        report
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["good"]
    );
    assert_eq!(report.rejected.len(), 2);
    assert!(report
        .rejected
        .iter()
        .all(|entry| entry.issue == McpImportIssue::UnsupportedSemantics));
}
#[test]
fn codex_parser_rejects_remote_or_invalid_environment_references() {
    let text = r#"
[mcp_servers.good]
command = "good-server"
env_vars = ["LOCAL_TOKEN"]

[mcp_servers.remote-source]
command = "server"
env_vars = [{ name = "REMOTE_TOKEN", source = "remote" }]

[mcp_servers.unknown-source]
command = "server"
env_vars = [{ name = "TOKEN", source = "elsewhere" }]

[mcp_servers.invalid-name]
command = "server"
env_vars = ["9TOKEN"]

[mcp_servers.http-env-vars]
url = "https://mcp.example.test/rpc"
env_vars = ["TOKEN"]
"#;

    let names: Vec<String> = parse_codex_mcp_toml(text)
        .into_iter()
        .map(|server| server.name)
        .collect();
    assert_eq!(names, ["good"]);
}
#[test]
fn codex_parser_drops_ambiguous_or_unsafe_transport_shapes() {
    let text = r#"
[mcp_servers.good]
url = "https://mcp.example.test/good"

[mcp_servers.both]
url = "https://mcp.example.test/both"
command = "server"

[mcp_servers.http-with-args]
url = "https://mcp.example.test/args"
args = []

[mcp_servers.http-with-env]
url = "https://mcp.example.test/env"
env = {}

[mcp_servers.stdio-with-bearer]
command = "server"
bearer_token_env_var = "TOKEN"

[mcp_servers.stdio-with-headers]
command = "server"
http_headers = {}

[mcp_servers.args-only]
args = ["server"]

[mcp_servers.empty-url]
url = "  "

[mcp_servers.empty-command]
command = ""
"#;

    let names: Vec<String> = parse_codex_mcp_toml(text)
        .into_iter()
        .map(|server| server.name)
        .collect();
    assert_eq!(names, ["good"]);
}
#[test]
fn codex_parser_rejects_invalid_or_conflicting_credential_references() {
    let text = r#"
[mcp_servers.good]
url = "https://mcp.example.test/good"
env_http_headers = { X-API-Key = "API_KEY" }

[mcp_servers.bad-bearer-env]
url = "https://mcp.example.test/bearer"
bearer_token_env_var = "TOKEN}${cmd:bad}"

[mcp_servers.bad-header-env]
url = "https://mcp.example.test/header-env"
env_http_headers = { X-Key = "9INVALID" }

[mcp_servers.bad-header-name]
url = "https://mcp.example.test/header-name"
env_http_headers = { "Bad Header" = "TOKEN" }

[mcp_servers.duplicate-header-case]
url = "https://mcp.example.test/duplicate"
env_http_headers = { X-Key = "ONE", x-key = "TWO" }

[mcp_servers.conflicting-authorization]
url = "https://mcp.example.test/auth"
bearer_token_env_var = "TOKEN"
env_http_headers = { authorization = "OTHER_TOKEN" }

[mcp_servers.owned-host]
url = "https://mcp.example.test/host"
env_http_headers = { hOsT = "HOST_OVERRIDE" }

[mcp_servers.owned-session]
url = "https://mcp.example.test/session"
env_http_headers = { MCP-Session-ID = "SESSION_ID" }

[mcp_servers.owned-protocol]
url = "https://mcp.example.test/protocol"
env_http_headers = { mcp-protocol-version = "PROTOCOL_VERSION" }
"#;

    let names: Vec<String> = parse_codex_mcp_toml(text)
        .into_iter()
        .map(|server| server.name)
        .collect();
    assert_eq!(names, ["good"]);
}
#[test]
fn codex_parser_rejects_unsupported_widening_and_misspelled_fields() {
    let text = r#"
[mcp_servers.good]
command = "good-server"

[mcp_servers.enabled-tools]
command = "server"
enabled_tools = ["read"]

[mcp_servers.disabled-tools]
command = "server"
disabled_tools = ["write"]

[mcp_servers.required]
command = "server"
required = true

[mcp_servers.cwd]
command = "server"
cwd = "/tmp"

[mcp_servers.explicit-type]
url = "https://mcp.example.test/type"
type = "http"

[mcp_servers.chatgpt-auth]
url = "https://mcp.example.test/chatgpt"
auth = "chatgpt"

[mcp_servers.oauth]
url = "https://mcp.example.test/oauth"
oauth_client_id = "client"

[mcp_servers.oauth-resource]
url = "https://mcp.example.test/oauth-resource"
oauth_resource = "https://resource.example.test"

[mcp_servers.wrong-case]
url = "https://mcp.example.test/case"
bearerTokenEnvVar = "TOKEN"
"#;

    let names: Vec<String> = parse_codex_mcp_toml(text)
        .into_iter()
        .map(|server| server.name)
        .collect();
    assert_eq!(names, ["good"]);
}
#[test]
fn codex_parser_drops_only_the_malformed_entry() {
    let text = r#"
[mcp_servers.before]
command = "before"

[mcp_servers.bad]
command = "bad"
args = "not-an-array"

[mcp_servers.after]
url = "https://mcp.example.test/after"
"#;

    let names: std::collections::BTreeSet<String> = parse_codex_mcp_toml(text)
        .into_iter()
        .map(|server| server.name)
        .collect();
    assert_eq!(
        names,
        std::collections::BTreeSet::from(["after".to_string(), "before".to_string()])
    );
    assert!(parse_codex_mcp_toml("not = = toml [").is_empty());
    assert!(parse_codex_mcp_toml("other = 1").is_empty());
    assert!(parse_codex_mcp_toml("mcp_servers = []").is_empty());
}

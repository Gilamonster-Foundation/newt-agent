use super::*;

#[test]
fn management_login_argv_round_trips_only_from_newt_configuration() {
    let text = "[[mcp_servers]]\nname = 'documents'\ncommand = 'document-server'\nlogin_argv = ['document-client', 'login', 'literal;argument']\n";
    let native = parse_newt_mcp_toml(text);
    let serialized = serde_json::to_value(&native[0]).unwrap();
    assert_eq!(
        serialized["login_argv"],
        serde_json::json!(["document-client", "login", "literal;argument"])
    );
    let persisted = crate::Config::with_mcp_server_added("", &native[0]).unwrap();
    let round_trip: crate::Config = toml::from_str(&persisted).unwrap();
    assert_eq!(
        serde_json::to_value(&round_trip.mcp_servers[0]).unwrap()["login_argv"],
        serialized["login_argv"]
    );
    let borrowed = parse_claude_mcp(
        &serde_json::json!({"mcpServers": {"documents": {"command": "document-server", "login_argv": ["untrusted-login"]}}}),
    );
    let serialized = serde_json::to_value(&borrowed[0]).unwrap();
    assert!(
        serialized.get("login_argv").is_none(),
        "borrowed metadata cannot configure a host login command"
    );
}

#[test]
fn management_login_argv_requires_an_enabled_trusted_stdio_entry() {
    let mut entry: McpServerEntry = toml::from_str("name = 'documents'\ncommand = 'document-server'\nlogin_argv = ['document-client', 'login']\n").unwrap();
    assert_eq!(
        entry.operator_login_argv(),
        Some(["document-client".to_owned(), "login".to_owned()].as_slice())
    );
    entry.trust = McpTrust::Untrusted;
    assert!(entry.operator_login_argv().is_none());
    entry.trust = McpTrust::Trusted;
    entry.enabled = false;
    assert!(entry.operator_login_argv().is_none());
    entry.enabled = true;
    entry.transport = TransportKind::Http;
    assert!(entry.operator_login_argv().is_none());
}

// ---- ~/.newt/mcp.toml source: parse + precedence ----
#[test]
fn parse_newt_mcp_toml_reads_servers_and_tolerates_garbage() {
    let text = r#"
[[mcp_servers]]
name = "a"
command = "a-mcp"

[[mcp_servers]]
name = "b"
type = "http"
url = "https://x/mcp"
"#;
    let got = parse_newt_mcp_toml(text);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].name, "a");
    assert_eq!(got[0].command.as_deref(), Some("a-mcp"));
    assert_eq!(got[1].transport, TransportKind::Http);
    // Malformed TOML → empty (non-fatal), missing section → empty.
    assert!(parse_newt_mcp_toml("not = = toml [").is_empty());
    assert!(parse_newt_mcp_toml("other = 1").is_empty());
}
#[test]
fn parse_newt_mcp_toml_reads_static_oauth_client_id() {
    let text = r#"
[[mcp_servers]]
name = "example"
type = "http"
url = "https://mcp.example.com/mcp"
oauth_client_id = "public-client-abc123"
oauth_scopes = "files:read"
oauth_callback_port = 8080
"#;
    let got = parse_newt_mcp_toml(text);
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].oauth_client_id.as_deref(),
        Some("public-client-abc123")
    );
    assert_eq!(got[0].oauth_scopes.as_deref(), Some("files:read"));
    assert_eq!(got[0].oauth_callback_port, Some(8080));
}
#[test]
fn parse_newt_mcp_toml_reads_secret_refs_and_literals() {
    let text = r#"
[[mcp_servers]]
name = "gh"
command = "gh-mcp"
[mcp_servers.env]
GH_TOKEN = { cmd = "vault kv get -field=token secret/gh" }
RUST_LOG = "debug"
"#;
    let got = parse_newt_mcp_toml(text);
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].env.get("RUST_LOG"),
        Some(&SecretValue::literal("debug"))
    );
    assert!(matches!(
        got[0].env.get("GH_TOKEN"),
        Some(SecretValue::Ref(SecretRef { cmd: Some(_), .. }))
    ));
}

#[test]
fn checked_newt_mcp_parser_reports_malformed_toml_without_source_text() {
    let error = parse_newt_mcp_toml_checked(
        "[[mcp_servers]]\nname = \"private-token-marker\"\ncommand = =\n",
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid MCP TOML syntax at line 3, column 11"
    );
    assert!(!error.to_string().contains("private-token-marker"));
    assert!(parse_newt_mcp_toml_checked("other = 1").unwrap().is_empty());
}

#[test]
fn checked_newt_mcp_parser_tells_a_shape_error_from_a_syntax_error() {
    // #3062 review (F13): valid TOML whose entry does not fit the MCP schema
    // is not a syntax error. Both say where, never what: the parser's own
    // message quotes the line, here a credential.
    let error = parse_newt_mcp_toml_checked(
        "[[mcp_servers]]\nname = \"docs\"\ncommand = \"docs-mcp\"\nheaders = \"SYNTHETIC_SECRET_MARKER\"\n",
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "invalid MCP config at line 4, column 11");
}

#[test]
fn an_unloadable_newt_mcp_file_is_a_diagnostic_not_an_empty_list() {
    // #2915 review: discovery used to turn a malformed `mcp.toml` into no
    // servers. A missing file is still empty; anything else is named, without
    // quoting the line (here, a credential) the parser stopped on.
    let path = Path::new("cfg/mcp.toml");
    let malformed = "[[mcp_servers]]\nname = \"docs\"\nheaders = { Authorization = \"SYNTHETIC_SECRET_MARKER\" \n";
    let diagnostic = newt_mcp_file_servers(path, Ok(malformed.into())).unwrap_err();
    assert!(diagnostic.contains("mcp.toml"), "{diagnostic}");
    assert!(
        diagnostic.contains("invalid MCP TOML syntax"),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("SYNTHETIC_SECRET_MARKER"),
        "{diagnostic}"
    );

    let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
    assert!(newt_mcp_file_servers(path, Err(missing))
        .unwrap()
        .is_empty());
    let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    assert!(newt_mcp_file_servers(path, Err(denied)).is_err());
}

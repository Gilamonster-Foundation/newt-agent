use super::*;

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

//! Tests for [`super::print_next_steps`] (#2913) — import's next-step text must
//! point HTTP servers at `newt auth`, not a re-import or a credential-free probe.

use super::*;

fn http_entry(name: &str) -> McpServerEntry {
    McpServerEntry {
        name: name.into(),
        enabled: true,
        transport: TransportKind::Http,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some("https://example.test/mcp".into()),
        headers: BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: McpTrust::Trusted,
        origin: None,
    }
}

#[test]
fn print_next_steps_http_points_at_newt_auth() {
    let mut out = Vec::new();
    print_next_steps(&mut out, &[http_entry("h")]).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("newt auth"), "{text}");
    assert!(!text.contains("re-probe"), "{text}");
    assert!(!text.contains("re-import"), "{text}");
}

#[test]
fn print_next_steps_stdio_uses_doctor() {
    let mut out = Vec::new();
    print_next_steps(&mut out, &[stdio_entry("s", Some("scrybe-mcp-server"))]).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("newt doctor"), "{text}");
}

#[test]
fn print_next_steps_mixed_entry_points_at_auth() {
    let mut out = Vec::new();
    print_next_steps(
        &mut out,
        &[stdio_entry("s", Some("scrybe-mcp-server")), http_entry("h")],
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("newt auth"), "{text}");
}

/// #2923 review: a stdio server has no OAuth, so its per-server line must not
/// tell the operator to run `newt auth <name>`. RED before: the Configured
/// arm advised `newt auth s` for the stdio entry.
#[test]
fn print_next_steps_never_offers_auth_to_a_stdio_server() {
    let mut out = Vec::new();
    print_next_steps(&mut out, &[stdio_entry("s", Some("scrybe-mcp-server"))]).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("newt auth"), "{text}");
}

/// #2923 review round 5: a disabled stdio server is not admitted, and its
/// line must not send the operator to `newt auth` either. RED before: "not
/// admitted (…); import/approve it before `newt auth`".
#[test]
fn print_next_steps_never_offers_auth_to_a_disabled_stdio_server() {
    let mut entry = stdio_entry("s", Some("scrybe-mcp-server"));
    entry.enabled = false;
    let mut out = Vec::new();
    print_next_steps(&mut out, &[entry]).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("not admitted"), "{text}");
    assert!(!text.contains("newt auth"), "{text}");
}

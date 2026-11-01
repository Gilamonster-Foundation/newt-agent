use super::*;

#[test]
fn canonical_http_url_rejects_credential_components_and_normalizes_hosts() {
    for raw in [
        "https://user:secret@example.test/mcp",
        "https://@example.test/mcp",
        "https://example.test/mcp?auth=secret",
        "https://example.test/mcp#secret",
    ] {
        assert!(canonical_mcp_http_url(raw).is_err());
    }

    let ipv6 = canonical_mcp_http_url("https://[2001:DB8::1]:8443/mcp").unwrap();
    assert_eq!(ipv6.host, "2001:db8::1");
    assert_eq!(ipv6.url, "https://[2001:db8::1]:8443/mcp");

    let idna = canonical_mcp_http_url("https://BÜCHER.example/mcp").unwrap();
    assert_eq!(idna.host, "xn--bcher-kva.example");
    assert_eq!(idna.url, "https://xn--bcher-kva.example/mcp");
}

#[test]
fn endpoint_change_is_canonical_and_counts_a_resource_path_change() {
    // #2915: a respelling is not a move; `/mcp` → `/mcp/` is, because a saved
    // login is bound to the exact resource.
    let changed = |a, b| mcp_endpoint_changed(a, b);
    assert!(!changed(
        Some("https://API.example.test/mcp"),
        Some("https://api.example.test/mcp")
    ));
    assert!(changed(
        Some("https://api.example.test/mcp"),
        Some("https://api.example.test/mcp/")
    ));
    assert!(changed(None, Some("https://api.example.test/mcp")));
}

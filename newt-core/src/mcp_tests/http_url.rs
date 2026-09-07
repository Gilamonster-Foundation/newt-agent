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

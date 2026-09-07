use super::*;

#[test]
fn exact_http_hosts_are_normalized_and_deduplicated() {
    let entries = [
        McpServerEntry {
            name: "one".into(),
            enabled: true,
            transport: TransportKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some("https://BROKER.Example.test:8443/mcp".into()),
            headers: BTreeMap::new(),
            request_timeout_secs: None,
            trust: McpTrust::Trusted,
        },
        McpServerEntry {
            name: "two".into(),
            url: Some("https://broker.example.test/other".into()),
            ..stdio_entry("two", None)
        },
        McpServerEntry {
            name: "disabled".into(),
            enabled: false,
            transport: TransportKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some("https://disabled.example.test/mcp".into()),
            headers: BTreeMap::new(),
            request_timeout_secs: None,
            trust: McpTrust::Trusted,
        },
    ];
    let mut entries = entries;
    entries[1].transport = TransportKind::Http;
    assert_eq!(
        imported_http_hosts(&entries).unwrap(),
        vec!["broker.example.test"]
    );
}

#[test]
fn http_url_is_canonicalized_once_for_persistence_and_grants() {
    let mut entry = McpServerEntry {
        name: "one".into(),
        enabled: true,
        transport: TransportKind::Http,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some("https://BÜCHER.example:443/mcp".into()),
        headers: BTreeMap::new(),
        request_timeout_secs: None,
        trust: McpTrust::Trusted,
    };
    let host = canonicalize_import_http_url(&mut entry).unwrap().unwrap();
    assert_eq!(host, "xn--bcher-kva.example");
    assert_eq!(
        entry.url.as_deref(),
        Some("https://xn--bcher-kva.example/mcp")
    );
    assert_eq!(imported_http_hosts(&[entry]).unwrap(), [host]);
}

#[test]
fn import_url_validation_is_independent_of_network_grants_and_rejects_fragments() {
    for url in [
        "ftp://example.test/mcp",
        "https://user:never-echo-this@example.test/mcp",
        "https://example.test/mcp?auth=never-echo-this",
        "https://example.test/mcp#access_token=never-echo-this",
    ] {
        let entry = McpServerEntry {
            name: "review".into(),
            enabled: true,
            transport: TransportKind::Http,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: Some(url.into()),
            headers: BTreeMap::new(),
            request_timeout_secs: None,
            trust: McpTrust::Untrusted,
        };
        let mut entry = entry;
        let error = canonicalize_import_http_url(&mut entry).unwrap_err();
        assert!(!error.to_string().contains("never-echo-this"));
    }
}

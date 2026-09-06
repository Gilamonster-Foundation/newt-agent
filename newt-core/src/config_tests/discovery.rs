use super::*;

// Host discovery defaults and DGX node configuration/loading.

#[test]
fn discovery_defaults_cover_localhost_unboxing() {
    // #1130: absent [discovery] seeds the localhost sweep — ollama's port
    // plus the vLLM range (several ports = several one-model instances).
    let cfg: Config = toml::from_str("").unwrap();
    assert_eq!(cfg.discovery.hosts, vec!["localhost".to_string()]);
    assert_eq!(cfg.discovery.ollama_ports, vec![11434]);
    assert_eq!(cfg.discovery.vllm_ports, vec![8000, 8080, 8001, 8002, 8003]);
    assert_eq!(cfg.default_backend, None);

    // Declared values override wholesale (no merge magic).
    let cfg: Config = toml::from_str(
            "default_backend=\"dgx1-vllm\"\n[discovery]\nhosts=[\"localhost\",\"dgx1\"]\nvllm_ports=[8000]\n",
        )
        .unwrap();
    assert_eq!(cfg.default_backend.as_deref(), Some("dgx1-vllm"));
    assert_eq!(cfg.discovery.hosts.len(), 2);
    assert_eq!(cfg.discovery.vllm_ports, vec![8000]);
    // Unlisted keys keep their defaults ([serde(default)] per-field).
    assert_eq!(cfg.discovery.ollama_ports, vec![11434]);
}

#[test]
fn disk_dgx_nodes_load_per_file_by_stem_and_override_inline() {
    let dir = tempfile::tempdir().unwrap();
    // A minimal drop-in: name omitted (filename is authoritative), carries
    // the multi-endpoint info a [[backends]] entry can't (vllm + ssh_host).
    std::fs::write(
        dir.path().join("dgx1.toml"),
        "ollama = \"http://REDACTED-HOST:11434\"\n\
             vllm = \"http://REDACTED-HOST:8000\"\n\
             ssh_host = \"REDACTED-HOST\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("README.md"), "not a node").unwrap();

    // [dgx] absent → created on first drop-in, with the node populated.
    let mut cfg = Config::default();
    assert!(cfg.dgx.is_none());
    cfg.merge_dgx_nodes_from_dir(dir.path());
    let dgx = cfg.dgx.as_ref().expect("[dgx] created from drop-ins");
    assert_eq!(dgx.nodes.len(), 1);
    let node = &dgx.nodes[0];
    assert_eq!(node.name, "dgx1", "name comes from the filename stem");
    assert_eq!(node.ollama.as_deref(), Some("http://REDACTED-HOST:11434"));
    assert_eq!(node.vllm.as_deref(), Some("http://REDACTED-HOST:8000"));
    assert_eq!(node.ssh_host.as_deref(), Some("REDACTED-HOST"));
    // A single node resolves as active without an explicit active_node.
    assert_eq!(dgx.active_node().unwrap().name, "dgx1");

    // Disk replaces an inline node of the same name in place (no duplicate).
    cfg.dgx.as_mut().unwrap().nodes[0].ollama = Some("http://stale:1".into());
    cfg.merge_dgx_nodes_from_dir(dir.path());
    assert_eq!(cfg.dgx.as_ref().unwrap().nodes.len(), 1, "no duplicate");
    assert_eq!(
        cfg.dgx.unwrap().nodes[0].ollama.as_deref(),
        Some("http://REDACTED-HOST:11434"),
        "disk wins"
    );
}

#[test]
fn config_default_has_no_dgx() {
    assert!(Config::default().dgx.is_none());
}

#[test]
fn config_with_dgx_roundtrips() {
    let cfg = Config {
        dgx: Some(crate::dgx::DgxConfig::home_template()),
        ..Config::default()
    };
    let text = toml::to_string_pretty(&cfg).unwrap();
    let back = toml::from_str::<Config>(&text).unwrap();
    let dgx = back.dgx.expect("dgx should round-trip");
    assert_eq!(dgx.active_node.as_deref(), Some("home"));
    assert_eq!(dgx.nodes.len(), 1);
    assert_eq!(dgx.formations.len(), 2);
}

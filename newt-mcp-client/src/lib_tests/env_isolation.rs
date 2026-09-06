use super::*;
use newt_core::mcp::{McpServerEntry, TransportKind};

// A real subprocess is the ONLY way to observe env leakage (this is the
// security boundary, not mockable logic) — kept out of the mocked unit
// tier by #[ignore]; run explicitly / on the integration lane.
#[tokio::test]
#[ignore = "spawns a real `sh` subprocess (integration tier)"]
async fn stdio_spawn_does_not_leak_secret_env() {
    // A secret in newt's environment must NOT reach the child.
    std::env::set_var("LEAKY_SECRET_TOKEN", "sk-should-not-appear");
    let entry = McpServerEntry {
        name: "envprobe".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("sh".into()),
        args: vec!["-c".into(), "env; sleep 0.1".into()],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
        request_timeout_secs: None,
        trust: newt_core::mcp::McpTrust::Trusted,
    };
    // top() = advisory leash: `sh` is permitted (exec unrestricted) and the
    // env is still scrubbed to the explicit grants, so this validates the
    // confined path's env isolation without a fail-closed on a restricted axis.
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry admits");
    let mut t = StdioTransport::spawn(&admitted, &Caveats::top()).expect("spawn");
    let mut leaked = false;
    let mut saw_path = false;
    while let Ok(Some(line)) = t.stdout.next_line().await {
        if line.starts_with("LEAKY_SECRET_TOKEN=") {
            leaked = true;
        }
        if line.starts_with("PATH=") {
            saw_path = true;
        }
    }
    assert!(
        !leaked,
        "secret env leaked into the stdio MCP subprocess (#1155)"
    );
    assert!(saw_path, "PATH should be passed so the child can exec");
}

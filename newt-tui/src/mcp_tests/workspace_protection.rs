//! Reconnect retains the startup resource guard before exposing server tools.

use super::*;

#[tokio::test]
async fn reconnect_preserves_the_sessions_operator_resource_protection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let operator = root.join("operator");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&operator).unwrap();
    let command = operator.join("private-server");
    std::fs::write(&command, b"private executable fixture").unwrap();
    let protection = newt_core::workspace_protection::WorkspaceProtection::new(
        std::slice::from_ref(&operator),
        std::slice::from_ref(&operator),
    )
    .unwrap();
    let mut mcp = Mcp::empty();
    mcp.protection = Some(std::sync::Arc::new(protection));
    let mut caveats = newt_core::confined_exec::workspace_confined_caveats(&workspace);
    caveats.exec = newt_core::Scope::none();
    caveats.net = newt_core::Scope::All;
    let entry = McpServerEntry {
        name: "protected-reconnect".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some(command.to_string_lossy().into_owned()),
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        trust: newt_core::mcp::McpTrust::Trusted,
        origin: None,
    };
    let error = mcp
        .reconnect(&entry, &caveats, &[], &[], None)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("workspace-protected"),
        "must fail at protected admission before attempted process creation: {error:#}"
    );
    assert!(mcp.is_empty());
    assert!(
        matches!(mcp.statuses.as_slice(), [(name, McpStatus::Skipped(_))] if name == "protected-reconnect")
    );
    assert_eq!(
        std::fs::read(command).unwrap(),
        b"private executable fixture"
    );
}

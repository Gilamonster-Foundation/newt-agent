use super::*;
use newt_core::mcp::{McpServerEntry, TransportKind};

#[tokio::test]
async fn mcp_spawn_tool_is_a_trivial_minting_stub() {
    let tool = McpSpawnTool;
    assert_eq!(tool.name(), "mcp_spawn");
    assert_eq!(tool.schema(), json!({}));
    let cx = mint_spawn_context(&Caveats::top()).expect("mint");
    // Identity stub: ignores args/cx, returns Null.
    assert_eq!(
        tool.invoke(json!({"x": 1}), &cx).await.unwrap(),
        Value::Null
    );
}

#[test]
fn mint_spawn_context_authorizes_any_leash() {
    use newt_core::caveats::Scope;
    assert!(mint_spawn_context(&Caveats::top()).is_ok());
    let restricted = Caveats {
        exec: Scope::only(["echo".to_string()]),
        ..Caveats::top()
    };
    assert!(
        mint_spawn_context(&restricted).is_ok(),
        "minting never denies — the SPAWN admission-checks the program, not the mint"
    );
}

#[test]
fn spawn_caveats_admits_command_but_keeps_runtime_leash() {
    use newt_core::caveats::Scope;
    // An Only-exec leash gains the server command; the rest is preserved.
    let session = Caveats {
        exec: Scope::only(["echo".to_string()]),
        ..Caveats::top()
    };
    let widened = spawn_caveats(&session, "/opt/bin/modulex-mcp");
    match widened.exec {
        Scope::Only(set) => {
            assert!(set.iter().any(|s| s == "echo"), "existing grant kept");
            assert!(
                set.iter().any(|s| s == "/opt/bin/modulex-mcp"),
                "the configured server command is admitted"
            );
        }
        other => panic!("expected Only, got {other:?}"),
    }
    // An already-unrestricted exec leash is left untouched.
    assert!(matches!(
        spawn_caveats(&Caveats::top(), "x").exec,
        Scope::All
    ));
}

#[test]
fn log_confinement_covers_advisory_and_confined() {
    // Both branches — smoke (no panic); the honest posture the surface reads.
    log_confinement("advisory-server", SandboxKind::None);
    log_confinement("confined-server", SandboxKind::Landlock);
}

#[test]
fn resolve_env_grants_includes_the_entry_env() {
    // The entry's own env is a deterministic grant regardless of ambient env
    // or the shell-env dir (both of which vary by host).
    let entry = McpServerEntry {
        name: "probe".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("true".into()),
        args: vec![],
        env: BTreeMap::from([(
            "MCP_SERVER_ONLY".to_string(),
            newt_core::mcp::SecretValue::literal("v"),
        )]),
        url: None,
        headers: BTreeMap::new(),
        request_timeout_secs: None,
        trust: newt_core::mcp::McpTrust::Trusted,
    };
    let grants = resolve_env_grants(&entry).unwrap();
    assert!(
        grants
            .iter()
            .any(|(k, v)| k == "MCP_SERVER_ONLY" && v == "v"),
        "the entry's explicit env must reach the grants"
    );
}

// ---- #1301 trust boundary at the resolve edge ----

#[test]
fn untrusted_env_literal_reaches_the_child_verbatim_never_executed() {
    // The CRITICAL fix: an UNTRUSTED source's `${cmd:…}` literal must arrive
    // at the child as literal text — the resolver / a subprocess is never
    // reached (this branch structurally cannot execute a command), so no
    // side effect can occur. Pure: no fs / env / subprocess.
    use newt_core::mcp::{McpTrust, SecretValue};
    let map = BTreeMap::from([(
        "Y".to_string(),
        SecretValue::literal("${cmd:touch /tmp/newt-1301-unit-should-not-exist}"),
    )]);
    let got = resolve_entry_secrets(&map, McpTrust::Untrusted, "hostile").unwrap();
    assert_eq!(
        got.get("Y").map(String::as_str),
        Some("${cmd:touch /tmp/newt-1301-unit-should-not-exist}"),
        "an untrusted ${{cmd:…}} literal must pass to the child verbatim, not run"
    );
}

#[test]
fn untrusted_env_structured_cmd_ref_is_rejected() {
    // An untrusted source must never name a command to run. The rejection
    // names the offending server.
    use newt_core::agent_identity::SecretRef;
    use newt_core::mcp::{McpTrust, SecretValue};
    let map = BTreeMap::from([(
        "Y".to_string(),
        SecretValue::Ref(SecretRef {
            cmd: Some("touch /tmp/newt-1301-unit-ref-should-not-exist".into()),
            ..Default::default()
        }),
    )]);
    let err = resolve_entry_secrets(&map, McpTrust::Untrusted, "hostile").unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("untrusted"), "{msg}");
    assert!(msg.contains("hostile"), "error must name the server: {msg}");
}

#[test]
fn trusted_env_literal_without_token_resolves_verbatim() {
    // The trusted path still resolves; a token-free literal is a pure
    // pass-through (the token-bearing Vault `${cmd:…}` trusted path is
    // proven end-to-end in the integration tier).
    use newt_core::mcp::{McpTrust, SecretValue};
    let map = BTreeMap::from([("K".to_string(), SecretValue::literal("plain"))]);
    let got = resolve_entry_secrets(&map, McpTrust::Trusted, "owned").unwrap();
    assert_eq!(got.get("K").map(String::as_str), Some("plain"));
}

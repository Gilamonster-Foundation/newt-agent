use super::*;

#[test]
fn admit_denies_untrusted_and_disabled_admits_trusted() {
    // step-1.1: enabled != trusted != approved, decided at ONE gate.
    // A trusted, enabled server is admitted (its witness carries the entry).
    let trusted = stdio("trusted", "/bin/echo");
    let ok = admit(&trusted).unwrap();
    assert_eq!(ok.entry().name, "trusted");

    // A discovered (untrusted) STDIO overlay is refused — it may not spawn
    // without approval outside the repo; headless has no interactive path.
    let untrusted = McpServerEntry {
        trust: McpTrust::Untrusted,
        ..stdio("evil", "/bin/sh")
    };
    assert!(matches!(
        admit(&untrusted),
        Err(AdmissionDenied::UntrustedNotApproved { .. })
    ));

    // Transport-agnostic: an untrusted HTTP overlay is refused too.
    let untrusted_http = McpServerEntry {
        trust: McpTrust::Untrusted,
        transport: TransportKind::Http,
        command: None,
        url: Some("https://evil.example".into()),
        ..stdio("evil-http", "")
    };
    assert!(matches!(
        admit(&untrusted_http),
        Err(AdmissionDenied::UntrustedNotApproved { .. })
    ));

    // A disabled entry is never admitted, regardless of trust (enabled is a
    // visibility switch, not a trust decision).
    let disabled = McpServerEntry {
        enabled: false,
        ..stdio("off", "/bin/echo")
    };
    assert!(matches!(admit(&disabled), Err(AdmissionDenied::Disabled)));
}

/// #2484: the refusal names the server and prints the exact command for WHERE it
/// came from, rendered by the ONE `Display` (no per-caller string building).
fn untrusted_from(name: &str, origin: Option<McpOrigin>) -> String {
    let entry = McpServerEntry {
        trust: McpTrust::Untrusted,
        origin,
        ..stdio(name, "/bin/sh")
    };
    admit(&entry).unwrap_err().to_string()
}

#[test]
fn a_claude_user_server_is_refused_with_the_from_claude_command() {
    let msg = untrusted_from("gila-canvas", Some(McpOrigin::ClaudeUser));
    assert!(
        msg.contains("`newt mcp import --from-claude --name gila-canvas`"),
        "{msg}"
    );
}

#[test]
fn a_project_file_server_is_refused_with_the_quoted_path_command() {
    let msg = untrusted_from(
        "proj-srv",
        Some(McpOrigin::File(PathBuf::from("/work/my proj/.mcp.json"))),
    );
    assert!(
        msg.contains("`newt mcp import '/work/my proj/.mcp.json' --name proj-srv`"),
        "{msg}"
    );
    let plain = untrusted_from("s", Some(McpOrigin::File(PathBuf::from("/w/.mcp.json"))));
    assert!(
        plain.contains("`newt mcp import /w/.mcp.json --name s`"),
        "{plain}"
    );
}

#[test]
fn an_unimportable_server_name_is_escaped_and_never_placed_in_a_command() {
    for name in ["x; rm -rf ~", "\u{1b}[31m", "a__b"] {
        let msg = untrusted_from(name, Some(McpOrigin::ClaudeUser));
        assert!(msg.contains("cannot be imported under that name"), "{msg}");
        assert!(!msg.contains("--name"), "no command for {name:?}: {msg}");
        assert!(!msg.contains('\u{1b}'), "raw control byte leaked: {msg:?}");
        assert!(
            msg.contains(&format!("{name:?}")),
            "escaped name shown: {msg}"
        );
    }
}

#[test]
fn an_untracked_origin_names_the_server_without_inventing_a_command() {
    let msg = untrusted_from("orphan", None);
    assert!(msg.contains("\"orphan\""), "{msg}");
    assert!(!msg.contains("--name"), "{msg}");
}

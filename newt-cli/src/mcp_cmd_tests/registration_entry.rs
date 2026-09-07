use super::*;

#[test]
fn mcp_add_parses_repeatable_args_and_env() {
    let cli = crate::Cli::try_parse_from([
        "newt",
        "mcp",
        "add",
        "scrybe",
        "--command",
        "scrybe-mcp-server",
        "--arg",
        "stdio",
        "--arg",
        "--verbose",
        "--env",
        "A=1",
        "--env",
        "B=2",
        "--timeout-secs",
        "90",
        "--project",
    ])
    .unwrap();
    let Some(crate::Command::Mcp {
        cmd:
            Some(McpCmd::Add {
                name,
                command,
                args,
                transport,
                url,
                env,
                timeout_secs,
                project,
            }),
    }) = cli.command
    else {
        panic!("expected mcp add");
    };
    assert_eq!(name, "scrybe");
    assert_eq!(command.as_deref(), Some("scrybe-mcp-server"));
    assert_eq!(args, vec!["stdio", "--verbose"]);
    assert_eq!(transport, TransportKind::Stdio, "stdio is the default");
    assert_eq!(url, None);
    assert_eq!(env, vec!["A=1", "B=2"]);
    assert_eq!(timeout_secs, Some(90));
    assert!(project);
}

#[test]
fn mcp_add_rejects_an_unknown_transport() {
    let err =
        crate::Cli::try_parse_from(["newt", "mcp", "add", "x", "--transport", "grpc"]).unwrap_err();
    assert!(err.to_string().contains("stdio, sse, http"), "{err}");
}

#[test]
fn build_entry_requires_the_transport_matched_endpoint() {
    // stdio without --command.
    let err = build_entry(
        "x".into(),
        None,
        vec![],
        TransportKind::Stdio,
        None,
        &[],
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--command"), "{err}");
    // sse without --url.
    let err = build_entry(
        "x".into(),
        None,
        vec![],
        TransportKind::Sse,
        None,
        &[],
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--url"), "{err}");
    // --url on a stdio server is a mistake, not silently dropped noise.
    let err = build_entry(
        "x".into(),
        Some("cmd".into()),
        vec![],
        TransportKind::Stdio,
        Some("https://x".into()),
        &[],
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--url"), "{err}");
    // --command on an http server likewise.
    let err = build_entry(
        "x".into(),
        Some("cmd".into()),
        vec![],
        TransportKind::Http,
        Some("https://x".into()),
        &[],
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--command"), "{err}");
}

#[test]
fn build_entry_assembles_a_full_stdio_registration() {
    let entry = build_entry(
        "scrybe".into(),
        Some("scrybe-mcp-server".into()),
        vec!["stdio".into()],
        TransportKind::Stdio,
        None,
        &["SCRYBE_LOG=info".to_string()],
        Some(120),
    )
    .unwrap();
    assert_eq!(entry.name, "scrybe");
    assert!(entry.enabled);
    assert_eq!(entry.command.as_deref(), Some("scrybe-mcp-server"));
    assert_eq!(entry.args, vec!["stdio"]);
    assert_eq!(
        entry
            .env
            .get("SCRYBE_LOG")
            .and_then(SecretValue::as_literal),
        Some("info")
    );
    assert_eq!(entry.request_timeout_secs, Some(120));
    assert!(entry.is_valid());
}

#[test]
fn env_pairs_split_on_the_first_equals_and_reject_malformed() {
    let got = parse_env_pairs(&["A=1".into(), "B=x=y".into(), "EMPTY=".into()]).unwrap();
    assert_eq!(got.get("A").and_then(SecretValue::as_literal), Some("1"));
    assert_eq!(got.get("B").and_then(SecretValue::as_literal), Some("x=y"));
    assert_eq!(got.get("EMPTY").and_then(SecretValue::as_literal), Some(""));
    assert!(parse_env_pairs(&["NOEQUALS".into()]).is_err());
    assert!(parse_env_pairs(&["=value".into()]).is_err());
}

#[test]
fn mcp_add_import_parses() {
    let cli = crate::Cli::try_parse_from([
        "newt",
        "mcp",
        "import",
        "/tmp/claude.json",
        "--name",
        "review",
        "--grant-net",
        "--force",
    ])
    .unwrap();
    let Some(crate::Command::Mcp {
        cmd:
            Some(McpCmd::Import {
                path,
                from_claude,
                from_codex,
                name,
                all,
                grant_net,
                force,
                merge,
                project,
            }),
    }) = cli.command
    else {
        panic!("expected mcp import");
    };
    assert_eq!(path.as_deref(), Some(Path::new("/tmp/claude.json")));
    assert!(!from_claude);
    assert!(!from_codex);
    assert_eq!(name.as_deref(), Some("review"));
    assert!(!all);
    assert!(grant_net);
    assert!(force);
    assert!(!merge);
    assert!(!project);
    // A built-in source makes the path optional, but selection remains
    // explicit so bulk adoption is never accidental.
    assert!(crate::Cli::try_parse_from([
        "newt",
        "mcp",
        "import",
        "--from-claude",
        "--name",
        "review"
    ])
    .is_ok());
    assert!(crate::Cli::try_parse_from(["newt", "mcp", "import", "--from-codex", "--all"]).is_ok());
    assert!(crate::Cli::try_parse_from(["newt", "mcp", "import", "--from-claude"]).is_err());
    assert!(crate::Cli::try_parse_from([
        "newt",
        "mcp",
        "import",
        "--from-claude",
        "--from-codex",
        "--all"
    ])
    .is_err());
    // --force and --merge are mutually exclusive.
    assert!(crate::Cli::try_parse_from([
        "newt",
        "mcp",
        "import",
        "/tmp/c.json",
        "--all",
        "--force",
        "--merge"
    ])
    .is_err());
}

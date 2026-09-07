use super::*;

#[test]
fn binary_candidates_try_path_dirs_before_venv() {
    let cands = binary_candidates_in(
        &[PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")],
        Some(Path::new("/home/u/venv/bin")),
        "scrybe-mcp-server",
    );
    assert_eq!(
        cands,
        vec![
            PathBuf::from("/usr/local/bin/scrybe-mcp-server"),
            PathBuf::from("/usr/bin/scrybe-mcp-server"),
            PathBuf::from("/home/u/venv/bin/scrybe-mcp-server"),
        ]
    );
    // No venv → PATH candidates only.
    assert_eq!(
        binary_candidates_in(&[PathBuf::from("/bin")], None, "x"),
        vec![PathBuf::from("/bin/x")]
    );
}

#[test]
fn first_existing_returns_the_earliest_present_candidate() {
    let cands = vec![
        PathBuf::from("/a/x"),               // missing → skipped
        PathBuf::from("/b/x"),               // present → the winner
        PathBuf::from("/home/u/venv/bin/x"), // present too, but later
    ];
    let present: std::collections::BTreeSet<PathBuf> =
        [PathBuf::from("/b/x"), PathBuf::from("/home/u/venv/bin/x")]
            .into_iter()
            .collect();
    assert_eq!(
        first_existing(&cands, |p| present.contains(p)),
        Some(PathBuf::from("/b/x")),
        "PATH resolves before ~/venv/bin; earliest present wins"
    );
    // Nothing present → None (the pip-hint path).
    assert_eq!(first_existing(&cands, |_| false), None);
}

#[test]
fn finalize_install_leaves_an_explicit_path_and_non_stdio_untouched() {
    // A command that already carries a path separator is respected as-is —
    // even for the bundled scrybe entry.
    let mut abs = stdio_entry("scrybe", Some("/opt/scrybe/bin/scrybe-mcp-server"));
    finalize_install_command(&mut abs, "scrybe", true).unwrap();
    assert_eq!(
        abs.command.as_deref(),
        Some("/opt/scrybe/bin/scrybe-mcp-server")
    );
    // A non-stdio server has no binary to resolve.
    let mut http = McpServerEntry {
        transport: TransportKind::Http,
        command: None,
        url: Some("https://x/mcp".into()),
        ..stdio_entry("remote", None)
    };
    finalize_install_command(&mut http, "remote", true).unwrap();
    assert!(http.command.is_none());
}

// ---- `newt mcp import` source resolution ----

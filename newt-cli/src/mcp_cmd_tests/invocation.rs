use super::*;

// Backward compatibility: bare `newt mcp` must still parse to NO
// subcommand — the serve-over-stdio mode.
#[test]
fn bare_mcp_parses_to_serve_mode() {
    let cli = crate::Cli::try_parse_from(["newt", "mcp"]).unwrap();
    assert!(
        matches!(cli.command, Some(crate::Command::Mcp { cmd: None })),
        "bare `newt mcp` must keep serving over stdio"
    );
}

// The explicit, unambiguous manual serve verb: `newt mcp serve`.
#[test]
fn mcp_serve_parses_to_serve_variant() {
    let cli = crate::Cli::try_parse_from(["newt", "mcp", "serve"]).unwrap();
    assert!(
        matches!(
            cli.command,
            Some(crate::Command::Mcp {
                cmd: Some(McpCmd::Serve)
            })
        ),
        "`newt mcp serve` must parse to the Serve variant"
    );
}

// TTY seam: a piped stdin (an MCP client, or the stdout-purity tests)
// means SERVE — the backward-compatible path.
#[test]
fn bare_mcp_action_serves_when_stdin_is_piped() {
    assert_eq!(bare_mcp_action(false), BareMcpAction::Serve);
}

// TTY seam: an interactive human at a terminal gets the verb menu, not
// a server that blocks on stdin.
#[test]
fn bare_mcp_action_prints_help_when_stdin_is_a_terminal() {
    assert_eq!(bare_mcp_action(true), BareMcpAction::Help);
}

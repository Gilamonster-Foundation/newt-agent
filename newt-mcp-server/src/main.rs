// #1432 — the compiled half of the stdout law.
//
// stdout on this binary is a JSON-RPC wire: one stray `println!` corrupts a
// frame. `stdio_guard::redirect_stdout_to_stderr` (dup2) already catches this
// at runtime for anything that reaches fd 1, including the dep tree. This lint
// catches OUR OWN violations at compile time instead, so the runtime guard is
// the backstop rather than the only line of defence — the belt-and-suspenders
// posture codex ships (`codex-rs/exec/src/lib.rs:5`).
//
// Deliberate writes go through the private handle the guard hands back.
#![deny(clippy::print_stdout, clippy::print_stderr)]

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // Carried-coreutils dispatch (agent-bridle #206): if invoked as
    // `newt-mcp-server --invoke-bundled <name> …` (the brush engine's carried
    // coreutils shim re-execing us), run the in-process uutils coreutil and exit
    // before touching the JSON-RPC wire.
    if let Some(code) = newt_core::maybe_dispatch() {
        std::process::exit(code);
    }
    // Declare fd 1 a protocol channel before anything else can paint on it.
    // Routing tracing to stderr (below) covers logging; this covers the OTHER
    // family of stdout writers — spinners and progress readouts — which no
    // subscriber configuration can reach. Unconditional on platform.
    newt_core::tty::enter_protocol_mode();

    // Route ALL tracing output to stderr. This binary uses stdout as
    // the JSON-RPC wire — any tracing or logging on stdout would
    // corrupt the protocol. Defaulting the subscriber to stderr is
    // the cheapest insurance against a dependency emitting
    // `tracing::info!()` anywhere in the tree.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    // #1021 PR 5.3: this binary has no clap parser of its own (unlike `newt
    // mcp --persona`, which reuses newt-cli's global flag) — a minimal
    // `--persona <name>` here keeps the standalone binary (e.g. `claude mcp
    // add newt -- newt-mcp-server --persona personal-assistant`) capable of
    // the same restriction.
    let persona = parse_persona_arg(std::env::args());
    newt_mcp_server::run_stdio(persona.as_deref()).await
}

/// `--persona <name>` (or `--persona=<name>`) from an argument iterator.
/// `None` when absent or malformed (no value follows a bare `--persona`).
fn parse_persona_arg(args: impl Iterator<Item = String>) -> Option<String> {
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--persona=") {
            return Some(value.to_string());
        }
        if arg == "--persona" {
            return args.next();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_space_and_equals_forms() {
        assert_eq!(
            parse_persona_arg(
                ["newt-mcp-server", "--persona", "coach"]
                    .map(String::from)
                    .into_iter()
            ),
            Some("coach".to_string())
        );
        assert_eq!(
            parse_persona_arg(
                ["newt-mcp-server", "--persona=coach"]
                    .map(String::from)
                    .into_iter()
            ),
            Some("coach".to_string())
        );
    }

    #[test]
    fn absent_or_dangling_flag_is_none() {
        assert_eq!(
            parse_persona_arg(["newt-mcp-server"].map(String::from).into_iter()),
            None
        );
        assert_eq!(
            parse_persona_arg(
                ["newt-mcp-server", "--persona"]
                    .map(String::from)
                    .into_iter()
            ),
            None
        );
    }
}

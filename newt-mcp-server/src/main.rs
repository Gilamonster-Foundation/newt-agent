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
    newt_mcp_server::run_stdio().await
}

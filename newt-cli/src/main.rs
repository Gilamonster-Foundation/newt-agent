use anyhow::Result;

fn main() -> Result<()> {
    // Carried-coreutils dispatch (agent-bridle #206): if this process was invoked
    // as `newt --invoke-bundled <name> …` (the brush engine's carried-coreutils
    // shim re-execing us), run the in-process uutils coreutil and exit. MUST be
    // first — before clap/tracing/stack setup.
    if let Some(code) = newt_core::maybe_dispatch() {
        std::process::exit(code);
    }
    // Windows' default main-thread stack (~1 MB) overflows on the large clap
    // command tree during `Cli::parse()` before any diagnostics are printed.
    // Keep parse + dispatch behind Newt's explicit CLI stack policy. (#747)
    newt_cli::stack::run_on_cli_stack("newt-main", || {
        newt_cli::stack::build_cli_runtime()?.block_on(run())
    })
}

async fn run() -> Result<()> {
    // Route ALL tracing output to stderr. The `worker` and `mcp`
    // subcommands use stdout as the JSON-RPC wire — any tracing or
    // logging on stdout would corrupt the protocol. Defaulting the
    // subscriber to stderr is the cheapest insurance against a
    // dependency emitting `tracing::info!()` anywhere in the tree.
    // Default to `warn`: internal `info!` diagnostics (soul loaded, project
    // instructions loaded, …) are noise in the interactive scroller. Full logs
    // remain one env var away — `RUST_LOG=info` (or `=debug`) restores them.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let cli = newt_cli::parse_with_help()?;
    newt_cli::dispatch(cli).await
}

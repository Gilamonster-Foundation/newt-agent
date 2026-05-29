use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    // Route ALL tracing output to stderr. The `worker` and `mcp`
    // subcommands use stdout as the JSON-RPC wire — any tracing or
    // logging on stdout would corrupt the protocol. Defaulting the
    // subscriber to stderr is the cheapest insurance against a
    // dependency emitting `tracing::info!()` anywhere in the tree.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = newt_cli::Cli::parse();
    newt_cli::dispatch(cli).await
}

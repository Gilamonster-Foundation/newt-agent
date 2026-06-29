use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    // Windows' default main-thread stack (~1 MB) overflows on the (large) clap
    // CLI tree during `Cli::parse()` — STATUS_STACK_OVERFLOW (0xC00000FD); Linux's
    // 8 MB default hides it. Run parse + the async dispatch on a thread with an
    // explicit large stack so every platform has room. (#709)
    std::thread::Builder::new()
        .name("newt-main".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(16 * 1024 * 1024)
                .build()?
                .block_on(run())
        })
        .expect("spawn newt-main thread")
        .join()
        .expect("newt-main thread panicked")
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
    let cli = newt_cli::Cli::parse();
    newt_cli::dispatch(cli).await
}

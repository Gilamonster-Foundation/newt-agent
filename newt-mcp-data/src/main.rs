use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // NOTE: this binary does NOT call `newt_core::tty::enter_protocol_mode()`,
    // unlike every other stdio JSON-RPC server in the workspace. It has no
    // `newt-core` dependency by deliberate design (see the note in Cargo.toml:
    // "pure data — no confined shell, no inference, no leash"), and that
    // boundary is worth more than the guard: nothing here can construct an
    // ephemeral writer in the first place, since the only ones that exist live
    // in `newt-core::tty` and `newt-tui`. Revisit if this crate ever grows a
    // `newt-core` dependency for another reason.
    //
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
    newt_mcp_data::run_stdio().await
}

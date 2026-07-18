//! Newt-Agent MCP server — stdio JSON-RPC.
//!
//! v0 tool surface (vi-minimal):
//! - `code_read` — read a file
//! - `code_edit` — apply a unified diff patch
//! - `code_search` — regex search across a directory tree
//! - `goal_run` — tier-routed inference (wired through Router +
//!   BackendRegistry; tries to discover a local Ollama on startup)

use std::sync::Arc;

use newt_core::Router;
use newt_inference::BackendRegistry;

pub mod caveats;
pub mod handlers;
pub mod server;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

/// Default model name handed to the discovered Ollama backend.
///
/// A follow-up will read this from `Config` (see `newt_core::Config`)
/// so an operator can override without recompiling. For now it matches
/// the value `newt-acp-worker::run_with_io` uses, so both surfaces
/// drive the same model by default.
const DEFAULT_OLLAMA_MODEL: &str = "llama3.1:8b";

pub async fn run_stdio(persona: Option<&str>) -> anyhow::Result<()> {
    run_with_io(tokio::io::stdin(), tokio::io::stdout(), persona).await
}

/// Run the MCP server against an explicit reader/writer pair.
///
/// Used by the CLI binary's `Mcp` dispatch arm to feed a private
/// "real stdout" file handle (obtained from
/// [`newt_cli::stdio_guard::redirect_stdout_to_stderr`]) into the
/// server *after* fd 1 has been redirected to stderr. That sequence
/// is what protects the JSON-RPC wire from rogue `println!` calls in
/// dependencies.
///
/// `persona` (#1021 PR 5.3) is the already-global `--persona` CLI flag,
/// wired into this subcommand for the first time — see [`connect_persona`].
pub async fn run_with_io<R, W>(
    reader: R,
    mut writer: W,
    persona: Option<&str>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let registry = build_default_registry().await?;
    let router = Arc::new(Router::new());
    let (toolset, persona_tools) = connect_persona(persona).await?;
    let mut server = server::McpServer::new();
    handlers::register_handlers(
        &mut server,
        registry,
        router,
        Arc::new(tokio::sync::Mutex::new(toolset)),
        Arc::new(persona_tools),
    );
    server.run(reader, &mut writer).await
}

/// Resolve `persona` (#1021 PR 5.3) into its connected MCP toolset and
/// `tools:` allow-list.
///
/// `None` (no `--persona`) returns an empty toolset and no restriction —
/// today's behavior, byte-for-byte unchanged. `Some(name)` that fails to
/// resolve is a loud error, not a silent fallback: see
/// [`newt_core::RoleProfile::load_from_dir`]'s doc comment for why a
/// headless server doesn't seed a missing persona the way the TUI does.
async fn connect_persona(
    persona: Option<&str>,
) -> anyhow::Result<(newt_mcp_client::McpToolset, Option<Vec<String>>)> {
    let Some(name) = persona else {
        return Ok((newt_mcp_client::McpToolset::empty(), None));
    };
    let profile = newt_core::RoleProfile::load_from_dir(name, &newt_core::Config::personas_dir())?;
    let cfg = newt_core::Config::resolve().unwrap_or_default();
    let workspace = std::env::current_dir().unwrap_or_default();
    // #1243 Leg 3: spawned stdio servers are confined by this leash. The headless
    // server has no interactive session leash, so spawn under an advisory top()
    // for now (parity with prior behavior); wiring the persona's caveats here is
    // a follow-up.
    let toolset = newt_mcp_client::McpToolset::connect(
        &workspace.to_string_lossy(),
        &cfg.mcp_servers,
        true,
        &newt_core::caveats::Caveats::top(),
    )
    .await;
    tracing::info!(
        persona = name,
        connected_servers = toolset.summary().len(),
        "MCP server: persona activated"
    );
    Ok((toolset, profile.tools))
}

/// Build the default backend registry for the MCP server.
///
/// Tries to discover a local Ollama endpoint with the default model.
/// If discovery fails the registry stays empty — `goal_run` will then
/// surface `NoBackendForTier` as a clean JSON-RPC error rather than
/// crashing the whole server (the other tools still work fine).
async fn build_default_registry() -> anyhow::Result<Arc<BackendRegistry>> {
    let cfg = newt_core::Config::resolve().unwrap_or_default();
    let registry = registry_from_config(&cfg)?;
    if !registry.is_empty() {
        tracing::info!(
            providers = registry.len(),
            "MCP server: using configured provider plugin registry"
        );
        return Ok(Arc::new(registry));
    }

    let mut registry = BackendRegistry::new();
    match newt_inference::local::LocalOllamaBackend::discover(DEFAULT_OLLAMA_MODEL).await {
        Ok(backend) => {
            tracing::info!(
                model = DEFAULT_OLLAMA_MODEL,
                endpoint = backend.endpoint(),
                "MCP server: discovered local Ollama backend"
            );
            registry.register(Arc::new(backend));
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "MCP server: no local Ollama discovered; goal_run will fail with NoBackendForTier"
            );
        }
    }
    Ok(Arc::new(registry))
}

fn registry_from_config(cfg: &newt_core::Config) -> anyhow::Result<BackendRegistry> {
    BackendRegistry::load_from_config(cfg)
}

#[cfg(test)]
mod tests {
    use newt_core::config::ProviderConfig;
    use newt_core::{Config, Tier};

    fn provider(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            command: "newt-provider-openai".into(),
            model: Some("gpt-test".into()),
            env_pass: vec!["OPENAI_API_KEY".into()],
            tiers: vec![Tier::Complex],
        }
    }

    #[test]
    fn registry_from_config_registers_provider_plugins() {
        let cfg = Config {
            providers: vec![provider("openai-provider")],
            ..Config::default()
        };

        let registry = super::registry_from_config(&cfg).unwrap();

        assert_eq!(registry.names(), vec!["openai-provider"]);
        assert!(registry.pick(Tier::Complex).is_ok());
    }
}

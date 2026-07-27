// #1432 — the compiled half of the stdout law (see newt-mcp-server/src/main.rs).
// `newt worker` speaks JSON-RPC on stdout; a stray `println!` corrupts a frame.
// The dup2 guard in `newt-cli` catches it at runtime for the whole dep tree;
// this catches our own at compile time.
#![deny(clippy::print_stdout, clippy::print_stderr)]

//! Newt-Agent ACP worker.
//!
//! Speaks the Agent Client Protocol (agentclientprotocol.com) over stdio so
//! `drake-foreman` can dispatch coding goals to Newt instances.
//!
//! Contract (per memory `feedback_drake_patch_not_prose` and
//! `feedback_empty_diff_is_a_crash`):
//! - Worker ONLY edits files; never `git add` / `git commit` / `git push`.
//! - Empty `git diff` post-turn is a deterministic crash — foreman counts it
//!   against the model's scorecard.
//! - `TaskReply.model_id` is mandatory.

mod diff;
mod identity;
pub mod prom;
mod server;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use diff::{capture_diff, is_empty_diff};
pub use identity::{
    worker_session_caveats, IdentityError, WorkerIdentity, WORKER_TURN_CALL_BUDGET,
};
pub use prom::NewtMetrics;
pub use server::{AcpServer, Session, TaskReply};

use std::sync::Arc;

/// Spawn the default ACP worker over stdio.
///
/// Discovers a local Ollama endpoint (per `LocalOllamaBackend::discover`)
/// using the default model `llama3.1:8b` and runs the server until stdin
/// closes.
///
/// Identity is resolved from the default operator key path
/// (`~/.newt/identity.pem`, generated on first run). To opt out, use
/// [`run_with_io_metrics_and_identity`] directly with
/// [`WorkerIdentity::AllowNoKey`].
pub async fn run_stdio() -> anyhow::Result<()> {
    run_with_io(tokio::io::stdin(), tokio::io::stdout()).await
}

/// Like [`run_stdio`] but with an explicit reader/writer pair, and an optional
/// Prometheus metrics registry that will receive per-turn observations.
///
/// Used by the CLI binary's `Worker` dispatch arm to feed a private
/// "real stdout" file handle (obtained from
/// [`newt_cli::stdio_guard::redirect_stdout_to_stderr`]) into the
/// server *after* fd 1 has been redirected to stderr.
///
/// When `metrics` is `Some`, the server records timing, token counts, and
/// cost into the Prometheus counters after every `prompt` turn.
pub async fn run_with_io<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    run_with_io_and_metrics(reader, writer, None).await
}

/// Convenience entry-point: resolves the operator key from the default
/// path (no override), refuses on failure. For finer control — explicit
/// key path, env override, or the debug `--allow-no-key` escape hatch —
/// call [`run_with_io_metrics_and_identity`] directly.
pub async fn run_with_io_and_metrics<R, W>(
    reader: R,
    writer: W,
    metrics: Option<Arc<NewtMetrics>>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let identity = WorkerIdentity::resolve(None, false)
        .map_err(|e| anyhow::anyhow!("operator identity required: {e}"))?;
    run_with_io_metrics_and_identity(reader, writer, metrics, identity).await
}

/// Full entry-point: accepts explicit I/O streams, an optional metrics
/// registry, and the worker [`WorkerIdentity`] that the ACP server
/// attenuates per dispatch.
///
/// Issue #94: every `prompt` turn the ACP server dispatches under
/// `Coder::run` now derives its [`newt_core::Caveats`] from this
/// identity — never from `Caveats::top()`. The headless worker rooted
/// in a real operator key (the default) therefore enforces the same
/// attenuation-only ocap discipline the TUI already does; the
/// `--allow-no-key` debug fallback preserves pre-#94 behavior for
/// developer iteration but is never the default.
pub async fn run_with_io_metrics_and_identity<R, W>(
    reader: R,
    writer: W,
    metrics: Option<Arc<NewtMetrics>>,
    identity: WorkerIdentity,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let backend = resolve_backend().await?;
    let server = AcpServer::new(backend)
        .with_metrics(metrics)
        .with_identity(identity);
    server.run(reader, writer).await
}

/// Pick the inference backend the worker runs against.
///
/// If the resolved config (`~/.newt/config.toml` et al.) declares an
/// OpenAI-compatible backend, the first such entry is used — with bearer
/// auth resolved from its `api_key_env` / `api_key_file`. This is how
/// Newt targets a hosted OpenAI-compatible endpoint.
///
/// Otherwise the worker falls back to local Ollama auto-discovery using
/// `$NEWT_DEFAULT_MODEL` (default `llama3.1:8b`) — the historical
/// behavior, unchanged when no OpenAI backend is configured.
/// Choose the configured OpenAI-compatible backend to run against, if any.
///
/// An explicit `OLLAMA_HOST` is an unambiguous "use this Ollama" override and
/// wins over a configured OpenAI backend (explicit env > config file), so it
/// forces the Ollama path by returning `None`. This also keeps the mock-mode
/// e2e test — which points `OLLAMA_HOST` at a wiremock — hermetic against a
/// developer's real `~/.newt/config.toml`.
fn select_openai_backend(
    cfg: &newt_core::Config,
    ollama_override: bool,
) -> Option<&newt_core::BackendConfig> {
    if ollama_override {
        return None;
    }
    cfg.backends
        .iter()
        .find(|b| b.kind == Some(newt_core::BackendKind::Openai))
}

fn select_provider_config(
    cfg: &newt_core::Config,
    ollama_override: bool,
) -> Option<&newt_core::config::ProviderConfig> {
    if ollama_override {
        return None;
    }
    cfg.providers.first()
}

async fn resolve_backend() -> anyhow::Result<Arc<dyn newt_inference::InferenceBackend>> {
    use newt_core::Config;

    let ollama_override = std::env::var_os("OLLAMA_HOST").is_some();
    let cfg = Config::resolve().unwrap_or_default();
    if let Some(provider) = select_provider_config(&cfg, ollama_override) {
        tracing::info!(
            name = %provider.name,
            command = %provider.command,
            model = %provider.model.as_deref().unwrap_or("(missing)"),
            "worker: using configured provider plugin"
        );
        return Ok(Arc::new(
            newt_inference::provider_plugin::ProviderPluginBackend::from_config(provider)?,
        ));
    }
    if let Some(openai) = select_openai_backend(&cfg, ollama_override) {
        tracing::info!(
            name = %openai.name,
            endpoint = %openai.endpoint,
            model = %openai.effective_model().unwrap_or("(server decides)"),
            authenticated = openai.resolve_api_key().is_some(),
            "worker: using configured OpenAI-compatible backend"
        );
        return Ok(Arc::new(
            newt_inference::local::LocalVllmBackend::from_config(openai),
        ));
    }

    let default_model =
        std::env::var("NEWT_DEFAULT_MODEL").unwrap_or_else(|_| "llama3.1:8b".to_string());
    let backend = newt_inference::local::LocalOllamaBackend::discover(&default_model).await?;
    Ok(Arc::new(backend))
}

#[cfg(test)]
mod backend_selection_tests {
    use super::select_openai_backend;
    use newt_core::config::ProviderConfig;
    use newt_core::router::Tier;
    use newt_core::{BackendConfig, BackendKind, Config};

    fn backend(name: &str, kind: BackendKind) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            endpoint: "http://e".into(),
            model: Some("m".into()),
            model_path: None,
            tiers: vec![Tier::Fast],
            kind: Some(kind),
            api: Default::default(),
            api_key_file: None,
            api_key_env: None,
            ..Default::default()
        }
    }

    fn cfg(backends: Vec<BackendConfig>) -> Config {
        Config {
            backends,
            ..Config::default()
        }
    }

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
    fn picks_openai_backend_when_present_and_no_override() {
        let c = cfg(vec![
            backend("local", BackendKind::Ollama),
            backend("remote", BackendKind::Openai),
        ]);
        let chosen = select_openai_backend(&c, false).expect("openai backend");
        assert_eq!(chosen.name, "remote");
        assert_eq!(chosen.kind, Some(BackendKind::Openai));
    }

    #[test]
    fn ollama_host_override_forces_ollama_path_even_with_openai_config() {
        // The OLLAMA_HOST override must win over a configured OpenAI backend.
        let c = cfg(vec![backend("remote", BackendKind::Openai)]);
        assert!(select_openai_backend(&c, true).is_none());
    }

    #[test]
    fn no_openai_backend_yields_none() {
        let c = cfg(vec![backend("local", BackendKind::Ollama)]);
        assert!(select_openai_backend(&c, false).is_none());
    }

    #[test]
    fn empty_backend_list_yields_none() {
        let c = cfg(vec![]);
        assert!(select_openai_backend(&c, false).is_none());
        assert!(select_openai_backend(&c, true).is_none());
    }

    #[test]
    fn first_openai_backend_wins_when_several() {
        let mut first = backend("remote-a", BackendKind::Openai);
        first.endpoint = "http://a".into();
        let mut second = backend("remote-b", BackendKind::Openai);
        second.endpoint = "http://b".into();
        let c = cfg(vec![backend("local", BackendKind::Ollama), first, second]);
        let chosen = select_openai_backend(&c, false).expect("openai backend");
        assert_eq!(chosen.name, "remote-a");
        assert_eq!(chosen.endpoint, "http://a");
    }

    #[test]
    fn provider_config_wins_before_openai_compatible_backend() {
        let c = Config {
            providers: vec![provider("openai-provider")],
            backends: vec![backend("remote", BackendKind::Openai)],
            ..Config::default()
        };

        let chosen = super::select_provider_config(&c, false).expect("provider");

        assert_eq!(chosen.name, "openai-provider");
    }

    #[test]
    fn ollama_host_override_forces_ollama_path_even_with_provider_config() {
        let c = Config {
            providers: vec![provider("openai-provider")],
            ..Config::default()
        };

        assert!(super::select_provider_config(&c, true).is_none());
    }
}

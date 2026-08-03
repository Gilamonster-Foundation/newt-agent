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
/// Instantiate EXACTLY the selected configured backend, by its declared kind.
/// An explicitly selected backend is authoritative — an unsupported kind is a
/// deterministic error, never a silent fallback to a different backend.
fn instantiate_configured_backend(
    backend: &newt_core::BackendConfig,
) -> anyhow::Result<Arc<dyn newt_inference::InferenceBackend>> {
    use newt_core::BackendKind;
    match backend.kind.unwrap_or_default() {
        // API-aware transport: `api = "responses"` → /v1/responses, else Chat
        // Completions. Both the flat and coder ACP paths consume this backend.
        BackendKind::Openai => {
            tracing::info!(
                name = %backend.name,
                endpoint = %backend.endpoint,
                model = %backend.effective_model().unwrap_or("(server decides)"),
                api = ?backend.api.unwrap_or_default(),
                authenticated = backend.resolve_api_key().is_some(),
                "worker: selected OpenAI-compatible backend"
            );
            Ok(newt_inference::openai_inference_backend(backend))
        }
        BackendKind::Ollama => {
            let model = backend.effective_model().ok_or_else(|| {
                anyhow::anyhow!(
                    "worker: Ollama backend '{}' has no configured model",
                    backend.name
                )
            })?;
            tracing::info!(
                name = %backend.name, endpoint = %backend.endpoint, %model,
                "worker: selected configured Ollama backend"
            );
            Ok(Arc::new(newt_inference::local::LocalOllamaBackend::new(
                backend.endpoint.clone(),
                model,
            )))
        }
        other => anyhow::bail!(
            "worker: backend '{}' has unsupported kind {other:?} — the ACP worker \
             supports `openai` and `ollama` backends",
            backend.name
        ),
    }
}

/// Resolve exactly ONE backend through the shared selection contract
/// ([`Config::select_backend`]) and instantiate that one. An explicitly selected
/// backend is authoritative — never replaced by the first OpenAI entry or a bare
/// `providers.first()`. `OLLAMA_HOST` is the unambiguous "use my local Ollama"
/// override (and keeps the mock-mode e2e hermetic), so it forces local discovery
/// ahead of the config. Local discovery is a fallback ONLY when the selection is
/// [`SelectionOutcome::Unset`](newt_core::config::SelectionOutcome) — nothing is
/// configured. A [`SelectionOutcome::UnknownNamed`] selector is a hard error, not
/// a cue to discover.
async fn resolve_backend() -> anyhow::Result<Arc<dyn newt_inference::InferenceBackend>> {
    use newt_core::config::{SelectedBackend, SelectionOutcome};
    use newt_core::Config;

    let ollama_override = std::env::var_os("OLLAMA_HOST").is_some();
    let cfg = Config::resolve().unwrap_or_default();

    if !ollama_override {
        match cfg.select_backend() {
            SelectionOutcome::Selected(SelectedBackend::Provider(provider)) => {
                tracing::info!(
                    name = %provider.name,
                    command = %provider.command,
                    model = %provider.model.as_deref().unwrap_or("(missing)"),
                    "worker: selected provider-plugin backend"
                );
                return Ok(Arc::new(
                    newt_inference::provider_plugin::ProviderPluginBackend::from_config(provider)?,
                ));
            }
            SelectionOutcome::Selected(SelectedBackend::Configured(backend)) => {
                return instantiate_configured_backend(backend);
            }
            SelectionOutcome::UnknownNamed(name) => {
                // An explicit selector ($NEWT_PROVIDER / default_backend) named an
                // entry that matches no configured backend or provider. Surface
                // the operator error — never silently discover or run a different
                // backend (an explicitly selected backend is authoritative).
                anyhow::bail!(
                    "worker: selected backend '{name}' is not defined in any \
                     [[backends]] or [[providers]] entry — fix the \
                     $NEWT_PROVIDER / default_backend selector (no silent fallback)"
                );
            }
            SelectionOutcome::Unset => {} // nothing configured → local discovery below
        }
    }

    let default_model =
        std::env::var("NEWT_DEFAULT_MODEL").unwrap_or_else(|_| "llama3.1:8b".to_string());
    tracing::info!(
        model = %default_model,
        "worker: no configured backend selected — falling back to local Ollama discovery"
    );
    let backend = newt_inference::local::LocalOllamaBackend::discover(&default_model).await?;
    Ok(Arc::new(backend))
}

#[cfg(test)]
mod backend_selection_tests {
    //! W1 (unified backend resolution) — the worker-layer half.
    //!
    //! These assert the **destination** the shared contract instantiates: which
    //! transport (`ollama-local` / `vllm-local` / `openai-responses`) and which
    //! URL a selected backend resolves to — not merely the returned Rust type.
    //! The *selection precedence* itself (including the `$NEWT_PROVIDER` cases,
    //! which need a serialized env guard) is proved in
    //! `newt_core::config`'s `select_backend_tests`.
    use super::instantiate_configured_backend;
    use newt_core::config::{ProviderConfig, SelectedBackend, SelectionOutcome};
    use newt_core::router::Tier;
    use newt_core::{BackendConfig, BackendKind, Config, OpenAiApi};
    // `.name()` / `.endpoint()` are trait methods — bring the trait into scope so
    // the tests can assert the concrete destination each backend instantiates to.
    use newt_inference::InferenceBackend;

    fn openai(name: &str, api: OpenAiApi, endpoint: &str) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            endpoint: endpoint.into(),
            model: Some("m".into()),
            tiers: vec![Tier::Fast],
            kind: Some(BackendKind::Openai),
            api: Some(api),
            ..Default::default()
        }
    }

    fn ollama(name: &str, endpoint: &str) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            endpoint: endpoint.into(),
            model: Some("llama3.1:8b".into()),
            tiers: vec![Tier::Fast],
            kind: Some(BackendKind::Ollama),
            ..Default::default()
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

    fn cfg(backends: Vec<BackendConfig>, providers: Vec<ProviderConfig>) -> Config {
        Config {
            backends,
            providers,
            default_backend: None,
            ..Config::default()
        }
    }

    // --- Destination: the selected backend instantiates to the right transport
    // and URL (invariant 1: the explicitly selected backend is authoritative). ---

    #[test]
    fn ollama_backend_instantiates_ollama_transport_at_its_endpoint() {
        let b = ollama("local", "http://ollama.host:11434/");
        let backend = instantiate_configured_backend(&b).expect("instantiate");
        assert_eq!(backend.name(), "ollama-local");
        assert_eq!(backend.endpoint(), Some("http://ollama.host:11434/"));
    }

    #[test]
    fn openai_chat_completions_instantiates_vllm_transport_at_its_endpoint() {
        let b = openai(
            "cloud",
            OpenAiApi::ChatCompletions,
            "http://vllm.host:8000/",
        );
        let backend = instantiate_configured_backend(&b).expect("instantiate");
        // Chat Completions → the vLLM (POST /v1/chat/completions) transport.
        assert_eq!(backend.name(), "vllm-local");
        assert_eq!(backend.endpoint(), Some("http://vllm.host:8000/"));
    }

    #[test]
    fn openai_responses_instantiates_responses_transport_at_its_endpoint() {
        let b = openai("sol", OpenAiApi::Responses, "http://sol.host:8000/");
        let backend = instantiate_configured_backend(&b).expect("instantiate");
        // Responses → the POST /v1/responses transport, NOT chat/completions.
        assert_eq!(backend.name(), "openai-responses");
        assert_eq!(backend.endpoint(), Some("http://sol.host:8000/"));
    }

    #[test]
    fn explicitly_selected_unsupported_kind_is_a_deterministic_error() {
        // An `embedded` backend is not an ACP-worker transport: a hard error,
        // never a silent fallback to Ollama discovery.
        let b = BackendConfig {
            name: "in-proc".into(),
            kind: Some(BackendKind::Embedded),
            model_path: Some("~/models/tiny.gguf".into()),
            ..Default::default()
        };
        let err = instantiate_configured_backend(&b)
            .map(|_| ())
            .expect_err("unsupported kind must error");
        let msg = err.to_string();
        assert!(msg.contains("unsupported kind"), "got: {msg}");
        assert!(msg.contains("in-proc"), "error names the backend: {msg}");
    }

    #[test]
    fn ollama_backend_without_a_model_is_a_deterministic_error() {
        // Regression: an Ollama backend with no configured model must error
        // (the worker cannot invent a model), not panic or silently discover.
        let b = BackendConfig {
            name: "modelless".into(),
            endpoint: "http://ollama.host:11434/".into(),
            kind: Some(BackendKind::Ollama),
            model: None,
            ..Default::default()
        };
        let err = instantiate_configured_backend(&b)
            .map(|_| ())
            .expect_err("no-model must error");
        assert!(
            err.to_string().contains("no configured model"),
            "got: {err}"
        );
    }

    // --- Selection: the unified contract picks the RIGHT entry (env-free cases;
    // the `$NEWT_PROVIDER` cases live in newt-core's serialized guard). ---

    #[test]
    fn default_backend_selects_ollama_even_when_openai_is_also_configured() {
        // The bug this closes: "mixed Ollama + OpenAI ⇒ OpenAI wins" is WRONG
        // when Ollama was explicitly selected. default_backend = the Ollama
        // entry ⇒ that Ollama backend is authoritative and instantiates to its
        // own endpoint, not the OpenAI one.
        let mut c = cfg(
            vec![
                ollama("local", "http://ollama.host:11434/"),
                openai(
                    "cloud",
                    OpenAiApi::ChatCompletions,
                    "http://vllm.host:8000/",
                ),
            ],
            vec![],
        );
        c.default_backend = Some("local".into());

        let SelectionOutcome::Selected(SelectedBackend::Configured(sel)) = c.select_backend()
        else {
            panic!("expected the configured Ollama backend to be selected");
        };
        assert_eq!(sel.name, "local");
        let backend = instantiate_configured_backend(sel).expect("instantiate");
        assert_eq!(backend.name(), "ollama-local");
        assert_eq!(backend.endpoint(), Some("http://ollama.host:11434/"));
    }

    #[test]
    fn provider_plugin_is_selected_and_instantiated_when_named() {
        let mut c = cfg(vec![], vec![provider("myplugin")]);
        c.default_backend = Some("myplugin".into());

        let SelectionOutcome::Selected(SelectedBackend::Provider(p)) = c.select_backend() else {
            panic!("expected the named provider plugin to be selected");
        };
        assert_eq!(p.name, "myplugin");
        let backend =
            newt_inference::provider_plugin::ProviderPluginBackend::from_config(p).expect("build");
        assert_eq!(backend.name(), "myplugin");
        // A subprocess plugin bridges inference in-process → no HTTP endpoint.
        assert_eq!(backend.endpoint(), None);
    }

    #[test]
    fn unknown_named_backend_is_an_error_not_a_silent_fallback() {
        // default_backend names an entry that matches no backend AND no provider,
        // while an OpenAI backend is present. The contract must NOT silently run
        // the OpenAI backend — it reports the unknown selector.
        let mut c = cfg(
            vec![openai(
                "cloud",
                OpenAiApi::ChatCompletions,
                "http://vllm.host:8000/",
            )],
            vec![],
        );
        c.default_backend = Some("ghost".into());

        match c.select_backend() {
            SelectionOutcome::UnknownNamed(name) => assert_eq!(name, "ghost"),
            other => panic!("expected UnknownNamed(\"ghost\"), got {other:?}"),
        }
    }

    #[test]
    fn nothing_configured_is_unset_permitting_local_discovery() {
        let c = cfg(vec![], vec![]);
        assert!(matches!(c.select_backend(), SelectionOutcome::Unset));
    }
}

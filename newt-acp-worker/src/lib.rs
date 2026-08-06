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

/// Instantiate EXACTLY the selected configured backend, by its declared kind.
/// An explicitly selected backend is authoritative — an unsupported kind is a
/// deterministic error, never a silent fallback to a different backend.
/// `ollama_host` is the `$OLLAMA_HOST` override, applied ONLY to a selected
/// Ollama backend's endpoint (it must never erase or redirect an explicitly
/// selected OpenAI/provider backend). The selected Ollama backend keeps its own
/// configured model.
fn instantiate_configured_backend(
    backend: &newt_core::BackendConfig,
    ollama_host: Option<&str>,
) -> anyhow::Result<Arc<dyn newt_inference::InferenceBackend>> {
    use newt_core::BackendKind;
    match backend.kind.unwrap_or_default() {
        // API-aware transport: `api = "responses"` → /v1/responses, else Chat
        // Completions. Both the flat and coder ACP paths consume this backend.
        // `$OLLAMA_HOST` does NOT apply here — it is an Ollama endpoint hint and
        // must not redirect an explicitly selected OpenAI backend.
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
            // `$OLLAMA_HOST` supplies the endpoint for a SELECTED Ollama backend,
            // keeping its selected model. Absent → the configured endpoint.
            let endpoint = ollama_host
                .filter(|h| !h.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| backend.endpoint.clone());
            tracing::info!(
                name = %backend.name, %endpoint, %model,
                ollama_host_override = ollama_host.is_some(),
                "worker: selected configured Ollama backend"
            );
            Ok(Arc::new(
                newt_inference::local::LocalOllamaBackend::new(endpoint, model)
                    // Ollama Cloud (https://ollama.com) authenticates with a
                    // bearer; LAN Ollama resolves no key and stays keyless.
                    .with_api_key(backend.resolve_api_key()),
            ))
        }
        // Native Anthropic Messages transport (/v1/messages, x-api-key).
        // `$OLLAMA_HOST` does NOT apply — same rule as OpenAI above.
        BackendKind::Anthropic => {
            tracing::info!(
                name = %backend.name,
                endpoint = %backend.endpoint,
                model = %backend.effective_model().unwrap_or("(server decides)"),
                authenticated = backend.resolve_api_key().is_some(),
                "worker: selected Anthropic backend"
            );
            Ok(Arc::new(newt_inference::AnthropicBackend::from_config(
                backend,
            )))
        }
        other => anyhow::bail!(
            "worker: backend '{}' has unsupported kind {other:?} — the ACP worker \
             supports `openai`, `ollama`, and `anthropic` backends",
            backend.name
        ),
    }
}

/// The resolved worker backend, or a request to fall back to local Ollama
/// discovery. Split from [`resolve_backend`] so the selection contract (which is
/// pure over a [`Config`] + the `$OLLAMA_HOST` override) is unit-testable without
/// touching the environment or the network.
enum ResolvedBackend {
    Ready(Arc<dyn newt_inference::InferenceBackend>),
    /// Nothing configured — the caller may discover a local Ollama.
    DiscoverLocalOllama,
}

/// Resolve exactly ONE backend from the shared selection contract, applying the
/// `$OLLAMA_HOST` override ONLY as an Ollama endpoint hint. An explicitly named
/// unknown backend is a hard error; nothing configured yields
/// [`ResolvedBackend::DiscoverLocalOllama`]. `$OLLAMA_HOST` never erases an
/// explicit `$NEWT_PROVIDER` / `default_backend` selection.
fn resolve_configured_backend(
    cfg: &newt_core::Config,
    ollama_host: Option<&str>,
) -> anyhow::Result<ResolvedBackend> {
    use newt_core::config::{SelectedBackend, SelectionOutcome};
    match cfg.select_backend() {
        SelectionOutcome::Selected(SelectedBackend::Provider(provider)) => {
            tracing::info!(
                name = %provider.name,
                command = %provider.command,
                model = %provider.model.as_deref().unwrap_or("(missing)"),
                "worker: selected provider-plugin backend"
            );
            Ok(ResolvedBackend::Ready(Arc::new(
                newt_inference::provider_plugin::ProviderPluginBackend::from_config(provider)?,
            )))
        }
        SelectionOutcome::Selected(SelectedBackend::Configured(backend)) => Ok(
            ResolvedBackend::Ready(instantiate_configured_backend(backend, ollama_host)?),
        ),
        SelectionOutcome::UnknownNamed(name) => anyhow::bail!(
            "worker: selected backend '{name}' is not defined in any \
             [[backends]] or [[providers]] entry — fix the \
             $NEWT_PROVIDER / default_backend selector (no silent fallback)"
        ),
        SelectionOutcome::Unset => Ok(ResolvedBackend::DiscoverLocalOllama),
    }
}

/// Resolve exactly ONE backend through the shared selection contract
/// ([`Config::select_backend`]) and instantiate it. An explicitly selected
/// backend is authoritative — never replaced by the first OpenAI entry or a bare
/// `providers.first()`, and NEVER erased by `$OLLAMA_HOST`: that env var is only
/// an Ollama endpoint hint (applied to a selected Ollama backend, or to local
/// discovery), so a `$NEWT_PROVIDER` / `default_backend` that names an OpenAI or
/// provider backend still wins even when `$OLLAMA_HOST` is set. Local discovery
/// is a fallback ONLY when the selection is
/// [`SelectionOutcome::Unset`](newt_core::config::SelectionOutcome) — nothing is
/// configured. A [`SelectionOutcome::UnknownNamed`] selector is a hard error.
///
/// A configuration RESOLUTION error propagates (it is not swallowed into an empty
/// config): a malformed or unreadable config must not silently become a different
/// backend choice.
async fn resolve_backend() -> anyhow::Result<Arc<dyn newt_inference::InferenceBackend>> {
    use anyhow::Context as _;

    let cfg =
        newt_core::Config::resolve().context("worker: failed to resolve Newt configuration")?;
    let ollama_host = std::env::var("OLLAMA_HOST").ok().filter(|h| !h.is_empty());

    match resolve_configured_backend(&cfg, ollama_host.as_deref())? {
        ResolvedBackend::Ready(backend) => Ok(backend),
        ResolvedBackend::DiscoverLocalOllama => {
            let default_model =
                std::env::var("NEWT_DEFAULT_MODEL").unwrap_or_else(|_| "llama3.1:8b".to_string());
            tracing::info!(
                model = %default_model,
                "worker: no configured backend selected — falling back to local Ollama discovery"
            );
            let backend =
                newt_inference::local::LocalOllamaBackend::discover(&default_model).await?;
            Ok(Arc::new(backend))
        }
    }
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
    use super::{instantiate_configured_backend, resolve_configured_backend, ResolvedBackend};
    use newt_core::config::{ProviderConfig, SelectedBackend, SelectionOutcome};
    use newt_core::router::Tier;
    use newt_core::{BackendConfig, BackendKind, Config, OpenAiApi};
    use std::sync::Arc;
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
        let backend = instantiate_configured_backend(&b, None).expect("instantiate");
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
        let backend = instantiate_configured_backend(&b, None).expect("instantiate");
        // Chat Completions → the vLLM (POST /v1/chat/completions) transport.
        assert_eq!(backend.name(), "vllm-local");
        assert_eq!(backend.endpoint(), Some("http://vllm.host:8000/"));
    }

    #[test]
    fn openai_responses_instantiates_responses_transport_at_its_endpoint() {
        let b = openai("sol", OpenAiApi::Responses, "http://sol.host:8000/");
        let backend = instantiate_configured_backend(&b, None).expect("instantiate");
        // Responses → the POST /v1/responses transport, NOT chat/completions.
        assert_eq!(backend.name(), "openai-responses");
        assert_eq!(backend.endpoint(), Some("http://sol.host:8000/"));
    }

    #[test]
    fn anthropic_backend_instantiates_native_transport_at_its_endpoint() {
        let b = BackendConfig {
            name: "claude".into(),
            endpoint: "https://api.anthropic.com".into(),
            model: Some("claude-sonnet-4-5".into()),
            kind: Some(BackendKind::Anthropic),
            tiers: vec![Tier::Complex],
            ..Default::default()
        };
        let backend = instantiate_configured_backend(&b, None).expect("instantiate");
        // Anthropic → the native /v1/messages transport, never the bail arm.
        assert_eq!(backend.name(), "anthropic");
        assert_eq!(backend.endpoint(), Some("https://api.anthropic.com"));
        assert_eq!(backend.model_id(), "claude-sonnet-4-5");
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
        let err = instantiate_configured_backend(&b, None)
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
        let err = instantiate_configured_backend(&b, None)
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
        let backend = instantiate_configured_backend(sel, None).expect("instantiate");
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

    // --- R3: the worker resolver — $OLLAMA_HOST is an Ollama endpoint hint that
    // NEVER erases an explicit selection; config-error propagation is in
    // resolve_backend's `?` (a malformed config cannot become a silent empty one). ---

    fn ready(r: anyhow::Result<ResolvedBackend>) -> Arc<dyn InferenceBackend> {
        match r.expect("resolved") {
            ResolvedBackend::Ready(b) => b,
            ResolvedBackend::DiscoverLocalOllama => panic!("expected a configured backend"),
        }
    }

    #[test]
    fn ollama_host_supplies_the_endpoint_of_a_selected_ollama_backend() {
        // An explicit Ollama backend + $OLLAMA_HOST → the SELECTED model at the
        // OLLAMA_HOST endpoint (the override supplies the endpoint, not the model).
        let mut c = cfg(vec![ollama("local", "http://configured:11434/")], vec![]);
        c.default_backend = Some("local".into());
        let backend = ready(resolve_configured_backend(&c, Some("http://mock:9/")));
        assert_eq!(backend.name(), "ollama-local");
        assert_eq!(backend.endpoint(), Some("http://mock:9/"));
        assert_eq!(backend.model_id(), "llama3.1:8b");
    }

    #[test]
    fn ollama_host_does_not_erase_an_explicitly_selected_openai_backend() {
        // The bug this closes: $OLLAMA_HOST's mere presence used to skip selection
        // entirely and discover local Ollama, silently ignoring an explicit
        // OpenAI selector. Now the selected OpenAI backend wins and keeps its own
        // endpoint — $OLLAMA_HOST is inert for it.
        let mut c = cfg(
            vec![openai(
                "cloud",
                OpenAiApi::ChatCompletions,
                "http://vllm:8000/",
            )],
            vec![],
        );
        c.default_backend = Some("cloud".into());
        let backend = ready(resolve_configured_backend(&c, Some("http://mock:9/")));
        assert_eq!(backend.name(), "vllm-local");
        assert_eq!(
            backend.endpoint(),
            Some("http://vllm:8000/"),
            "$OLLAMA_HOST must not redirect a selected OpenAI backend"
        );
    }

    #[test]
    fn unset_selection_requests_local_discovery_even_with_ollama_host() {
        let c = cfg(vec![], vec![]);
        assert!(matches!(
            resolve_configured_backend(&c, Some("http://mock:9/")).expect("ok"),
            ResolvedBackend::DiscoverLocalOllama
        ));
    }

    #[test]
    fn resolver_errors_on_an_unknown_named_backend() {
        let mut c = cfg(
            vec![openai(
                "cloud",
                OpenAiApi::ChatCompletions,
                "http://vllm:8000/",
            )],
            vec![],
        );
        c.default_backend = Some("ghost".into());
        let err = resolve_configured_backend(&c, None)
            .map(|_| ())
            .expect_err("unknown named backend is a hard error");
        assert!(err.to_string().contains("ghost"), "got: {err}");
    }
}

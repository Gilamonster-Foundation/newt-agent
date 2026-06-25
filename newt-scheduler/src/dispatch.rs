//! Dispatch — the swappable inference **strategy**.
//!
//! The [`BackendPool`](crate::BackendPool) selects *which* backend (by model-pin +
//! health); a [`Dispatcher`] decides *how* to reach it — local HTTP (reuse
//! newt-inference), a mesh peer, or a mock. This is the seam the toolkit is
//! decomposed along: the role-routing loop talks to `Dispatcher` + `PoolSource` +
//! `Prober` traits and never to a concrete transport, so a use case swaps the
//! strategy (rapid-dev `LocalDispatcher` ↔ a future remote `MeshDispatcher` behind
//! the `mesh` feature) without touching the loop.
//!
//! It **reuses** newt-inference's [`ChatRequest`]/[`ChatReply`] rather than
//! reinventing them — one inference path.

use crate::{BackendPool, Failover, PoolBackend};
use async_trait::async_trait;
use newt_core::{BackendKind, Tier};
use newt_inference::local::{LocalOllamaBackend, LocalVllmBackend};
use newt_inference::InferenceBackend;

pub use newt_inference::{ChatReply, ChatRequest};

/// The swappable dispatch strategy: run one role turn on a selected backend.
///
/// `Send + Sync` so a `&dyn Dispatcher` can be shared across the crew's roles; the
/// pool chose `backend` (model-pin), the strategy owns the transport.
#[async_trait]
pub trait Dispatcher: Send + Sync {
    /// Run `req` against `backend` using `model`, returning the reply (or an error,
    /// which the pool's failover treats as "try the next candidate").
    async fn dispatch(
        &self,
        backend: &PoolBackend,
        model: &str,
        req: ChatRequest,
    ) -> anyhow::Result<ChatReply>;
}

/// The **reuse** strategy and fast first impl: build a newt-inference backend for
/// the selected endpoint + pinned model and call it. One inference path; remote
/// dispatch (a `MeshDispatcher`) is the swap-in behind the `mesh` feature.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalDispatcher;

#[async_trait]
impl Dispatcher for LocalDispatcher {
    async fn dispatch(
        &self,
        backend: &PoolBackend,
        model: &str,
        req: ChatRequest,
    ) -> anyhow::Result<ChatReply> {
        match backend.kind {
            BackendKind::Ollama => {
                LocalOllamaBackend::new(backend.endpoint.clone(), model)
                    .complete(req)
                    .await
            }
            // Hosted / OpenAI-compatible (hosted OpenAI, vLLM, …): the
            // `/v1/chat/completions` wire with the bearer token, so the crew/team
            // can run on a HOSTED LLM, not just local Ollama.
            BackendKind::Openai => {
                LocalVllmBackend::new(backend.endpoint.clone(), model)
                    .with_api_key(backend.api_key.clone())
                    .complete(req)
                    .await
            }
            // The in-process embedded backend (#639): a first-class backend that
            // loads a local GGUF (no endpoint) and runs candle. Behind the
            // `embedded` feature so the lean/headless build never pulls candle.
            BackendKind::Embedded => {
                #[cfg(feature = "embedded")]
                {
                    let path = backend.model_path.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "embedded backend '{}' needs a model_path (the local GGUF file)",
                            backend.name
                        )
                    })?;
                    newt_inference::embedded::EmbeddedBackend::new(model, path)?
                        .complete(req)
                        .await
                }
                #[cfg(not(feature = "embedded"))]
                {
                    let _ = req;
                    anyhow::bail!(
                        "backend '{}' is kind=embedded, but this build lacks the `embedded` \
                         feature — rebuild with --features embedded (or use an ollama/openai backend)",
                        backend.name
                    )
                }
            }
        }
    }
}

impl BackendPool {
    /// Run a role turn end-to-end: select a live backend that serves `(tier,
    /// model)`, dispatch via the strategy, and **fail over** to the next candidate
    /// on error. Returns the chosen backend + reply (and the names that failed, to
    /// [`mark`](Self::mark)). `None` when nothing live serves the pinned model.
    ///
    /// This is the async sibling of [`dispatch_with_failover`](Self::dispatch_with_failover),
    /// specialised to inference via the [`Dispatcher`] strategy.
    pub async fn run_role(
        &self,
        dispatcher: &dyn Dispatcher,
        tier: Tier,
        model: &str,
        req: ChatRequest,
    ) -> Option<Failover<ChatReply>> {
        let mut failed = Vec::new();
        for b in self.ranked_candidates(tier, Some(model)) {
            match dispatcher.dispatch(b, model, req.clone()).await {
                Ok(result) => {
                    return Some(Failover {
                        chosen: b.name.clone(),
                        result,
                        failed,
                    })
                }
                Err(_) => failed.push(b.name.clone()),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Health, PoolBackend, StaticSource};

    fn be(name: &str, model: &str, health: Health) -> PoolBackend {
        PoolBackend::new(name, format!("http://{name}:11434"), BackendKind::Ollama)
            .with_models([model])
            .with_health(health)
    }

    /// Deterministic dispatch strategy: errors for the named backends, else echoes.
    struct MockDispatcher {
        fail: Vec<String>,
    }
    #[async_trait]
    impl Dispatcher for MockDispatcher {
        async fn dispatch(
            &self,
            backend: &PoolBackend,
            model: &str,
            _req: ChatRequest,
        ) -> anyhow::Result<ChatReply> {
            if self.fail.iter().any(|f| f == &backend.name) {
                anyhow::bail!("simulated failure on {}", backend.name);
            }
            Ok(ChatReply {
                content: format!("served by {} ({model})", backend.name),
                model_id: model.to_string(),
                usage: None,
            })
        }
    }

    fn pool() -> BackendPool {
        BackendPool::from_source(&StaticSource {
            backends: vec![
                be("dgx", "qwen3-coder:30b", Health::Up),
                be("dgx-2", "qwen3-coder:30b", Health::Up),
            ],
        })
    }

    #[tokio::test]
    async fn run_role_fails_over_to_the_next_candidate() {
        let p = pool();
        let d = MockDispatcher {
            fail: vec!["dgx".into()],
        };
        let out = p
            .run_role(
                &d,
                Tier::Complex,
                "qwen3-coder:30b",
                ChatRequest::new().user("hi"),
            )
            .await
            .unwrap();
        assert_eq!(out.chosen, "dgx-2");
        assert_eq!(out.failed, vec!["dgx".to_string()]);
        assert!(out.result.content.contains("served by dgx-2"));
    }

    #[tokio::test]
    async fn run_role_none_when_all_fail() {
        let p = pool();
        let d = MockDispatcher {
            fail: vec!["dgx".into(), "dgx-2".into()],
        };
        let out = p
            .run_role(
                &d,
                Tier::Complex,
                "qwen3-coder:30b",
                ChatRequest::new().user("hi"),
            )
            .await;
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn run_role_none_when_no_backend_serves_the_model() {
        let p = pool();
        let d = MockDispatcher { fail: vec![] };
        let out = p
            .run_role(
                &d,
                Tier::Complex,
                "devstral-small-2:24b",
                ChatRequest::new().user("hi"),
            )
            .await;
        assert!(
            out.is_none(),
            "no backend has devstral → no dispatch, no pick"
        );
    }

    #[tokio::test]
    async fn local_dispatcher_is_a_trait_object() {
        // The reuse strategy compiles + is the `&dyn Dispatcher` the loop will hold.
        let _d: &dyn Dispatcher = &LocalDispatcher;
    }

    #[test]
    fn pool_backend_carries_api_key_for_hosted() {
        // A hosted/OpenAI backend keeps its bearer token (empty filtered to None).
        let hosted = PoolBackend::new("openai", "https://api.openai.com", BackendKind::Openai)
            .with_api_key(Some("sk-test".to_string()));
        assert_eq!(hosted.api_key.as_deref(), Some("sk-test"));
        let keyless = PoolBackend::new("o", "http://localhost", BackendKind::Ollama)
            .with_api_key(Some(String::new()));
        assert!(keyless.api_key.is_none(), "empty key normalises to None");
    }
}

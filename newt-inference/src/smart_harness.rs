//! One independent auxiliary backend, shared by CLI and TUI consumers.

use std::sync::Arc;
use std::time::Duration;

use newt_core::config::SmartHarnessConfig;
use newt_core::{BackendKind, SummarizeFn};
use serde_json::Value;

use crate::backend::{ChatRequest, InferenceBackend};
use crate::local::{LocalOllamaBackend, LocalVllmBackend};
use crate::retry::RetryPolicy;

/// Resolved callback and credential-free manifest committed by the host session.
pub struct Auxiliary {
    pub complete: Arc<SummarizeFn>,
    pub manifest: Value,
}

/// Resolve the auxiliary. An unavailable or incomplete choice never silently falls back to
/// primary inference; an external auxiliary MAY deliberately name the primary's origin, and
/// the run manifest records that it did so the two runs stay comparable (D10, D12).
pub fn build(
    config: &SmartHarnessConfig,
    primary_endpoint: &str,
    primary_kind: BackendKind,
) -> anyhow::Result<Auxiliary> {
    anyhow::ensure!(
        config.adjudication.timeout_ms > 0
            && config.adjudication.max_calls > 0
            && config.adjudication.max_output_tokens > 0,
        "auxiliary timeout, call budget, and output token limit must be positive"
    );
    anyhow::ensure!(
        config
            .device
            .as_deref()
            .is_none_or(|device| !device.trim().is_empty()),
        "smart-harness auxiliary placement declaration must not be empty"
    );
    let timeout = Duration::from_millis(config.adjudication.timeout_ms);
    let mut disclosure = newt_core::ocap::DisclosureFilter::new();
    let (backend, mut manifest) = match &config.backend {
        Some(reference) => {
            let placement = config
                .device
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "an external auxiliary requires an explicit placement declaration (e.g. \"cpu\" or \"cuda\"); it is recorded, not verified"
                    )
                })?;
            anyhow::ensure!(
                config.model.is_none() && config.model_path.is_none(),
                "external and embedded auxiliary configuration cannot be combined"
            );
            let endpoint = reference
                .endpoint
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("external auxiliary must pin its endpoint"))?;
            let model = reference
                .model
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("external auxiliary must pin its model"))?;
            let kind = reference
                .kind
                .ok_or_else(|| anyhow::anyhow!("external auxiliary must pin its protocol"))?;
            anyhow::ensure!(
                kind != BackendKind::Embedded,
                "embedded auxiliary uses model/model_path, not backend"
            );
            let url = reqwest::Url::parse(endpoint)
                .map_err(|_| anyhow::anyhow!("invalid auxiliary endpoint URL"))?;
            anyhow::ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "auxiliary endpoint must be an HTTP URL without credentials, query, or fragment"
            );
            // D10 reversal (2026-09-11): sharing the primary's origin is a legitimate,
            // recorded choice, not a refusal. The independence rule existed for GPU
            // contention, not authority — proposal legality is validated by the harness
            // regardless of who proposed. What must survive is comparability: the
            // manifest says whether this judge shared the primary's origin.
            let shares_primary_origin = primary_kind != BackendKind::Embedded
                && reqwest::Url::parse(primary_endpoint)
                    .map(|primary| primary.origin() == url.origin())
                    .unwrap_or(false);
            let (_, _, _, key) = reference.resolve("", "", kind, &None);
            anyhow::ensure!(
                key.is_some()
                    || (reference.api_key_env.is_none() && reference.api_key_file.is_none()),
                "configured auxiliary credential is unavailable"
            );
            if let Some(key) = &key {
                disclosure.register(key);
            }
            let client = reqwest::Client::builder().timeout(timeout).build()?;
            let retry = RetryPolicy {
                max_retries: 0,
                ..RetryPolicy::default()
            };
            let backend: Arc<dyn InferenceBackend> = match kind {
                BackendKind::Ollama => Arc::new(
                    LocalOllamaBackend::new(endpoint, model)
                        .with_client(client)
                        .with_api_key(key)
                        .with_retry_policy(retry),
                ),
                BackendKind::Openai => Arc::new(
                    LocalVllmBackend::new(endpoint, model)
                        .with_client(client)
                        .with_api_key(key)
                        .with_retry_policy(retry),
                ),
                BackendKind::Anthropic => Arc::new(
                    crate::AnthropicBackend::new(endpoint, model)
                        .with_client(client)
                        .with_api_key(key)
                        .with_retry_policy(retry),
                ),
                BackendKind::Embedded => unreachable!("rejected above"),
            };
            (
                backend,
                serde_json::json!({"backend":kind.label(), "model":model,
                "endpoint":url.as_str(), "placement":placement,
                "placement_evidence":"operator-declared",
                "shares_primary_origin":shares_primary_origin}),
            )
        }
        None => {
            // The embedded auxiliary is constructed with `new_cpu` and cannot run
            // anywhere else, so a non-cpu declaration without an external backend
            // is a contradiction, not a choice. Refuse it before touching assets.
            anyhow::ensure!(
                config.device.as_deref().is_none_or(|d| d.trim() == "cpu"),
                "embedded auxiliary always runs on cpu; a non-cpu placement declaration requires an external backend"
            );
            let (backend, mut manifest) = embedded(config)?;
            manifest["placement"] = Value::String("cpu".into());
            manifest["shares_primary_origin"] = Value::Bool(false);
            (backend, manifest)
        }
    };
    manifest["primary_protocol"] = Value::String(primary_kind.label().into());
    manifest["adjudication"] = serde_json::to_value(&config.adjudication)?;
    let max_output_tokens = config.adjudication.max_output_tokens;
    let system_instruction = config.adjudication.system_instruction.clone();
    manifest["max_output_tokens"] = Value::from(max_output_tokens);
    manifest["transport_retries"] = Value::from(0);
    let complete: Arc<SummarizeFn> = Arc::new(move |prompt| {
        let backend = backend.clone();
        let disclosure = disclosure.clone();
        let system_instruction = system_instruction.clone();
        Box::pin(async move {
            let mut request = ChatRequest::new().max_tokens(max_output_tokens);
            if !system_instruction.trim().is_empty() {
                request = request.system(system_instruction);
            }
            let request = request.user(prompt);
            let reply = tokio::time::timeout(timeout, backend.complete(request))
                .await
                .map_err(|_| anyhow::anyhow!("auxiliary inference deadline exceeded"))?
                .map_err(|error| anyhow::anyhow!(disclosure.redact(&error.to_string())))?;
            Ok(disclosure.redact(&reply.content))
        })
    });
    Ok(Auxiliary { complete, manifest })
}

#[cfg(feature = "embedded")]
fn embedded(config: &SmartHarnessConfig) -> anyhow::Result<(Arc<dyn InferenceBackend>, Value)> {
    let model = config
        .model
        .as_deref()
        .unwrap_or(crate::palette::default_model().name);
    let path = config.model_path.clone().or_else(|| crate::palette::resolve_local(model))
        .ok_or_else(|| anyhow::anyhow!("embedded auxiliary model is unavailable; provision it with `newt models pull {model}` or configure an external backend"))?;
    let backend = crate::embedded::EmbeddedBackend::new_cpu(model, &path)?;
    anyhow::ensure!(
        backend.model().arch == crate::palette::ModelArch::Qwen2,
        "embedded auxiliary architecture is unsupported"
    );
    let (weights, tokenizer) = backend
        .pinned_asset_ids()
        .ok_or_else(|| anyhow::anyhow!("CPU auxiliary assets were not pinned"))?;
    let manifest = serde_json::json!({"backend":"embedded", "model":backend.model().name,
        "model_path":path.canonicalize()?, "weights":weights, "tokenizer":tokenizer,
        "placement_evidence":"constructor-enforced"});
    Ok((Arc::new(backend), manifest))
}

#[cfg(not(feature = "embedded"))]
fn embedded(_: &SmartHarnessConfig) -> anyhow::Result<(Arc<dyn InferenceBackend>, Value)> {
    anyhow::bail!("embedded auxiliary support is not compiled; enable the embedded feature or configure an external backend")
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::config::BackendRef;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ollama_reply(content: &str) -> ResponseTemplate {
        let body = serde_json::json!({
            "message": { "role": "assistant", "content": content },
            "done": true
        })
        .to_string()
            + "\n";
        ResponseTemplate::new(200).set_body_raw(body, "application/x-ndjson")
    }

    fn external(endpoint: &str) -> SmartHarnessConfig {
        let mut config = SmartHarnessConfig {
            enabled: true,
            device: Some("cpu".into()),
            backend: Some(BackendRef {
                kind: Some(BackendKind::Ollama),
                endpoint: Some(endpoint.into()),
                model: Some("auxiliary-fixture".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        config.adjudication.max_output_tokens = 64;
        config
    }

    /// Grounds the configured trust boundary in actual HTTP message roles.
    #[tokio::test]
    async fn auxiliary_system_instruction_precedes_user_evidence() {
        let server = MockServer::start().await;
        let mut config = external(&server.uri());
        config.adjudication.system_instruction = "Return the requested JSON value.".into();
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_partial_json(serde_json::json!({"messages":[
                {"role":"system", "content":"Return the requested JSON value."},
                {"role":"user", "content":"classify this evidence"}
            ]})))
            .respond_with(ollama_reply("\"answer\""))
            .expect(1)
            .mount(&server)
            .await;
        let auxiliary = build(&config, "http://primary.invalid:8000", BackendKind::Openai).unwrap();
        assert_eq!(
            auxiliary.manifest["adjudication"]["system_instruction"],
            config.adjudication.system_instruction
        );
        assert_eq!(
            (auxiliary.complete)("classify this evidence".into())
                .await
                .unwrap(),
            "\"answer\""
        );
    }

    /// Grounds backend selection in an actual request to an independent local server.
    #[tokio::test]
    async fn auxiliary_uses_its_own_bounded_toolless_request_and_never_retries() {
        let server = MockServer::start().await;
        let config = external(&server.uri());
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_partial_json(serde_json::json!({
                "model":"auxiliary-fixture", "stream":true,
                "options":{"num_predict":64},
                "messages":[{"role":"user","content":"classify this"}]
            })))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let auxiliary = build(&config, "http://primary.invalid:8000", BackendKind::Openai).unwrap();
        assert_eq!(auxiliary.manifest["placement"], "cpu");
        assert_eq!(auxiliary.manifest["model"], "auxiliary-fixture");
        assert!((auxiliary.complete)("classify this".into()).await.is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = requests[0].body_json().unwrap();
        assert!(body.get("tools").is_none());
        assert!(requests[0].headers.get("authorization").is_none());
    }

    #[test]
    fn unsafe_or_incomplete_placement_never_falls_back_to_primary() {
        for mutate in [
            |c: &mut SmartHarnessConfig| c.device = None,
            |c: &mut SmartHarnessConfig| c.device = Some("   ".into()),
            |c: &mut SmartHarnessConfig| c.backend.as_mut().unwrap().model = None,
            |c: &mut SmartHarnessConfig| c.backend.as_mut().unwrap().kind = None,
            |c: &mut SmartHarnessConfig| c.backend.as_mut().unwrap().endpoint = None,
            |c: &mut SmartHarnessConfig| c.model = Some("conflicting-embedded-model".into()),
        ] {
            let mut config = external("http://auxiliary.invalid:8001");
            mutate(&mut config);
            assert!(build(&config, "http://primary.invalid:8000", BackendKind::Openai).is_err());
        }
        for endpoint in [
            "http://user:secret@auxiliary.invalid",
            "http://auxiliary.invalid?key=secret",
        ] {
            assert!(build(
                &external(endpoint),
                "http://primary.invalid:8000",
                BackendKind::Openai
            )
            .is_err());
        }
    }

    /// D10 reversal: an auxiliary may name the primary's own origin. The manifest must say so,
    /// the declared placement must be recorded verbatim, and the request must actually reach
    /// that origin — grounded in a real HTTP exchange, not a config read.
    #[tokio::test]
    async fn a_same_origin_auxiliary_is_accepted_and_its_manifest_says_so() {
        let server = MockServer::start().await;
        let mut config = external(&server.uri());
        config.device = Some("cuda".into());
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_reply("\"narration\""))
            .expect(1)
            .mount(&server)
            .await;
        // Same origin as the primary — exactly what the old assertion refused.
        let auxiliary = build(&config, &server.uri(), BackendKind::Openai).unwrap();
        assert_eq!(auxiliary.manifest["shares_primary_origin"], true);
        assert_eq!(auxiliary.manifest["placement"], "cuda");
        assert_eq!(
            auxiliary.manifest["placement_evidence"],
            "operator-declared"
        );
        assert_eq!(
            (auxiliary.complete)("classify this".into()).await.unwrap(),
            "\"narration\""
        );
        // And a distinct origin is recorded as such, so the two runs are distinguishable.
        let distinct = build(&config, "http://primary.invalid:8000", BackendKind::Openai).unwrap();
        assert_eq!(distinct.manifest["shares_primary_origin"], false);
    }

    /// A non-cpu declaration is only meaningful with an external backend; the
    /// embedded path is cpu by construction and must say so before loading assets.
    #[test]
    fn embedded_auxiliary_refuses_a_non_cpu_declaration_before_loading_assets() {
        let config = SmartHarnessConfig {
            enabled: true,
            device: Some("cuda".into()),
            ..Default::default()
        };
        let err = build(&config, "http://primary.invalid:8000", BackendKind::Openai)
            .err()
            .expect("cuda without an external backend must be refused");
        assert!(
            err.to_string().contains("requires an external backend"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn auxiliary_transport_timeout_is_independent_and_fail_closed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(1)))
            .expect(1)
            .mount(&server)
            .await;
        let mut config = external(&server.uri());
        config.adjudication.timeout_ms = 10;
        let auxiliary = build(&config, "http://primary.invalid:8000", BackendKind::Openai).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            (auxiliary.complete)("classify".into()),
        )
        .await;
        assert!(result
            .expect("the auxiliary's own deadline must finish first")
            .is_err());
    }

    /// Grounds secret handling in the backend's actual HTTP error, which the
    /// host records as a failed adjudication after retaining the observation.
    #[tokio::test]
    async fn auxiliary_credentials_never_enter_manifest_or_recordable_errors() {
        use std::io::Write;
        let secret = "fixture-auxiliary-secret-12345";
        let mut key = tempfile::NamedTempFile::new().unwrap();
        key.write_all(secret.as_bytes()).unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(format!("rejected token {secret}")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut config = external(&server.uri());
        config.backend.as_mut().unwrap().api_key_file = Some(key.path().display().to_string());
        let auxiliary = build(&config, "http://primary.invalid:8000", BackendKind::Openai).unwrap();
        assert!(!auxiliary.manifest.to_string().contains(secret));
        let error = (auxiliary.complete)("classify".into()).await.unwrap_err();
        assert!(!error.to_string().contains(secret));
    }
}

use super::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// F5: the loop summarizer's Ollama request must carry the same
/// `options.num_ctx` the main loop sends — without it Ollama silently
/// truncates the (typically largest-of-session) summary request at the
/// model's default window.
#[tokio::test(flavor = "multi_thread")]
async fn loop_summarizer_sends_num_ctx_to_ollama() {
    use std::sync::{Arc, Mutex};
    use wiremock::{Request, Respond};

    struct Capture {
        body: Arc<Mutex<Option<serde_json::Value>>>,
    }
    impl Respond for Capture {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            *self.body.lock().unwrap() = serde_json::from_slice(&req.body).ok();
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": "SUM"}}))
        }
    }

    let server = MockServer::start().await;
    let body = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(Capture { body: body.clone() })
        .mount(&server)
        .await;

    let s = make_loop_summarizer(
        server.uri(),
        "test-model".into(),
        newt_core::BackendKind::Ollama,
        None,
        None,
        SummarizerOpts {
            num_ctx: Some(4_096),
            ..Default::default()
        },
    );
    let out = s("summarize the middle".into()).await.unwrap();
    assert_eq!(out, "SUM");
    let captured = body.lock().unwrap().clone().expect("request captured");
    assert_eq!(
        captured["options"]["num_ctx"], 4_096,
        "the summarizer request must cap Ollama's window like the main loop"
    );
    assert_eq!(
        captured["keep_alive"], "5m",
        "summary request carries keep_alive (24.1, mirrors the main loop)"
    );
    assert!(
        captured.get("tools").is_none(),
        "summarizer stays tools-disabled"
    );

    // No cap configured → no options key (model default, as before).
    let s_none = make_loop_summarizer(
        server.uri(),
        "test-model".into(),
        newt_core::BackendKind::Ollama,
        None,
        None,
        SummarizerOpts::default(),
    );
    s_none("summarize".into()).await.unwrap();
    let captured = body.lock().unwrap().clone().unwrap();
    assert!(captured.get("options").is_none());
}

/// Step 24.1 (#559): for Ollama, the summarizer warms the model
/// (POST /api/generate, model + keep_alive) BEFORE the summary request, so a
/// cold reload is absorbed off the (short) summary timeout.
#[tokio::test(flavor = "multi_thread")]
async fn loop_summarizer_warms_the_model_first() {
    use std::sync::{Arc, Mutex};
    use wiremock::{Request, Respond};

    struct WarmCapture {
        body: Arc<Mutex<Option<serde_json::Value>>>,
    }
    impl Respond for WarmCapture {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            *self.body.lock().unwrap() = serde_json::from_slice(&req.body).ok();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"done": true}))
        }
    }

    let server = MockServer::start().await;
    let warm = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(WarmCapture { body: warm.clone() })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": "SUM"}})),
        )
        .mount(&server)
        .await;

    let s = make_loop_summarizer(
        server.uri(),
        "test-model".into(),
        newt_core::BackendKind::Ollama,
        None,
        None,
        SummarizerOpts::default(),
    );
    let out = s("summarize".into()).await.unwrap();
    assert_eq!(out, "SUM");
    let warm_body = warm
        .lock()
        .unwrap()
        .clone()
        .expect("warm request was made before the summary");
    assert_eq!(warm_body["model"], "test-model", "warm targets the model");
    assert_eq!(warm_body["keep_alive"], "5m", "warm carries keep_alive");
}

/// Step 24.2 (#559): a transient summarizer failure is retried (with
/// backoff) before giving up to the static-marker fallback.
#[tokio::test(flavor = "multi_thread")]
async fn loop_summarizer_retries_then_succeeds() {
    use std::sync::{Arc, Mutex};
    use wiremock::{Request, Respond};

    struct Flaky {
        calls: Arc<Mutex<u32>>,
    }
    impl Respond for Flaky {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let mut n = self.calls.lock().unwrap();
            *n += 1;
            if *n == 1 {
                ResponseTemplate::new(500) // first attempt fails
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "SUM"}}))
            }
        }
    }

    let server = MockServer::start().await;
    let calls = Arc::new(Mutex::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(Flaky {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let s = make_loop_summarizer(
        server.uri(),
        "test-model".into(),
        newt_core::BackendKind::Ollama,
        None,
        None,
        SummarizerOpts {
            retries: 2,
            ..Default::default()
        },
    );
    let out = s("summarize".into()).await.unwrap();
    assert_eq!(out, "SUM");
    assert_eq!(*calls.lock().unwrap(), 2, "retried once after the 500");
}

/// Step 24.2: after exhausting retries the summarizer returns an error
/// (which the compression pipeline turns into the static marker).
#[tokio::test(flavor = "multi_thread")]
async fn loop_summarizer_gives_up_after_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let s = make_loop_summarizer(
        server.uri(),
        "test-model".into(),
        newt_core::BackendKind::Ollama,
        None,
        None,
        SummarizerOpts {
            retries: 1,
            ..Default::default()
        },
    );
    let err = s("summarize".into()).await.unwrap_err();
    assert!(
        err.to_string().contains("summarizer endpoint 500"),
        "exhausted error surfaces the last failure: {err}"
    );
}

/// Step 24.3 (#559): when the primary model's attempts all fail, the summary
/// falls back to the configured secondary model (a rung above the static
/// marker) rather than failing outright.
#[tokio::test(flavor = "multi_thread")]
async fn loop_summarizer_falls_back_to_secondary_model() {
    use wiremock::{Request, Respond};

    struct ByModel;
    impl Respond for ByModel {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            if body["model"] == "fallback-model" {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "FB SUM"}}))
            } else {
                ResponseTemplate::new(500) // the primary model always fails
            }
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ByModel)
        .mount(&server)
        .await;

    let s = make_loop_summarizer(
        server.uri(),
        "test-model".into(),
        newt_core::BackendKind::Ollama,
        None,
        None,
        SummarizerOpts {
            retries: 0,
            fallback_model: Some("fallback-model".into()),
            ..Default::default()
        },
    );
    let out = s("summarize".into()).await.unwrap();
    assert_eq!(out, "FB SUM", "fell back to the secondary model");
}

/// With no explicit fallback configured, the summarizer must not spend
/// another live turn auto-picking an installed Ollama model. The compression
/// pipeline turns the surfaced primary error into the static marker.
#[tokio::test(flavor = "multi_thread")]
async fn loop_summarizer_does_not_auto_pick_fallback_when_unset() {
    use wiremock::{Request, Respond};

    struct ByModel;
    impl Respond for ByModel {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            // The installed small model would succeed, but it was not
            // explicitly configured as the summarizer fallback.
            if body["model"] == "nemotron-mini:4b" {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "UNCONFIGURED FB"}}))
            } else {
                ResponseTemplate::new(500) // the primary model always fails
            }
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ByModel)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "session-model:27b"}, {"name": "nemotron-mini:4b"}]
        })))
        .expect(0)
        .mount(&server)
        .await;

    let s = make_loop_summarizer(
        server.uri(),
        "session-model:27b".into(),
        newt_core::BackendKind::Ollama,
        None,
        None,
        SummarizerOpts {
            retries: 0,
            fallback_model: None,
            ..Default::default()
        },
    );
    let err = s("summarize".into()).await.unwrap_err();
    assert!(
        err.to_string().contains("summarizer endpoint 500"),
        "the primary error should surface for static-marker compression: {err}"
    );
}

/// Step 24.10 (#559): a `summarizer.toml` with its own backend overrides
/// every session backend field; an explicit key is used for the pinned host.
#[test]
fn resolve_summarizer_backend_overrides_when_set() {
    let sum_cfg = newt_core::SummarizerConfig {
        endpoint: Some("http://REDACTED-HOST:11434".into()),
        model: Some("qwen2.5-1.5b".into()),
        kind: Some(newt_core::BackendKind::Embedded),
        model_path: Some("/models/qwen.gguf".into()),
        ..Default::default()
    };
    let (url, model, kind, key, model_path) = super::resolve_summarizer_backend(
        &sum_cfg,
        "http://REDACTED-HOST:11434",
        "session-model:27b",
        newt_core::BackendKind::Ollama,
        &Some("session-key".into()),
        None, // override set ⇒ embedded lookup unused; keep hermetic
    );
    assert_eq!(url, "http://REDACTED-HOST:11434");
    assert_eq!(model, "qwen2.5-1.5b");
    assert_eq!(kind, newt_core::BackendKind::Embedded);
    // #661 group C: the GGUF path threads through for an embedded summarizer.
    assert_eq!(model_path.as_deref(), Some("/models/qwen.gguf"));
    // No key configured on the pinned endpoint → the session key is NOT
    // leaked to the different host.
    assert_eq!(key, None);
}

#[tokio::test]
async fn embedded_summarizer_without_a_model_fails_cleanly() {
    // #661 group C: kind=embedded with no model_path (or a build lacking the
    // `embedded` feature) yields a failing summarizer — the compressor then
    // degrades to the deterministic static marker (group D), never a panic.
    let s = make_loop_summarizer(
        "http://unused".into(),
        "qwen2.5-1.5b".into(),
        newt_core::BackendKind::Embedded,
        None,
        None, // no model_path
        SummarizerOpts::default(),
    );
    let out = s("summarize this".to_string()).await;
    assert!(
        out.is_err(),
        "an embedded summarizer with no model must fail (→ static marker), not panic"
    );
}

/// Step 24.10: an absent/default `summarizer.toml` reuses the session
/// backend verbatim (unchanged behavior), session key included.
#[test]
fn resolve_summarizer_backend_reuses_session_when_unset() {
    let sum_cfg = newt_core::SummarizerConfig::default();
    let (url, model, kind, key, _model_path) = super::resolve_summarizer_backend(
        &sum_cfg,
        "http://REDACTED-HOST:11434",
        "session-model:27b",
        newt_core::BackendKind::Ollama,
        &Some("session-key".into()),
        None, // no on-host model ⇒ deterministically degrade to session (hermetic)
    );
    assert_eq!(url, "http://REDACTED-HOST:11434");
    assert_eq!(model, "session-model:27b");
    assert_eq!(kind, newt_core::BackendKind::Ollama);
    assert_eq!(key.as_deref(), Some("session-key"));
}

/// Step 24.10: the timeout / retries / fallback knobs come from
/// `SummarizerConfig`; `keep_alive` falls back to `[tui].keep_alive`.
#[test]
fn summarizer_opts_reads_from_summarizer_config() {
    let sum_cfg = newt_core::SummarizerConfig {
        timeout_secs: 45,
        retries: 2,
        fallback_model: Some("nemotron-mini:4b".into()),
        ..Default::default()
    };
    let cfg = newt_core::Config::default();
    let opts = super::summarizer_opts(&sum_cfg, &cfg, Some(8192), false);
    assert_eq!(opts.timeout_secs, 45);
    assert_eq!(opts.retries, 2);
    assert_eq!(opts.fallback_model.as_deref(), Some("nemotron-mini:4b"));
    assert_eq!(opts.num_ctx, Some(8192));
    // No summarizer-specific keep_alive → inherits the [tui] default ("5m").
    assert_eq!(opts.keep_alive, "5m");
}

/// Step 24.7 (#559): the live retry/fallback notice text.
#[test]
fn summarizer_progress_message_text() {
    assert_eq!(
        super::retry_progress_msg(2, 3),
        "↻ summarizer retrying (attempt 2/3)…"
    );
    assert_eq!(
        super::fallback_progress_msg("qwen:0.5b"),
        "⚠ summarizer falling back to qwen:0.5b…"
    );
}

/// F5 mirror: OpenAI-compatible endpoints configure context server-side
/// — `num_ctx` must NOT leak into their request body.
#[tokio::test(flavor = "multi_thread")]
async fn loop_summarizer_omits_num_ctx_on_openai() {
    use std::sync::{Arc, Mutex};
    use wiremock::{Request, Respond};

    struct Capture {
        body: Arc<Mutex<Option<serde_json::Value>>>,
    }
    impl Respond for Capture {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            *self.body.lock().unwrap() = serde_json::from_slice(&req.body).ok();
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"choices": [{"message": {"content": "SUM"}}]}))
        }
    }

    let server = MockServer::start().await;
    let body = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Capture { body: body.clone() })
        .mount(&server)
        .await;

    let s = make_loop_summarizer(
        server.uri(),
        "test-model".into(),
        newt_core::BackendKind::Openai,
        Some("sk-test".into()),
        None,
        SummarizerOpts {
            num_ctx: Some(4_096),
            ..Default::default()
        },
    );
    let out = s("summarize the middle".into()).await.unwrap();
    assert_eq!(out, "SUM");
    let captured = body.lock().unwrap().clone().expect("request captured");
    assert!(
        captured.get("options").is_none(),
        "num_ctx is Ollama-only; OpenAI windows are server-side"
    );
}

/// Step 18.5 (#247): the `Summarizing` provider rebased onto the shared
/// path — one over-budget sync drives exactly ONE call to the (mocked)
/// summarizer endpoint through the same `make_loop_summarizer` wiring the
/// loop uses, the request carries the shared pipeline's template, and the
/// resulting history entry carries the pipeline's compaction markers.
#[tokio::test(flavor = "multi_thread")]
async fn summarizing_provider_delegates_through_loop_summarizer() {
    use std::sync::{Arc, Mutex};
    use wiremock::{Request, Respond};

    struct Capture {
        bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl Respond for Capture {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            self.bodies
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&req.body).unwrap());
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": "WIRE SUMMARY"}}))
        }
    }

    let server = MockServer::start().await;
    let bodies = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(Capture {
            bodies: bodies.clone(),
        })
        .mount(&server)
        .await;

    let mut memory = newt_core::MemoryManager::new();
    // Leave enough room for the irreducible active-prompt metadata + exact
    // user pair. A smaller authoritative budget must refuse compression
    // rather than summarize either half of that pair.
    memory.add_provider(
        newt_core::Summarizing::new(512).with_summarizer(make_loop_summarizer(
            server.uri(),
            "test-model".into(),
            newt_core::BackendKind::Ollama,
            None,
            None,
            SummarizerOpts {
                num_ctx: Some(512),
                ..Default::default()
            },
        )),
    );
    let metrics = |input_tokens: u32| newt_core::TurnMetrics {
        usage: Some(newt_core::TokenUsage {
            input_tokens,
            output_tokens: 9,
        }),
        ..Default::default()
    };
    let big = "x".repeat(200);
    for i in 0..5u32 {
        memory
            .sync_all(&format!("task {i}"), &big, &metrics(10 + i))
            .await;
    }
    assert!(bodies.lock().unwrap().is_empty(), "under budget — no calls");
    memory.sync_all("final task", &big, &metrics(600)).await;

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1, "exactly one summarizer call");
    let prompt = bodies[0]["messages"][0]["content"].as_str().unwrap();
    assert!(
        prompt.contains("## Conversation middle to summarise"),
        "must be the shared pipeline's request template"
    );
    drop(bodies);
    // The minted record carries the shared markers.
    let record = memory
        .take_compaction_record()
        .expect("compression minted a record");
    assert!(record.starts_with(newt_core::agentic::SUMMARY_PREFIX));
    assert!(record.contains("WIRE SUMMARY"));
    assert!(record.contains(newt_core::agentic::SUMMARY_END_MARKER));
}

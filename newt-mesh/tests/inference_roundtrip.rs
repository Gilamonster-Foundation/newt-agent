//! End-to-end smoke: a responder newt and an asker newt in the same
//! process exchange an inference request and reply, using
//! [`tests_common::MockBackend`] to keep the test fast and
//! deterministic. This is the canonical mock-mode proof that the
//! agent-mesh-bus + newt-inference wiring round-trips end-to-end.
//!
//! No external services. The bus + transport stack runs entirely
//! in-process on ephemeral UDP ports.

use std::sync::Arc;
use std::time::Duration;

use agent_mesh_core::{AgentKey, AgentMetadata, UserKey};
use newt_mesh::{InferenceRequest, MeshAsker, NewtMeshService};
use tests_common::MockBackend;

fn agent(user: &UserKey, role: &str, caps: Vec<String>) -> AgentKey {
    AgentKey::issue(
        user,
        AgentMetadata {
            role: role.into(),
            host: "test".into(),
            capabilities: caps,
            issued_at: "2026-05-29T12:00:00Z".into(),
            expires_at: None,
        },
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_receives_inference_reply() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let user = UserKey::generate();

    let responder_agent = agent(&user, "test-responder", vec!["newt-inference".to_string()]);
    let asker_agent = agent(&user, "test-asker", vec!["newt-asker".to_string()]);
    let responder_fp = responder_agent.fingerprint();

    let backend: Arc<dyn newt_inference::backend::InferenceBackend> =
        Arc::new(MockBackend::all_tiers("rt-backend", "rename complete"));
    let backend_model = backend.model_id().to_string();

    let service = NewtMeshService::bind(&user, responder_agent, backend, 0)
        .await
        .expect("bind service");

    let asker = MeshAsker::bind(&user, asker_agent)
        .await
        .expect("bind asker");

    // Give mDNS a moment to settle so the asker's bus can resolve the
    // responder by fingerprint.
    tokio::time::sleep(Duration::from_millis(750)).await;

    let req = InferenceRequest {
        prompt: "rename foo to bar".into(),
        tier: None,
        model: None,
        max_tokens: Some(256),
    };

    let reply = asker
        .ask(responder_fp, req, Duration::from_secs(10))
        .await
        .expect("ask round-trip");

    assert!(
        !reply.is_error(),
        "responder reported error: {:?}",
        reply.error
    );
    assert_eq!(reply.content, "rename complete");
    assert_eq!(reply.model_id, backend_model);

    service.close().await.unwrap();
    asker.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_surfaces_responder_error_for_bad_model_pin() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let user = UserKey::generate();

    let responder_agent = agent(&user, "test-responder", vec!["newt-inference".to_string()]);
    let asker_agent = agent(&user, "test-asker", vec!["newt-asker".to_string()]);
    let responder_fp = responder_agent.fingerprint();

    let backend: Arc<dyn newt_inference::backend::InferenceBackend> =
        Arc::new(MockBackend::all_tiers("rt-backend", "ignored"));

    let service = NewtMeshService::bind(&user, responder_agent, backend, 0)
        .await
        .expect("bind service");

    let asker = MeshAsker::bind(&user, asker_agent)
        .await
        .expect("bind asker");

    tokio::time::sleep(Duration::from_millis(750)).await;

    let req = InferenceRequest {
        prompt: "irrelevant".into(),
        tier: None,
        model: Some("model-that-does-not-exist".into()),
        max_tokens: None,
    };

    let reply = asker
        .ask(responder_fp, req, Duration::from_secs(10))
        .await
        .expect("ask round-trip");

    assert!(
        reply.is_error(),
        "expected error reply, got content={}",
        reply.content
    );
    let msg = reply.error.unwrap();
    assert!(
        msg.contains("model-that-does-not-exist"),
        "error did not mention the bad pin: {msg}"
    );

    service.close().await.unwrap();
    asker.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn service_reports_backend_metadata() {
    // Exercise the public getter surface so changes to NewtMeshService's
    // accessor shape stay backwards compatible.
    let user = UserKey::generate();
    let responder = agent(&user, "test-responder", vec!["newt-inference".to_string()]);

    let backend: Arc<dyn newt_inference::backend::InferenceBackend> =
        Arc::new(MockBackend::all_tiers("metadata-test", "x"));

    let service = NewtMeshService::bind(&user, responder, backend, 0)
        .await
        .expect("bind");

    assert_eq!(service.backend_name(), "metadata-test");
    assert_eq!(service.backend_model(), "metadata-test-model");
    assert_eq!(service.user_fingerprint(), user.fingerprint());
    assert!(service.local_port() > 0);

    service.close().await.unwrap();
}

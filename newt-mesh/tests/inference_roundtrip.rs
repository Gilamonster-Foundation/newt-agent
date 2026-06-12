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

use agent_mesh_core::{AgentKey, AgentMetadata, Caveats, UserKey};
use newt_mesh::{InferenceRequest, MeshAsker, MeshIntegrationError, NewtMeshService};
use tests_common::MockBackend;

mod util;

/// `true` iff the ask failed only because the responder's mDNS
/// announce hasn't propagated yet — the one transient first-contact
/// error [`util::with_announce_grace`] is allowed to retry (#274).
fn ask_unannounced(e: &MeshIntegrationError) -> bool {
    matches!(e, MeshIntegrationError::Bus(b) if util::bus_unannounced(b))
}

fn agent(user: &UserKey, role: &str, caps: Vec<String>) -> AgentKey {
    AgentKey::issue(
        user,
        AgentMetadata {
            role: role.into(),
            host: "test".into(),
            capabilities: caps,
            issued_at: "2026-05-29T12:00:00Z".into(),
            expires_at: None,
            caveats: Caveats::top(),
        },
    )
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(mesh_mdns)]
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

    // First contact: poll-with-deadline instead of a fixed mDNS-settle
    // sleep (#274) — passes the moment the responder's announce lands,
    // still fails if it never does.
    let reply = util::with_announce_grace(
        || {
            asker.ask(
                responder_fp,
                InferenceRequest {
                    prompt: "rename foo to bar".into(),
                    tier: None,
                    model: None,
                    max_tokens: Some(256),
                },
                Duration::from_secs(10),
            )
        },
        ask_unannounced,
    )
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
#[serial_test::serial(mesh_mdns)]
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

    // First contact via poll-with-deadline — see #274 note above.
    let reply = util::with_announce_grace(
        || {
            asker.ask(
                responder_fp,
                InferenceRequest {
                    prompt: "irrelevant".into(),
                    tier: None,
                    model: Some("model-that-does-not-exist".into()),
                    max_tokens: None,
                },
                Duration::from_secs(10),
            )
        },
        ask_unannounced,
    )
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
#[serial_test::serial(mesh_mdns)]
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

//! **Transport smoke only** (#1190). A responder newt and an asker newt in
//! the same process exchange one inference request/reply over the *real*
//! agent-mesh bus (iroh QUIC + mDNS on ephemeral UDP ports) — the proof that
//! the WIRE works: an asker can reach a responder and get a well-formed reply
//! back.
//!
//! This file deliberately asserts **transport**, not **logic**. The
//! responder's request→reply logic (reply content/model_id, malformed-request
//! handling, model-pin mismatch naming the bad pin) is covered deterministically
//! and socket-free by the `handle_inference_*` unit tests in
//! `src/service.rs` — the fully-mocked unit tier per the repo's testing law.
//! Re-asserting that logic here would only re-couple it to the live transport's
//! flakiness (the agent-mesh dial-back / link-local-address issue, #61/#62),
//! which is exactly the masking #1190 removes: a transport timeout and a logic
//! regression must not look identical.
//!
//! No external services; the bus runs entirely in-process. The live transport
//! remains flaky under CI until the agent-mesh dial-back fix (#61/#62) lands;
//! that flake is isolated to this cross-project `mesh-integration` lane.

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
// Live iroh transport (mDNS discovery + dial-back). Flaky on CI runners until
// the agent-mesh dial-back fix (#61/#62); runs nightly via `--include-ignored`,
// not on the per-PR gate (#1190). The responder logic it exercises is covered
// deterministically by the `handle_inference_*` unit tests in src/service.rs.
#[ignore = "live transport — nightly only (#1190; agent-mesh #61/#62)"]
async fn ask_receives_inference_reply() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let user = UserKey::generate();

    let responder_agent = agent(&user, "test-responder", vec!["newt-inference".to_string()]);
    let asker_agent = agent(&user, "test-asker", vec!["newt-asker".to_string()]);
    let responder_fp = responder_agent.fingerprint();

    let backend: Arc<dyn newt_inference::backend::InferenceBackend> =
        Arc::new(MockBackend::all_tiers("rt-backend", "rename complete"));

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

    // Assert the WIRE, not the logic (#1190): a well-formed reply came back
    // across the real transport — non-error, with the body intact (a non-empty
    // content proves the responder's bytes survived the round-trip). The exact
    // content/model_id are the responder logic's contract, pinned socket-free by
    // the `handle_inference_*` unit tests in src/service.rs.
    assert!(
        !reply.is_error(),
        "responder reported error: {:?}",
        reply.error
    );
    assert!(
        !reply.content.is_empty() && !reply.model_id.is_empty(),
        "reply body did not survive the wire: {reply:?}"
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

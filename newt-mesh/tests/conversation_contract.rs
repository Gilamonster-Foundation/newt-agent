//! Cross-project wire-contract regression tests: a **raw
//! `agent_mesh_bus::Bus` client** (no newt client code) talking to a
//! [`NewtMeshService`] responder.
//!
//! `inference_roundtrip.rs` proves newt's own `MeshAsker` can reach a
//! `NewtMeshService`; both ends there are newt code, so a protocol
//! change on the newt side could slip through with both halves moving
//! together. These tests pin the seam from the *agent-mesh* side:
//!
//! 1. the topic string (`newt/inference/v1` under the user namespace),
//! 2. the `InferenceRequest` / `InferenceReply` JSON shapes, built and
//!    parsed by hand — if either side renames a field, this fails,
//! 3. a multi-turn threaded conversation (the client carries the
//!    transcript; the responder is stateless per request),
//! 4. a **quiet-bind** client (`BusOptions { announce: false }`) whose
//!    replies can only arrive via the bus's dial-back path — mDNS
//!    resolution of the client is impossible by construction.
//!
//! All keys are generated in-memory; no filesystem or external
//! services. The bus + transport stack runs in-process on ephemeral
//! UDP ports.

use std::sync::Arc;
use std::time::Duration;

use agent_mesh_bus::{Bus, BusOptions, Topic};
use agent_mesh_core::{AgentKey, AgentMetadata, Caveats, Fingerprint, UserKey};
use async_trait::async_trait;
use newt_inference::backend::{ChatReply, ChatRequest, InferenceBackend};
use newt_mesh::NewtMeshService;

mod util;

/// The wire topic — spelled out by hand on purpose. If
/// `newt_mesh::INFERENCE_TOPIC` ever drifts from this string, the
/// responder will never see these requests and the tests time out:
/// that's the regression this file exists to catch.
const WIRE_TOPIC: &str = "newt/inference/v1";

fn agent(user: &UserKey, role: &str) -> AgentKey {
    AgentKey::issue(
        user,
        AgentMetadata {
            role: role.into(),
            host: "test".into(),
            capabilities: vec![role.to_string()],
            issued_at: "2026-06-07T12:00:00Z".into(),
            expires_at: None,
            caveats: Caveats::top(),
        },
    )
}

/// Deterministic backend whose reply is a function of the prompt, so
/// a multi-turn test can assert the transcript actually reached it.
/// Replies with `turns=<n> last=<last line of the prompt>` where `n`
/// is the number of `User:` lines in the prompt.
struct EchoBackend;

#[async_trait]
impl InferenceBackend for EchoBackend {
    fn name(&self) -> &str {
        "echo"
    }
    fn model_id(&self) -> &str {
        "echo-model"
    }
    fn supports_tier(&self, _tier: newt_core::router::Tier) -> bool {
        true
    }
    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatReply> {
        let prompt = req
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        // Count only lines that BEGIN with "User:" — assistant echo
        // lines embed the substring and must not be counted.
        let user_lines = prompt.lines().filter(|l| l.starts_with("User:")).count();
        let last = prompt.lines().last().unwrap_or("").to_string();
        Ok(ChatReply {
            content: format!("turns={user_lines} last={last}"),
            model_id: self.model_id().to_string(),
            usage: None,
        })
    }
}

/// Bind a responder around `EchoBackend`, returning the service guard
/// and its fingerprint.
async fn bind_echo_responder(user: &UserKey) -> (NewtMeshService, Fingerprint) {
    let responder_agent = agent(user, "newt-inference");
    let fp = responder_agent.fingerprint();
    let backend: Arc<dyn InferenceBackend> = Arc::new(EchoBackend);
    let service = NewtMeshService::bind(user, responder_agent, backend, 0)
        .await
        .expect("bind responder");
    (service, fp)
}

/// Send one hand-built `InferenceRequest` JSON over a raw bus and
/// parse the reply JSON by hand.
///
/// mDNS first contact uses poll-with-deadline (#274): only the
/// "peer not announced within …" resolve failure is retried, so the
/// wire-contract regressions this file pins (renamed fields, drifted
/// topic ⇒ handler timeout) still fail loudly on the first attempt.
async fn raw_ask(bus: &Bus, responder_fp: Fingerprint, prompt: &str) -> serde_json::Value {
    let topic = Topic::new(bus.user_fingerprint(), WIRE_TOPIC);
    let body = serde_json::json!({
        "prompt": prompt,
        "tier": null,
        "model": null,
        "max_tokens": 64,
    });
    let payload = serde_json::to_vec(&body).unwrap();
    let reply_bytes = util::with_announce_grace(
        || {
            bus.request(
                responder_fp,
                &topic,
                payload.clone(),
                Duration::from_secs(10),
            )
        },
        util::bus_unannounced,
    )
    .await
    .expect("bus request");
    serde_json::from_slice(&reply_bytes).expect("reply must be JSON")
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(mesh_mdns)]
#[ignore = "live transport — nightly only (#1190; agent-mesh #61/#62)"]
async fn raw_bus_client_round_trips_the_wire_contract() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let user = UserKey::generate();
    let (service, responder_fp) = bind_echo_responder(&user).await;

    let client = Bus::bind(&user, agent(&user, "raw-client"), 0)
        .await
        .expect("bind client");
    // No fixed mDNS-settle sleep: `raw_ask` polls with a deadline on
    // first contact (#274).
    let reply = raw_ask(&client, responder_fp, "hello mesh").await;

    // Pin the reply shape field-by-field.
    assert_eq!(reply["model_id"], "echo-model");
    assert!(
        reply["error"].is_null(),
        "expected no error, got {:?}",
        reply["error"]
    );
    assert_eq!(reply["content"], "turns=0 last=hello mesh");

    client.close().await.expect("close client");
    service.close().await.expect("close service");
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(mesh_mdns)]
#[ignore = "live transport — nightly only (#1190; agent-mesh #61/#62)"]
async fn raw_bus_client_holds_a_threaded_conversation() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let user = UserKey::generate();
    let (service, responder_fp) = bind_echo_responder(&user).await;

    let client = Bus::bind(&user, agent(&user, "conversing-client"), 0)
        .await
        .expect("bind client");

    // The responder is stateless: the client threads the transcript.
    let mut transcript = String::new();
    for (i, q) in ["first question", "second question", "third question"]
        .iter()
        .enumerate()
    {
        let prompt = format!("{transcript}User: {q}");
        let reply = raw_ask(&client, responder_fp, &prompt).await;
        let content = reply["content"].as_str().expect("content is a string");

        // The echo backend counts `User:` lines — proving the whole
        // transcript (not just the newest question) crossed the mesh.
        assert_eq!(
            content,
            format!("turns={} last=User: {q}", i + 1),
            "turn {} must carry the full transcript",
            i + 1
        );
        transcript.push_str(&format!("User: {q}\nAssistant: {content}\n"));
    }

    client.close().await.expect("close client");
    service.close().await.expect("close service");
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(mesh_mdns)]
#[ignore = "live transport — nightly only (#1190; agent-mesh #61/#62)"]
async fn quiet_client_gets_replies_via_dial_back() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let user = UserKey::generate();
    let (service, responder_fp) = bind_echo_responder(&user).await;

    // announce: false — the responder can never resolve this client
    // over mDNS, so the reply only arrives if the bus dials back the
    // request's source address (agent-mesh ≥ 0.6.2).
    let client = Bus::bind_with(
        &user,
        agent(&user, "quiet-client"),
        0,
        BusOptions { announce: false },
    )
    .await
    .expect("bind quiet client");

    let reply = raw_ask(&client, responder_fp, "quiet hello").await;
    assert_eq!(reply["content"], "turns=0 last=quiet hello");

    client.close().await.expect("close client");
    service.close().await.expect("close service");
}

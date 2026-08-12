//! dock — the agent-mesh transport for newt-web docking (requirement 2).
//!
//! A hub asks a peer newt-agent to LIST its sessions, MIRROR one session's
//! transcript, or ENQUEUE a prompt, carried over the bus's request/reply on the
//! `newt/dock/v1` topic. This is the [`crate::NewtDockService`] responder + the
//! [`DockClient`] dialer; newt-web's `dock::DockSource` MVP HTTP backend swaps to
//! this for the real cross-machine transport (`docs/decisions/newt_web_docking`
//! K7). The richer duplex `session_streams` primitive (live push) is a later
//! refinement — request/reply covers list/mirror/inject.
//!
//! Authorization: same operator is *authentication*, not authorization. The bus
//! handshake proves the caller shares the operator `UserKey`, but the responder
//! still resolves the VERIFIED caller agent fingerprint (from
//! [`RequestContext`], the envelope signer) against its OWN signed dock registry
//! and enforces the approved [`DockScope`] per operation — so a sibling agent the
//! operator never approved is refused here, at the resource owner, not merely on
//! the bypassable dialer. Fail-closed by default; `NEWT_INSECURE_DOCK_NO_APPROVAL`
//! is the named unsafe opt-out. **D2 across the mesh:** `Inject` enqueues into the
//! peer's own store inbox via `ConversationStore::inject_prompt`; the peer's own
//! REPL consumes it and stays the sole writer — the hub never writes a remote
//! transcript.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_mesh_bus::{Bus, PeerEndpoint, RequestContext, Topic};
use agent_mesh_core::{AgentKey, Fingerprint, UserKey};
use serde::{Deserialize, Serialize};

/// Topic (under the operator's user namespace) for dock requests.
pub const DOCK_TOPIC: &str = "newt/dock/v1";
/// Capability tag a dockable agent advertises in mDNS so a hub can pre-filter.
pub const DOCK_CAPABILITY_TAG: &str = "newt-session";

const DOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// A dock request from a hub to a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DockRequest {
    /// List the peer's sessions.
    ListSessions,
    /// Mirror one session's transcript (read-only).
    Transcript { conv: String },
    /// Enqueue a prompt into the peer's session (D2 — the peer runs it).
    Inject { conv: String, text: String },
}

/// A dock reply from a peer to a hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DockReply {
    Sessions(Vec<DockSessionInfo>),
    Transcript(DockTranscript),
    Injected,
    NotFound,
    Error(String),
}

/// One remote session (the wire twin of newt-web's `dock::DockedSession`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockSessionInfo {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub turns: usize,
    pub live: bool,
}

/// A mirrored transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockTranscript {
    pub title: String,
    pub turns: Vec<DockTurn>,
}

/// One `(user, assistant)` turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockTurn {
    pub user: String,
    pub assistant: String,
}

/// Service a single decoded dock request against the store at `state_dir`.
/// The caller's authorization on THIS responder. `None` means enforcement is
/// off (the explicit unsafe opt-out — serve any same-operator caller); `Some` is
/// the scope the caller's approved dock is limited to, enforced per operation.
type Authz = Option<newt_core::dock_registry::DockScope>;

/// Whether the responder-side approved-dock gate is disabled. Fail-closed by
/// default: a caller must be in this peer's signed dock registry unless the
/// named unsafe opt-out is set (loopback dev / raw-transport testing).
fn dock_approval_disabled() -> bool {
    std::env::var("NEWT_INSECURE_DOCK_NO_APPROVAL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Authorize the caller (verified agent fingerprint) against THIS peer's signed
/// dock registry. This is the resource-owning responder's own decision — same
/// operator is authentication, not authorization, so a sibling agent the
/// operator never approved is refused here even though the handshake admitted
/// it. Re-reads the registry per request, so a revocation committed before the
/// request denies (revocation linearization).
fn authorize_caller(state_dir: &Path, caller_agent_fp: &str) -> Result<Authz, DockReply> {
    if dock_approval_disabled() {
        return Ok(None);
    }
    let config = state_dir.join("config.toml");
    let identity = state_dir.join("identity.pem");
    let (registry, _warnings) =
        newt_core::dock_registry::load_docks_with_identity(&config, &identity);
    match registry.approved(caller_agent_fp) {
        Some(record) => Ok(Some(record.scope)),
        None => Err(DockReply::Error(format!(
            "caller {}… is not an approved dock on this peer (run `newt dock approve` here)",
            &caller_agent_fp[..12.min(caller_agent_fp.len())]
        ))),
    }
}

/// Synchronous (SQLite); the async handler runs it on a blocking thread.
fn handle_dock(state_dir: &Path, authz: Authz, req: DockRequest) -> DockReply {
    // The operator's dock-exposure kill-switch (requirement 7): a marker file in
    // the state dir. Fail-closed — while present, every dock request is refused
    // over the MESH too, not only over HTTP, so a forcible undock is complete
    // across transports.
    if state_dir.join("dock-exposure-disabled").exists() {
        return DockReply::Error("dock exposure disabled by the operator".into());
    }
    // Per-operation scope enforcement (skipped only under the unsafe opt-out):
    // a Mirror dock may read but not inject.
    if let Some(scope) = authz {
        let permitted = match &req {
            DockRequest::ListSessions | DockRequest::Transcript { .. } => scope.allows_read(),
            DockRequest::Inject { .. } => scope.allows_inject(),
        };
        if !permitted {
            return DockReply::Error(format!(
                "caller's dock scope `{}` does not permit this operation",
                scope.as_wire()
            ));
        }
    }
    use newt_core::ConversationStore;
    // `list_all` is cross-workspace, so the workspace arg here is irrelevant; the
    // transcript/inject paths re-open FENCED at the conversation's own workspace.
    let open = |ws: &Path| ConversationStore::new(state_dir, ws, 1000);
    match req {
        DockRequest::ListSessions => match open(state_dir).and_then(|s| {
            let list = s.list_all()?;
            Ok(list
                .into_iter()
                .take(30)
                .map(|(c, workspace)| {
                    let live = s
                        .live_owner(&c.id)
                        .ok()
                        .flatten()
                        .is_some_and(|owner| s.is_owner_live(&owner));
                    DockSessionInfo {
                        id: c.id,
                        title: c.title,
                        workspace,
                        turns: c.turn_count,
                        live,
                    }
                })
                .collect::<Vec<_>>())
        }) {
            Ok(sessions) => DockReply::Sessions(sessions),
            Err(e) => DockReply::Error(e.to_string()),
        },
        DockRequest::Transcript { conv } => match resolve_ws(state_dir, &conv) {
            None => DockReply::NotFound,
            Some(ws) => match open(Path::new(&ws)).and_then(|s| s.load(&conv)) {
                Ok(rec) => DockReply::Transcript(DockTranscript {
                    title: rec.title,
                    turns: rec
                        .turns
                        .iter()
                        .map(|t| DockTurn {
                            user: t.user.clone(),
                            assistant: t.assistant.clone(),
                        })
                        .collect(),
                }),
                Err(_) => DockReply::NotFound,
            },
        },
        DockRequest::Inject { conv, text } => match resolve_ws(state_dir, &conv) {
            None => DockReply::NotFound,
            Some(ws) => {
                match open(Path::new(&ws)).and_then(|s| s.inject_prompt(&conv, &text, None)) {
                    Ok(_) => DockReply::Injected,
                    Err(e) => DockReply::Error(e.to_string()),
                }
            }
        },
    }
}

/// The workspace path a conversation belongs to (store `load`/`inject` are
/// workspace-fenced, so the caller need not know it).
fn resolve_ws(state_dir: &Path, conv: &str) -> Option<String> {
    newt_core::ConversationStore::new(state_dir, state_dir, 1000)
        .ok()?
        .list_all()
        .ok()?
        .into_iter()
        .find(|(c, _)| c.id == conv)
        .map(|(_, w)| w)
}

/// The dock **responder**: binds a bus and answers dock requests over
/// `newt/dock/v1` from the store at `state_dir`. Mirrors `NewtMeshService`.
pub struct NewtDockService {
    bus: Bus,
    agent_pubkey: [u8; 32],
}

impl NewtDockService {
    /// Bind on `port` (0 = ephemeral) and serve docks from `state_dir`.
    ///
    /// # Errors
    /// Propagates a bus bind failure.
    pub async fn bind(
        user: &UserKey,
        agent: AgentKey,
        state_dir: PathBuf,
        port: u16,
    ) -> anyhow::Result<Self> {
        let agent_pubkey = agent.verifying_key().to_bytes();
        let user_fp = user.fingerprint();
        let bus = Bus::bind(user, agent, port).await?;
        let topic = Topic::new(user_fp, DOCK_TOPIC);
        // handle_requests_with_context gives us the VERIFIED caller principal
        // (the envelope signer), so the responder authorizes WHICH agent is
        // dialing against its own signed registry — complete mediation at the
        // resource owner, not merely on the (bypassable) dialer.
        bus.handle_requests_with_context(topic, move |ctx: RequestContext, body| {
            let state_dir = state_dir.clone();
            let caller_agent_fp = ctx.caller_agent_fp.hex();
            async move {
                let reply = tokio::task::spawn_blocking(move || {
                    // Authorize the caller FIRST — refuse before any disclosure
                    // or side effect.
                    let authz = match authorize_caller(&state_dir, &caller_agent_fp) {
                        Ok(authz) => authz,
                        Err(deny) => return deny,
                    };
                    match serde_json::from_slice(&body) {
                        Ok(req) => handle_dock(&state_dir, authz, req),
                        Err(e) => DockReply::Error(format!("bad dock request: {e}")),
                    }
                })
                .await
                .unwrap_or_else(|e| DockReply::Error(format!("dock handler panicked: {e}")));
                Ok(serde_json::to_vec(&reply).unwrap_or_default())
            }
        });
        Ok(Self { bus, agent_pubkey })
    }

    /// The raw agent pubkey a hub needs to build a [`PeerEndpoint`].
    #[must_use]
    pub fn agent_pubkey(&self) -> [u8; 32] {
        self.agent_pubkey
    }
    /// The bound UDP port.
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.bus.local_port()
    }
    /// This responder's agent fingerprint.
    #[must_use]
    pub fn agent_fingerprint(&self) -> Fingerprint {
        self.bus.agent_fingerprint()
    }
    /// Close the bus.
    ///
    /// # Errors
    /// Propagates a bus close failure.
    pub async fn close(self) -> anyhow::Result<()> {
        Ok(self.bus.close().await?)
    }
}

/// The dock **dialer**: a hub-side bus that requests docks from peers.
pub struct DockClient {
    bus: Bus,
    user_fp: Fingerprint,
}

impl DockClient {
    /// Bind a hub-side dial bus (0 = ephemeral port).
    ///
    /// # Errors
    /// Propagates a bus bind failure.
    pub async fn bind(user: &UserKey, agent: AgentKey, port: u16) -> anyhow::Result<Self> {
        let user_fp = user.fingerprint();
        let bus = Bus::bind(user, agent, port).await?;
        Ok(Self { bus, user_fp })
    }

    async fn request(&self, peer: PeerEndpoint, req: &DockRequest) -> anyhow::Result<DockReply> {
        let topic = Topic::new(self.user_fp, DOCK_TOPIC);
        let body = serde_json::to_vec(req)?;
        let reply = self
            .bus
            .request_direct(peer, &topic, body, DOCK_TIMEOUT)
            .await?;
        Ok(serde_json::from_slice(&reply)?)
    }

    /// List a peer's sessions.
    ///
    /// # Errors
    /// Bus/transport failure, or a non-`Sessions` reply.
    pub async fn list_sessions(&self, peer: PeerEndpoint) -> anyhow::Result<Vec<DockSessionInfo>> {
        match self.request(peer, &DockRequest::ListSessions).await? {
            DockReply::Sessions(s) => Ok(s),
            other => Err(anyhow::anyhow!("unexpected dock reply: {other:?}")),
        }
    }

    /// Mirror a peer session's transcript.
    ///
    /// # Errors
    /// Bus/transport failure, `NotFound`, or an unexpected reply.
    pub async fn transcript(
        &self,
        peer: PeerEndpoint,
        conv: &str,
    ) -> anyhow::Result<DockTranscript> {
        match self
            .request(peer, &DockRequest::Transcript { conv: conv.into() })
            .await?
        {
            DockReply::Transcript(t) => Ok(t),
            DockReply::NotFound => Err(anyhow::anyhow!("no such session on the peer")),
            other => Err(anyhow::anyhow!("unexpected dock reply: {other:?}")),
        }
    }

    /// Enqueue a prompt into a peer session (D2 — the peer runs it).
    ///
    /// # Errors
    /// Bus/transport failure, `NotFound`, or an unexpected reply.
    pub async fn inject(&self, peer: PeerEndpoint, conv: &str, text: &str) -> anyhow::Result<()> {
        match self
            .request(
                peer,
                &DockRequest::Inject {
                    conv: conv.into(),
                    text: text.into(),
                },
            )
            .await?
        {
            DockReply::Injected => Ok(()),
            DockReply::NotFound => Err(anyhow::anyhow!("no such session on the peer")),
            other => Err(anyhow::anyhow!("unexpected dock reply: {other:?}")),
        }
    }

    /// Close the dial bus.
    ///
    /// # Errors
    /// Propagates a bus close failure.
    pub async fn close(self) -> anyhow::Result<()> {
        Ok(self.bus.close().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_mesh_core::{AgentMetadata, Caveats};
    use std::net::{IpAddr, Ipv4Addr};

    fn agent(user: &UserKey, role: &str, caps: Vec<String>) -> AgentKey {
        AgentKey::issue(
            user,
            AgentMetadata {
                role: role.into(),
                host: "test".into(),
                capabilities: caps,
                issued_at: "2026-08-11T00:00:00Z".into(),
                expires_at: None,
                caveats: Caveats::top(),
            },
        )
    }

    fn loopback(pubkey: [u8; 32], port: u16) -> PeerEndpoint {
        PeerEndpoint::from_parts(pubkey, IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    /// Seed `state_dir`'s registry so the responder approves `caller_pubkey` at
    /// `scope` — the responder-side half of a dock (the peer approving a hub
    /// agent). Signs with the operator `user`, whose identity is written so the
    /// responder's `load_docks_with_identity` can verify.
    fn approve_caller(
        user: &UserKey,
        state_dir: &Path,
        caller_pubkey: &[u8; 32],
        scope: DockScope,
    ) {
        let config = state_dir.join("config.toml");
        let identity = state_dir.join("identity.pem");
        if !identity.exists() {
            user.save(&identity).unwrap();
        }
        let fp = newt_core::dock_registry::agent_fingerprint_of_pubkey(caller_pubkey);
        let hex: String = caller_pubkey.iter().map(|b| format!("{b:02x}")).collect();
        // Path-only signer: loads the key from identity.pem INTERNALLY, so no
        // UserKey type crosses the newt-mesh (path agent-mesh) / newt-core
        // (registry agent-mesh) seam.
        newt_core::dock_registry::approve_dock_with_identity(
            &config, &identity, &fp, "hub", &hex, scope, "tx",
        )
        .unwrap();
    }

    /// Pure dock-request handling against a seeded store — the DETERMINISTIC
    /// per-PR gate (no bus, no network), the twin of `service.rs`'s
    /// `handle_inference_*` unit tests. Grounds the protocol logic; the live
    /// transport is grounded by the ignored loopback-QUIC test below.
    use newt_core::dock_registry::DockScope;
    const MI: Authz = Some(DockScope::MirrorInject);

    #[test]
    fn handle_dock_lists_mirrors_and_injects() {
        let dir = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(dir.path(), dir.path(), 100).unwrap();
        let conv = store.create("a session", None).unwrap();
        store.append_turn(&conv, "q", "answer text").unwrap();

        match handle_dock(dir.path(), MI, DockRequest::ListSessions) {
            DockReply::Sessions(s) => {
                assert!(s.iter().any(|x| x.title == "a session"));
            }
            other => panic!("expected Sessions, got {other:?}"),
        }
        match handle_dock(
            dir.path(),
            MI,
            DockRequest::Transcript { conv: conv.clone() },
        ) {
            DockReply::Transcript(t) => {
                assert!(t.turns.iter().any(|x| x.assistant == "answer text"));
            }
            other => panic!("expected Transcript, got {other:?}"),
        }
        match handle_dock(
            dir.path(),
            MI,
            DockRequest::Inject {
                conv: conv.clone(),
                text: "INJ".into(),
            },
        ) {
            DockReply::Injected => {}
            other => panic!("expected Injected, got {other:?}"),
        }
        // D2: the inject landed in the peer's own inbox.
        assert_eq!(
            store.take_injected_prompt(&conv).unwrap().map(|p| p.body),
            Some("INJ".to_string())
        );
        // Unknown conversation → NotFound, never a panic.
        assert!(matches!(
            handle_dock(
                dir.path(),
                MI,
                DockRequest::Transcript {
                    conv: "nope".into()
                }
            ),
            DockReply::NotFound
        ));
    }

    #[test]
    fn a_mirror_scope_caller_can_read_but_never_inject() {
        let dir = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(dir.path(), dir.path(), 100).unwrap();
        let conv = store.create("a session", None).unwrap();
        store.append_turn(&conv, "q", "answer text").unwrap();

        let mirror: Authz = Some(DockScope::Mirror);
        // Reads are permitted.
        assert!(matches!(
            handle_dock(dir.path(), mirror, DockRequest::ListSessions),
            DockReply::Sessions(_)
        ));
        assert!(matches!(
            handle_dock(
                dir.path(),
                mirror,
                DockRequest::Transcript { conv: conv.clone() }
            ),
            DockReply::Transcript(_)
        ));
        // Inject is refused BEFORE the store is touched.
        match handle_dock(
            dir.path(),
            mirror,
            DockRequest::Inject {
                conv: conv.clone(),
                text: "SHOULD_NOT_LAND".into(),
            },
        ) {
            DockReply::Error(msg) => assert!(msg.contains("does not permit")),
            other => panic!("a mirror dock must refuse inject, got {other:?}"),
        }
        assert!(
            store.take_injected_prompt(&conv).unwrap().is_none(),
            "the refused inject must never reach the inbox"
        );
    }

    #[test]
    fn a_revoked_caller_is_denied_on_the_next_request_linearization() {
        // Approve a caller, confirm it authorizes, then revoke and confirm the
        // very next authorize_caller (which re-reads the registry) denies — the
        // revocation linearization the responder relies on.
        let user = UserKey::generate();
        let dir = tempfile::tempdir().unwrap();
        let caller_pubkey = [0x5au8; 32];
        approve_caller(&user, dir.path(), &caller_pubkey, DockScope::MirrorInject);
        let fp = newt_core::dock_registry::agent_fingerprint_of_pubkey(&caller_pubkey);

        assert!(
            matches!(authorize_caller(dir.path(), &fp), Ok(Some(_))),
            "the approved caller must authorize"
        );

        // Revoke (via the path-only identity signer) and re-check.
        let identity = dir.path().join("identity.pem");
        newt_core::dock_registry::revoke_dock_with_identity(
            &dir.path().join("config.toml"),
            &identity,
            &fp,
        )
        .unwrap();
        assert!(
            matches!(authorize_caller(dir.path(), &fp), Err(DockReply::Error(_))),
            "a revoked caller must be denied on the next request"
        );
    }

    #[test]
    fn an_unapproved_caller_is_refused_before_any_disclosure() {
        // authorize_caller with enforcement ON (no opt-out) and an empty
        // registry: the caller is not approved, so it is denied. Uses a distinct
        // env-free path — the registry simply has no matching record.
        let dir = tempfile::tempdir().unwrap();
        // No identity/registry seeded → nothing is approved → deny.
        let denied = authorize_caller(dir.path(), "deadbeefdeadbeef");
        assert!(
            matches!(denied, Err(DockReply::Error(ref m)) if m.contains("not an approved dock")),
            "an unapproved caller must be refused: {denied:?}"
        );
    }

    /// The operator's kill-switch (requirement 7) must fail-closed over the
    /// MESH, not only over HTTP: with the `dock-exposure-disabled` marker in the
    /// state dir, every dock request is refused, so a forcible undock is complete
    /// across transports. Grounds the drive harness's "forcibly undocked" mesh
    /// assertion at the per-PR tier.
    #[test]
    fn a_disabled_marker_refuses_every_dock_request_over_the_mesh() {
        let dir = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(dir.path(), dir.path(), 100).unwrap();
        let conv = store.create("a session", None).unwrap();
        store.append_turn(&conv, "q", "answer text").unwrap();

        // Without the marker the peer lists its session (unenforced authz here —
        // the kill-switch is orthogonal to the approved-dock gate).
        assert!(matches!(
            handle_dock(dir.path(), None, DockRequest::ListSessions),
            DockReply::Sessions(_)
        ));

        // The operator flips the kill-switch.
        std::fs::write(dir.path().join("dock-exposure-disabled"), b"").unwrap();

        for req in [
            DockRequest::ListSessions,
            DockRequest::Transcript { conv: conv.clone() },
            DockRequest::Inject {
                conv: conv.clone(),
                text: "INJ".into(),
            },
        ] {
            match handle_dock(dir.path(), None, req) {
                DockReply::Error(msg) => assert!(msg.contains("disabled")),
                other => panic!("disabled dock must refuse with Error, got {other:?}"),
            }
        }

        // The refused inject never reached the peer's inbox.
        assert!(store.take_injected_prompt(&conv).unwrap().is_none());
    }

    /// The full dock lifecycle over a REAL loopback bus (real envelopes,
    /// handshake, QUIC): list, mirror, and inject — the last one landing in the
    /// peer's own store inbox (D2). Same operator, so the handshake auto-teams.
    /// Ignored per the repo's live-transport convention (see
    /// `conversation_contract.rs`) — runs in the nightly / `--include-ignored`
    /// tier of `mesh-integration.yml`, not the per-PR gate.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live transport — nightly/full mesh-integration tier only"]
    async fn dock_lifecycle_over_loopback_mesh() {
        let user = UserKey::generate();

        // Seed the peer's store with one conversation + a turn.
        let dir = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(dir.path(), dir.path(), 100).unwrap();
        let conv = store.create("mesh session", None).unwrap();
        store
            .append_turn(&conv, "q1", "STUB_REPLY from the peer")
            .unwrap();
        store.claim(&conv).unwrap(); // become the live owner → `live: true` over the dock

        let svc = NewtDockService::bind(
            &user,
            agent(&user, "peer", vec![DOCK_CAPABILITY_TAG.into()]),
            dir.path().to_path_buf(),
            0,
        )
        .await
        .unwrap();

        // The responder is fail-closed: seed its OWN registry to approve the hub
        // agent (mirror+inject) so the authorized caller succeeds. Same operator
        // is not enough — the peer must have approved this specific agent.
        let hub_agent = agent(&user, "hub", vec!["hub".into()]);
        let hub_pubkey = hub_agent.verifying_key().to_bytes();
        approve_caller(&user, dir.path(), &hub_pubkey, DockScope::MirrorInject);
        let client = DockClient::bind(&user, hub_agent, 0).await.unwrap();
        let pubkey = svc.agent_pubkey();
        let port = svc.local_port();

        // LIST over the mesh.
        let sessions = client.list_sessions(loopback(pubkey, port)).await.unwrap();
        assert!(
            sessions.iter().any(|s| s.title == "mesh session" && s.live),
            "peer session should be listed and live: {sessions:?}"
        );

        // MIRROR the transcript over the mesh.
        let t = client
            .transcript(loopback(pubkey, port), &conv)
            .await
            .unwrap();
        assert!(
            t.turns
                .iter()
                .any(|turn| turn.assistant.contains("STUB_REPLY")),
            "transcript should carry the peer's turn"
        );

        // INJECT over the mesh → the peer's own store inbox (D2).
        client
            .inject(loopback(pubkey, port), &conv, "MESH_INJECT run the lints")
            .await
            .unwrap();
        let injected = store.take_injected_prompt(&conv).unwrap();
        assert_eq!(
            injected.map(|p| p.body),
            Some("MESH_INJECT run the lints".to_string()),
            "the mesh inject must land in the peer's own inbox (the peer stays sole writer)"
        );

        // An unknown conversation is NotFound, not a panic.
        assert!(client
            .transcript(loopback(pubkey, port), "nope")
            .await
            .is_err());

        svc.close().await.unwrap();
        client.close().await.unwrap();
    }

    /// THE keystone hostile test (PR #1643 security closure): three agents share
    /// ONE operator UserKey — A the resource-owning responder, B an APPROVED hub,
    /// C an UNAPPROVED sibling. Same operator is authentication, not
    /// authorization: C, which the operator never approved, must be denied at A's
    /// responder on EVERY operation — even though the mesh handshake admits it —
    /// before any disclosure or side effect. Real loopback QUIC.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live transport — nightly/full mesh-integration tier only"]
    async fn a_sibling_agent_the_operator_never_approved_is_denied_over_the_mesh() {
        let user = UserKey::generate();
        let dir = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(dir.path(), dir.path(), 100).unwrap();
        let conv = store.create("secret session", None).unwrap();
        store.append_turn(&conv, "q", "TOP SECRET answer").unwrap();
        store.claim(&conv).unwrap();

        // A = the resource-owning responder (fail-closed by default).
        let a = NewtDockService::bind(
            &user,
            agent(&user, "peer", vec![DOCK_CAPABILITY_TAG.into()]),
            dir.path().to_path_buf(),
            0,
        )
        .await
        .unwrap();
        let a_pubkey = a.agent_pubkey();
        let a_port = a.local_port();

        // B = an APPROVED hub; C = an UNAPPROVED sibling (same UserKey, distinct AgentKey).
        let b_agent = agent(&user, "approved-hub", vec!["hub".into()]);
        let b_pubkey = b_agent.verifying_key().to_bytes();
        approve_caller(&user, dir.path(), &b_pubkey, DockScope::MirrorInject);
        let b = DockClient::bind(&user, b_agent, 0).await.unwrap();
        let c = DockClient::bind(&user, agent(&user, "sibling-c", vec!["hub".into()]), 0)
            .await
            .unwrap();

        // B (approved) is served.
        assert!(
            b.list_sessions(loopback(a_pubkey, a_port)).await.is_ok(),
            "the approved hub B must be served"
        );

        // C (unapproved sibling) is DENIED on every operation.
        assert!(
            c.list_sessions(loopback(a_pubkey, a_port)).await.is_err(),
            "unapproved sibling C must not be able to LIST sessions"
        );
        assert!(
            c.transcript(loopback(a_pubkey, a_port), &conv)
                .await
                .is_err(),
            "C must not be able to read the transcript"
        );
        assert!(
            c.inject(loopback(a_pubkey, a_port), &conv, "C_INJECT malicious")
                .await
                .is_err(),
            "C must not be able to inject"
        );
        // And C's rejected inject never reached A's inbox (refused before the
        // side effect).
        assert!(
            store.take_injected_prompt(&conv).unwrap().is_none(),
            "the sibling's inject must never land in the peer's inbox"
        );

        a.close().await.unwrap();
        b.close().await.unwrap();
        c.close().await.unwrap();
    }
}

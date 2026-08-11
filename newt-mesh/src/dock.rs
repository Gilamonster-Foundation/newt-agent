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
//! Trust: the bus handshake refuses a peer under a different `UserKey`, so a
//! same-operator dock needs no extra check for phase 1 (the approved-dock
//! ceremony is Phase 3). **D2 across the mesh:** `Inject` enqueues into the
//! peer's own store inbox via `ConversationStore::inject_prompt`; the peer's own
//! REPL consumes it and stays the sole writer — the hub never writes a remote
//! transcript.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_mesh_bus::{Bus, PeerEndpoint, Topic};
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
/// Synchronous (SQLite); the async handler runs it on a blocking thread.
fn handle_dock(state_dir: &Path, req: DockRequest) -> DockReply {
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
        bus.handle_requests(topic, move |body| {
            let state_dir = state_dir.clone();
            async move {
                let reply =
                    tokio::task::spawn_blocking(move || match serde_json::from_slice(&body) {
                        Ok(req) => handle_dock(&state_dir, req),
                        Err(e) => DockReply::Error(format!("bad dock request: {e}")),
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

    /// The full dock lifecycle over a REAL loopback bus (real envelopes,
    /// handshake, QUIC): list, mirror, and inject — the last one landing in the
    /// peer's own store inbox (D2). Same operator, so the handshake auto-teams.
    #[tokio::test(flavor = "multi_thread")]
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
        let client = DockClient::bind(&user, agent(&user, "hub", vec!["hub".into()]), 0)
            .await
            .unwrap();
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
}

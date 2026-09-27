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
use agent_mesh_core::{AgentKey, AgentMetadata, Caveats, Fingerprint, UserKey};
use serde::{Deserialize, Serialize};

/// Topic (under the operator's user namespace) for dock requests.
pub const DOCK_TOPIC: &str = "newt/dock/v1";
/// Capability tag a dockable agent advertises in mDNS so a hub can pre-filter.
pub const DOCK_CAPABILITY_TAG: &str = "newt-session";

/// Which end of a dock this process is. Each end has its own identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockRole {
    /// The cockpit that lists, mirrors and injects: newt-web's [`DockClient`].
    Hub,
    /// The machine whose sessions are docked: the [`NewtDockService`] responder.
    Host,
}

impl DockRole {
    fn name(self) -> &'static str {
        match self {
            Self::Hub => "hub",
            Self::Host => "host",
        }
    }

    fn capabilities(self) -> Vec<String> {
        match self {
            Self::Hub => Vec::new(),
            Self::Host => vec![DOCK_CAPABILITY_TAG.to_string()],
        }
    }
}

/// This installation's dock agent key for `role`: **derived, never stored**
/// (`docs/decisions/newt_web_docking.md` K8.4).
///
/// A dock approval pins an agent's fingerprint, so that fingerprint must
/// survive a restart. The key is recomputed from the operator's root key with
/// `AgentKey::issue_derived`, which leaves agent-mesh's "agent private bytes are
/// never persisted" contract intact. The label carries `instance` because every
/// machine of one operator shares that root: without it, two hosts would derive
/// the same key — and the same mesh endpoint id.
#[must_use]
pub fn dock_agent(user: &UserKey, role: DockRole, instance: &str) -> AgentKey {
    let label = format!("newt/dock/v1/{}/{instance}", role.name());
    AgentKey::issue_derived(
        user,
        &label,
        AgentMetadata {
            role: format!("newt-dock-{}", role.name()),
            host: instance.to_string(),
            capabilities: role.capabilities(),
            issued_at: "2026-01-01T00:00:00Z".into(), // a claim; expiry is generation-based
            expires_at: None,
            caveats: Caveats::top(),
        },
    )
}

/// The file, in a state dir, naming this installation for [`dock_agent`].
pub const DOCK_INSTANCE_FILE: &str = "dock-instance";

/// This installation's dock instance name, from [`DOCK_INSTANCE_FILE`] in
/// `state_dir`, created with a random name on first use.
///
/// The name is not a secret — it only keeps two installations' derived keys
/// apart — so it may be stored. It does select which key the operator root
/// derives, so it is the operator's alone to choose: write `nuc1` into the
/// file to name the machine. Changing it changes this installation's dock
/// identity, so existing approvals must be re-granted.
///
/// Concurrent first starts agree on one name: each candidate is written in
/// full to its own file and published by hard link, which fails rather than
/// replace a name already published, and a loser reads the winner's.
///
/// # Errors
/// An unreadable file, a state dir the name cannot be published in, or an
/// empty file — it is refused rather than regenerated, because two starts
/// both "recovering" it could each pick a different identity.
pub fn dock_instance(state_dir: &Path) -> std::io::Result<String> {
    let path = state_dir.join(DOCK_INSTANCE_FILE);
    let read = || -> std::io::Result<String> {
        let name = std::fs::read_to_string(&path)?.trim().to_string();
        if name.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} is empty: write a name into it or delete it",
                    path.display()
                ),
            ));
        }
        Ok(name)
    };
    match read() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        found => return found,
    }
    let name = newt_core::new_conversation_id();
    std::fs::create_dir_all(state_dir)?;
    let candidate = state_dir.join(format!(".{DOCK_INSTANCE_FILE}.{name}"));
    std::fs::write(&candidate, format!("{name}\n"))?;
    let published = std::fs::hard_link(&candidate, &path);
    let _ = std::fs::remove_file(&candidate);
    match published {
        Ok(()) => Ok(name),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read(),
        Err(e) => Err(e),
    }
}

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

/// Whether the host that sent `host` is promoted on this hub: approved in the
/// hub's own signed registry, by the same check a responder applies to its
/// caller. A host that is not is staged instead, for the operator to promote
/// with `newt dock approve --staged` (K8.5).
fn host_promoted(state_dir: &Path, host: &crate::uplink::Hello) -> bool {
    let host_fp = Fingerprint::of_bytes(&host.pubkey).hex();
    if authorize_caller(state_dir, &host_fp).is_ok() {
        return true;
    }
    if let Err(e) = newt_core::dock_registry::stage_host(
        &state_dir.join("config.toml"),
        &host.pubkey,
        &host.instance,
        std::time::SystemTime::now(),
    ) {
        tracing::warn!(error = %e, host = %host_fp, "dock uplink: could not stage host");
    }
    false
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

/// Answer one dock request from `caller_agent_fp` (a verified agent
/// fingerprint): authorize the caller FIRST — refuse before any disclosure or
/// side effect — then handle it. Shared by the direct responder
/// ([`NewtDockService`]) and the uplink ([`crate::DockUplink`]), so both carry
/// exactly the same authorization. SQLite is synchronous, so it runs on a
/// blocking thread.
pub(crate) async fn serve_dock(
    state_dir: PathBuf,
    caller_agent_fp: String,
    request: Result<DockRequest, String>,
) -> DockReply {
    tokio::task::spawn_blocking(move || {
        let authz = match authorize_caller(&state_dir, &caller_agent_fp) {
            Ok(authz) => authz,
            Err(deny) => return deny,
        };
        match request {
            Ok(req) => handle_dock(&state_dir, authz, req),
            Err(e) => DockReply::Error(format!("bad dock request: {e}")),
        }
    })
    .await
    .unwrap_or_else(|e| DockReply::Error(format!("dock handler panicked: {e}")))
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
                let request = serde_json::from_slice(&body).map_err(|e| e.to_string());
                let reply = serve_dock(state_dir, caller_agent_fp, request).await;
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

/// Where a hub reaches a docked peer.
#[derive(Debug, Clone, Copy)]
pub enum DockPeer {
    /// Dial the peer's [`NewtDockService`] directly (K7, LAN).
    Direct(PeerEndpoint),
    /// Hand the request to the host with this agent fingerprint over the
    /// uplink it holds open to this hub (K8). The hub never dials it.
    Uplink(Fingerprint),
}

impl From<PeerEndpoint> for DockPeer {
    fn from(peer: PeerEndpoint) -> Self {
        Self::Direct(peer)
    }
}

/// The dock **dialer**: a hub-side bus that requests docks from peers.
pub struct DockClient {
    bus: Bus,
    user_fp: Fingerprint,
    uplinks: std::sync::Arc<crate::uplink::UplinkHub>,
}

impl DockClient {
    /// Bind a hub-side dial bus (0 = ephemeral port).
    ///
    /// # Errors
    /// Propagates a bus bind failure.
    pub async fn bind(user: &UserKey, agent: AgentKey, port: u16) -> anyhow::Result<Self> {
        let user_fp = user.fingerprint();
        let bus = Bus::bind(user, agent, port).await?;
        let uplinks = std::sync::Arc::default();
        Ok(Self {
            bus,
            user_fp,
            uplinks,
        })
    }

    /// Accept uplinks from docked hosts (K8): hold each host's poll and hand
    /// it the requests addressed to [`DockPeer::Uplink`]. Opt-in, so a hub
    /// only serves uplinks when it means to.
    ///
    /// Only a **promoted** host is served: one approved in this hub's own
    /// signed dock registry in `state_dir` (K8.5). Any other host is staged
    /// there for `newt dock approve --staged` on the hub's terminal and gets
    /// no request; the registry is re-read on every poll, so a revoked host
    /// loses its uplink at its next poll. A served host still authorizes every
    /// request against its own registry (K8.2).
    pub fn serve_uplinks(&self, state_dir: PathBuf) {
        self.serve_uplinks_with(move |host| host_promoted(&state_dir, host));
    }

    fn serve_uplinks_with(
        &self,
        promoted: impl Fn(&crate::uplink::Hello) -> bool + Send + Sync + 'static,
    ) {
        crate::uplink::serve(&self.bus, self.user_fp, self.uplinks.clone(), promoted);
    }

    /// Serve every uplink without consulting a registry, for tests of the
    /// carrier itself.
    #[cfg(test)]
    pub(crate) fn serve_all_uplinks(&self) {
        self.serve_uplinks_with(|_| true);
    }

    /// The hub state behind [`Self::serve_uplinks`].
    #[cfg(test)]
    pub(crate) fn uplinks(&self) -> &crate::uplink::UplinkHub {
        &self.uplinks
    }

    /// The bound UDP port hosts uplink to.
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.bus.local_port()
    }

    async fn request(
        &self,
        peer: impl Into<DockPeer>,
        req: &DockRequest,
    ) -> anyhow::Result<DockReply> {
        match peer.into() {
            DockPeer::Direct(peer) => {
                let topic = Topic::new(self.user_fp, DOCK_TOPIC);
                let body = serde_json::to_vec(req)?;
                let reply = self
                    .bus
                    .request_direct(peer, &topic, body, DOCK_TIMEOUT)
                    .await?;
                Ok(serde_json::from_slice(&reply)?)
            }
            DockPeer::Uplink(host) => self.uplinks.request(host, req.clone()).await,
        }
    }

    /// List a peer's sessions.
    ///
    /// # Errors
    /// Bus/transport failure, or a non-`Sessions` reply.
    pub async fn list_sessions(
        &self,
        peer: impl Into<DockPeer>,
    ) -> anyhow::Result<Vec<DockSessionInfo>> {
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
        peer: impl Into<DockPeer>,
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
    pub async fn inject(
        &self,
        peer: impl Into<DockPeer>,
        conv: &str,
        text: &str,
    ) -> anyhow::Result<()> {
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

    /// K8-b: a dock approval survives a restart. The approval pins the hub's
    /// fingerprint; a restarted hub re-derives the same key, so the responder
    /// still authorizes it. Negative control: a per-process random key (what
    /// `mint_agent` issued before K8-b) is refused under the same registry.
    #[test]
    fn a_dock_approval_survives_a_restart_of_the_derived_hub() {
        let user = UserKey::generate();
        let dir = tempfile::tempdir().unwrap();
        let hub = dock_agent(&user, DockRole::Hub, "home-hub");
        approve_caller(&user, dir.path(), &hub.public_bytes(), DockScope::Mirror);
        drop(hub); // the hub process exits

        let restarted = dock_agent(&user, DockRole::Hub, "home-hub");
        let fp = restarted.fingerprint().hex();
        assert_eq!(
            authorize_caller(dir.path(), &fp).ok(),
            Some(Some(DockScope::Mirror)),
            "the re-derived hub is still the approved hub"
        );

        let random = agent(&user, "hub", Vec::new());
        assert!(
            authorize_caller(dir.path(), &random.fingerprint().hex()).is_err(),
            "a per-process key is a stranger to the registry"
        );
    }

    /// Two installations of one operator derive distinct identities, and each
    /// end of one installation is distinct from the other.
    #[test]
    fn dock_identities_are_distinct_per_instance_and_role() {
        let user = UserKey::generate();
        let fp = |role, instance| dock_agent(&user, role, instance).fingerprint();
        assert_eq!(fp(DockRole::Host, "nuc1"), fp(DockRole::Host, "nuc1"));
        assert_ne!(fp(DockRole::Host, "nuc1"), fp(DockRole::Host, "nuc2"));
        assert_ne!(fp(DockRole::Hub, "nuc1"), fp(DockRole::Host, "nuc1"));
        let host = dock_agent(&user, DockRole::Host, "nuc1");
        assert_eq!(host.cert().metadata.capabilities, vec![DOCK_CAPABILITY_TAG]);
        host.cert()
            .verify()
            .expect("a derived dock key is a normal user-rooted cert");
    }

    /// The instance name is created once, then read back; an operator-written
    /// name wins; an empty file is treated as absent.
    #[test]
    fn dock_instance_is_created_once_and_operator_overridable() {
        let dir = tempfile::tempdir().unwrap();
        let first = dock_instance(dir.path()).unwrap();
        assert!(!first.is_empty());
        assert_eq!(
            dock_instance(dir.path()).unwrap(),
            first,
            "stable once created"
        );

        std::fs::write(dir.path().join(DOCK_INSTANCE_FILE), "nuc1\n").unwrap();
        assert_eq!(dock_instance(dir.path()).unwrap(), "nuc1");

        std::fs::write(dir.path().join(DOCK_INSTANCE_FILE), "  \n").unwrap();
        let empty = dock_instance(dir.path()).unwrap_err();
        assert_eq!(
            empty.kind(),
            std::io::ErrorKind::InvalidData,
            "refused, not regenerated"
        );
    }

    /// Concurrent first starts must agree: every caller returns the one name
    /// that ended up on disk, and no candidate file is left behind.
    #[test]
    fn concurrent_first_starts_agree_on_one_dock_instance() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let names: Vec<String> = (0..16)
            .map(|_| {
                let (dir, barrier) = (dir.path().to_path_buf(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    dock_instance(&dir).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        let persisted = std::fs::read_to_string(dir.path().join(DOCK_INSTANCE_FILE)).unwrap();
        assert!(
            names.iter().all(|n| n == persisted.trim()),
            "{names:?} vs {persisted:?}"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "candidates cleaned up"
        );
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

    /// The dock-grant audience theorem's TRANSPORT half (see
    /// `docs/decisions/newt_web_docking.md`, "Dock grant audience"): NO dock
    /// request writes the approved registry. A remote caller cannot cause a grant
    /// to appear in — or change in — a responder's `docks.d`; the only writers are
    /// the local, root-key-gated `newt dock approve/revoke` CLI. This is why
    /// "possession in the protected registry is the audience" holds: there is no
    /// mesh-reachable path that mutates the registry, so a grant can only be placed
    /// by an operator with local filesystem authority (who also holds the
    /// co-located root key). Runs the full authorize + handle path for
    /// list/transcript/inject — including a real inject side effect — and asserts
    /// `docks.d/peers.toml` is byte-identical before and after.
    #[test]
    fn no_dock_request_mutates_the_approved_registry() {
        let user = UserKey::generate();
        let dir = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(dir.path(), dir.path(), 100).unwrap();
        let conv = store.create("s", None).unwrap();
        store.append_turn(&conv, "q", "a").unwrap();

        let caller_pubkey = [0x33u8; 32];
        approve_caller(&user, dir.path(), &caller_pubkey, DockScope::MirrorInject);
        let fp = newt_core::dock_registry::agent_fingerprint_of_pubkey(&caller_pubkey);
        let registry_path = dir.path().join("ocap/docks.d/peers.toml");
        let before = std::fs::read(&registry_path).expect("registry seeded");

        for req in [
            DockRequest::ListSessions,
            DockRequest::Transcript { conv: conv.clone() },
            DockRequest::Inject {
                conv: conv.clone(),
                text: "AUDIENCE_PROBE".into(),
            },
        ] {
            let authz = authorize_caller(dir.path(), &fp).expect("approved caller authorizes");
            let _ = handle_dock(dir.path(), authz, req);
        }

        // The inject really landed in the peer's own store inbox (D2) — so the
        // side effect ran for real, and STILL the registry is untouched.
        assert_eq!(
            store.take_injected_prompt(&conv).unwrap().map(|p| p.body),
            Some("AUDIENCE_PROBE".to_string()),
            "the inject must have actually executed against the store"
        );
        let after = std::fs::read(&registry_path).unwrap();
        assert_eq!(
            before, after,
            "no dock request may write the approved registry (the audience is the local registry)"
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

    /// K8-d acceptance (K8.5, hub side): a host the hub has not approved is
    /// only staged. Its poll gets no request, it reports `Staged`, the hub's
    /// staging dir lists its key and instance name, and the hub cannot reach
    /// it. Once the operator promotes it — a signed approval in the hub's own
    /// registry — its next poll is served. A poll naming another host's key is
    /// refused and stages nothing. Real loopback QUIC.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live transport — nightly/full mesh-integration tier only"]
    async fn hub_stages_an_unapproved_host_and_serves_it_once_promoted() {
        use crate::uplink::UplinkState;
        let user = UserKey::generate();
        let hub_dir = tempfile::tempdir().unwrap();
        let host_dir = tempfile::tempdir().unwrap();
        let store =
            newt_core::ConversationStore::new(host_dir.path(), host_dir.path(), 100).unwrap();
        store.create("nuc1 session", None).unwrap();
        user.save(&hub_dir.path().join("identity.pem")).unwrap();
        let hub_config = hub_dir.path().join("config.toml");

        let hub_agent = dock_agent(&user, DockRole::Hub, "home-hub");
        let hub_pubkey = hub_agent.public_bytes();
        let hub = DockClient::bind(&user, hub_agent, 0).await.unwrap();
        hub.serve_uplinks(hub_dir.path().to_path_buf());
        approve_caller(&user, host_dir.path(), &hub_pubkey, DockScope::Mirror);

        let host_agent = dock_agent(&user, DockRole::Host, "nuc1");
        let (host_fp, host_pubkey) = (host_agent.fingerprint(), host_agent.public_bytes());
        let hub_ep = loopback(hub_pubkey, hub.local_port());
        let mut host =
            crate::DockUplink::start(&user, "nuc1", host_dir.path().to_path_buf(), hub_ep)
                .await
                .unwrap();
        tokio::time::timeout(Duration::from_secs(10), host.reached(UplinkState::Staged))
            .await
            .expect("an unapproved host is told it is staged");

        let staged =
            newt_core::dock_registry::staged_hosts(&hub_config, std::time::SystemTime::now());
        assert_eq!(staged.len(), 1, "{staged:?}");
        assert_eq!(staged[0].peer_label, "nuc1");
        assert_eq!(staged[0].peer_agent_fingerprint, host_fp.hex());
        let unreached = hub
            .list_sessions(DockPeer::Uplink(host_fp))
            .await
            .unwrap_err()
            .to_string();
        assert!(unreached.contains("has no open uplink"), "{unreached}");

        // A poll signed by one agent but naming another's key: refused, and
        // the named key is not staged.
        let spoofer = Bus::bind_outbound_only(&user, dock_agent(&user, DockRole::Host, "spoofer"))
            .await
            .unwrap();
        let spoofed = crate::uplink::Hello {
            pubkey: [9; 32],
            instance: "victim".into(),
        };
        let poll = serde_json::json!({ "host": spoofed, "answer": null });
        let topic = Topic::new(user.fingerprint(), crate::uplink::DOCK_UPLINK_TOPIC);
        let reply = spoofer
            .request_direct(
                hub_ep,
                &topic,
                poll.to_string().into_bytes(),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        let work: serde_json::Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(
            work,
            serde_json::json!({ "job": null, "staged": false }),
            "a poll naming another key gets no work"
        );
        spoofer.close().await.unwrap();
        let labels: Vec<String> =
            newt_core::dock_registry::staged_hosts(&hub_config, std::time::SystemTime::now())
                .into_iter()
                .map(|h| h.peer_label)
                .collect();
        assert_eq!(labels, ["nuc1"], "nothing staged under the spoofed key");

        // Promote: what `newt dock approve --staged` signs.
        approve_caller(&user, hub_dir.path(), &host_pubkey, DockScope::Mirror);
        tokio::time::timeout(
            Duration::from_secs(15),
            hub.uplinks().wait_for_uplink(host_fp),
        )
        .await
        .expect("a promoted host is served at its next poll");
        let sessions = hub.list_sessions(DockPeer::Uplink(host_fp)).await.unwrap();
        assert!(
            sessions.iter().any(|s| s.title == "nuc1 session"),
            "{sessions:?}"
        );

        host.close().await;
        hub.close().await.unwrap();
    }

    /// K8-c acceptance: a docked host serves list, transcript and inject to its
    /// hub over an uplink it dialed, while accepting no inbound connection. The
    /// host authorizes the hub against its own registry on every request
    /// (K8.2), through the uplink exactly as through a direct dial: an
    /// unapproved hub, a Mirror-only inject, a revoked hub and the kill switch
    /// are each refused by the host with its own reason. Real loopback QUIC.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live transport — nightly/full mesh-integration tier only"]
    async fn docked_host_serves_its_hub_over_an_uplink_and_accepts_no_inbound() {
        let user = UserKey::generate();
        let dir = tempfile::tempdir().unwrap();
        let store = newt_core::ConversationStore::new(dir.path(), dir.path(), 100).unwrap();
        let conv = store.create("laptop session", None).unwrap();
        store
            .append_turn(&conv, "q1", "UPLINK_REPLY from the laptop")
            .unwrap();

        let hub_agent = dock_agent(&user, DockRole::Hub, "home-hub");
        let hub_pubkey = hub_agent.public_bytes();
        let hub = DockClient::bind(&user, hub_agent, 0).await.unwrap();
        hub.serve_uplinks(dir.path().to_path_buf());
        let host_agent = dock_agent(&user, DockRole::Host, "laptop");
        let (host_fp, host_pubkey) = (host_agent.fingerprint(), host_agent.public_bytes());
        // The hub serves only a host it has promoted (K8-d); this test shares
        // one state dir between hub and host.
        approve_caller(&user, dir.path(), &host_pubkey, DockScope::Mirror);
        let host = crate::DockUplink::start(
            &user,
            "laptop",
            dir.path().to_path_buf(),
            loopback(hub_pubkey, hub.local_port()),
        )
        .await
        .unwrap();
        let laptop = DockPeer::Uplink(host_fp);
        tokio::time::timeout(
            Duration::from_secs(5),
            hub.uplinks().wait_for_uplink(host_fp),
        )
        .await
        .expect("the host uplinks within 5s");

        let refusal = |r: anyhow::Result<Vec<DockSessionInfo>>| r.unwrap_err().to_string();
        let unapproved = refusal(hub.list_sessions(laptop).await);
        assert!(
            unapproved.contains("is not an approved dock"),
            "the host itself refuses an unapproved hub: {unapproved}"
        );

        approve_caller(&user, dir.path(), &hub_pubkey, DockScope::Mirror);
        let sessions = hub.list_sessions(laptop).await.unwrap();
        assert!(
            sessions.iter().any(|s| s.title == "laptop session"),
            "{sessions:?}"
        );
        let t = hub.transcript(laptop, &conv).await.unwrap();
        assert!(t
            .turns
            .iter()
            .any(|turn| turn.assistant.contains("UPLINK_REPLY")));
        let mirror_only = hub
            .inject(laptop, &conv, "MIRROR_ONLY_INJECT")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            mirror_only.contains("does not permit this operation"),
            "a Mirror dock may not inject: {mirror_only}"
        );
        assert!(store.take_injected_prompt(&conv).unwrap().is_none());

        approve_caller(&user, dir.path(), &hub_pubkey, DockScope::MirrorInject);
        hub.inject(laptop, &conv, "UPLINK_INJECT run the lints")
            .await
            .unwrap();
        assert_eq!(
            store.take_injected_prompt(&conv).unwrap().map(|p| p.body),
            Some("UPLINK_INJECT run the lints".to_string()),
            "the inject lands in the host's own inbox (the host stays sole writer)"
        );

        std::fs::write(dir.path().join("dock-exposure-disabled"), b"").unwrap();
        let killed = refusal(hub.list_sessions(laptop).await);
        assert!(killed.contains("dock exposure disabled"), "{killed}");
        std::fs::remove_file(dir.path().join("dock-exposure-disabled")).unwrap();

        let hub_fp = newt_core::dock_registry::agent_fingerprint_of_pubkey(&hub_pubkey);
        newt_core::dock_registry::revoke_dock_with_identity(
            &dir.path().join("config.toml"),
            &dir.path().join("identity.pem"),
            &hub_fp,
        )
        .unwrap();
        let revoked = refusal(hub.list_sessions(laptop).await);
        assert!(
            revoked.contains("is not an approved dock"),
            "a revoked hub is refused on its next request: {revoked}"
        );

        // The host accepts nothing: a direct dial to its port is refused by the
        // transport, not merely unanswered.
        let direct = hub
            .list_sessions(loopback(host_pubkey, host.local_port()))
            .await
            .unwrap_err();
        assert!(
            matches!(
                direct.downcast_ref::<agent_mesh_bus::BusError>(),
                Some(agent_mesh_bus::BusError::Transport(_))
            ),
            "a dial to the docked host must be refused, got {direct:?}"
        );

        host.close().await;
        hub.close().await.unwrap();
    }
}

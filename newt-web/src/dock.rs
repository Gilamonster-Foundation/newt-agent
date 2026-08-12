//! dock — surface OTHER newt-agents' sessions in this cockpit (requirement 2).
//!
//! Two transports behind one seam:
//!   * **HTTP** (the MVP): a peer newt-web exposes `GET /api/sessions[…]`; a hub
//!     pulls it. Loopback/LAN, no identity.
//!   * **agent-mesh** (Phase 2): a peer runs a [`newt_mesh::NewtDockService`]
//!     responder; a hub dials it with a [`newt_mesh::DockClient`] over the bus
//!     (same-operator trust = the bus handshake). This is the real cross-machine
//!     transport.
//!
//! Peers are configured with `NEWT_WEB_DOCK_PEERS`, comma-separated:
//!   * `label=http://host:port`  (or a bare URL) — an HTTP peer
//!   * `label=mesh:<agent_pubkey_hex>@<ip>:<port>` — a mesh peer (direct-dial)
//!
//! Mirror-only for the transcript (D2 — the hub never writes a remote
//! transcript); Inject ENQUEUES to the remote, which runs it and stays the sole
//! writer. `docs/decisions/newt_web_docking` K7.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

/// One remote session (the wire twin of `newt_mesh::DockSessionInfo` and of a
/// peer's HTTP `/api/sessions` row).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DockedSession {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub turns: usize,
    #[serde(default)]
    pub live: bool,
}

/// A mirrored transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DockedTranscript {
    pub title: String,
    pub turns: Vec<DockedTurn>,
}

/// One `(user, assistant)` turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DockedTurn {
    pub user: String,
    pub assistant: String,
}

/// How to reach a peer.
#[derive(Debug, Clone)]
pub(crate) enum DockKind {
    /// The MVP HTTP transport: the peer's cockpit base URL.
    Http { base_url: String },
    /// The agent-mesh transport: the peer agent's pubkey + a direct-dial addr.
    Mesh { pubkey: [u8; 32], addr: SocketAddr },
}

/// A configured dock peer: an operator label + how to reach it.
#[derive(Debug, Clone)]
pub(crate) struct DockPeer {
    pub label: String,
    pub kind: DockKind,
}

// ── the shared mesh dial client (bound once at startup) ─────────────────────
static DOCK_CLIENT: OnceLock<Arc<newt_mesh::DockClient>> = OnceLock::new();

/// Install the process-wide mesh dial client (called once at startup when the
/// operator identity is available). A second call is ignored.
pub(crate) fn set_dock_client(client: Arc<newt_mesh::DockClient>) {
    let _ = DOCK_CLIENT.set(client);
}
fn dock_client() -> Option<Arc<newt_mesh::DockClient>> {
    DOCK_CLIENT.get().cloned()
}

// ── the approved-dock gate (requirement 5) ──────────────────────────────────
// The hub refuses to dial a mesh peer the operator never approved. The check
// is HUB-SIDE by necessity: the bus responder cannot see which same-operator
// agent is calling, but the hub already knows each peer's agent pubkey from its
// dock config, so it resolves `BLAKE3(pubkey)` against the signed dock registry
// before every dial. All three mesh operations funnel through
// `approved_endpoint`, so an ungated dial cannot be written.

/// The operator identity paths the dock registry resolves against — set once at
/// startup: `(config_path, identity_pem)`. The registry lives at
/// `config_path`'s sibling `ocap/docks.d/`.
static DOCK_IDENTITY: OnceLock<(std::path::PathBuf, std::path::PathBuf)> = OnceLock::new();

/// Record where the operator config + identity live, so the gate can load the
/// signed dock registry. Called once at startup; a second call is ignored.
pub(crate) fn set_dock_identity(config_path: std::path::PathBuf, identity_pem: std::path::PathBuf) {
    let _ = DOCK_IDENTITY.set((config_path, identity_pem));
}

/// Whether the hub enforces the approved-dock registry before a mesh dial.
/// **Fail-closed by default** (requirement 5): a security ceremony that defaults
/// OFF is theater, so a mesh peer is refused unless the operator has approved it
/// — the ceremony is the boundary, not an opt-in. The ONLY way off is an
/// explicit, greppable, unsafe opt-out.
fn require_dock_approval() -> bool {
    !dock_approval_disabled()
}

/// The one unsafe escape hatch: `NEWT_INSECURE_DOCK_NO_APPROVAL=1` turns the
/// approved-dock gate off (loopback dev / raw-transport testing only). Named so
/// it surfaces in any `grep INSECURE` audit; disabling the ceremony is a
/// deliberate act, never the absence of a flag.
fn dock_approval_disabled() -> bool {
    std::env::var("NEWT_INSECURE_DOCK_NO_APPROVAL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// The approval decision, pure over its inputs so it is testable without the
/// process-global identity/env. `enforce` off ⇒ always Ok (dev). On ⇒ the
/// registry at `identity` must hold a live approval for `BLAKE3(pubkey)`;
/// missing identity, unapproved, or revoked all refuse.
fn check_dock_approval(
    enforce: bool,
    identity: Option<&(std::path::PathBuf, std::path::PathBuf)>,
    pubkey: &[u8; 32],
) -> Result<(), String> {
    if !enforce {
        return Ok(());
    }
    let (config, identity) =
        identity.ok_or("dock approval required but no operator identity is configured")?;
    let fp = newt_core::dock_registry::agent_fingerprint_of_pubkey(pubkey);
    let (registry, _warnings) =
        newt_core::dock_registry::load_docks_with_identity(config, identity);
    if registry.approved(&fp).is_none() {
        return Err(format!(
            "peer {}… is not approved — run `newt dock approve`",
            &fp[..fp.len().min(12)]
        ));
    }
    Ok(())
}

/// Gate a mesh dial on the approved-dock registry, then build the endpoint.
/// The ONLY place a mesh `PeerEndpoint` is constructed, so no dial skips the
/// gate.
fn approved_endpoint(
    pubkey: &[u8; 32],
    addr: &SocketAddr,
) -> Result<newt_mesh::PeerEndpoint, String> {
    check_dock_approval(require_dock_approval(), DOCK_IDENTITY.get(), pubkey)?;
    Ok(newt_mesh::PeerEndpoint::new(*pubkey, *addr))
}

/// Percent-encode a value for a query string (peer label / conversation id).
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Find a configured peer by its label (fail-closed on an unknown one).
pub(crate) fn peer_by_label(label: &str) -> Option<DockPeer> {
    configured_peers().into_iter().find(|p| p.label == label)
}

/// The configured dock peers, from `NEWT_WEB_DOCK_PEERS`.
pub(crate) fn configured_peers() -> Vec<DockPeer> {
    std::env::var("NEWT_WEB_DOCK_PEERS")
        .map(|raw| parse_peers(&raw))
        .unwrap_or_default()
}

/// Parse a `NEWT_WEB_DOCK_PEERS` value into peers — pure (tests need no env). An
/// unparseable mesh entry is dropped (fail-closed), never docked as HTTP.
fn parse_peers(raw: &str) -> Vec<DockPeer> {
    raw.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (label, target) = match entry.split_once('=') {
                Some((l, u)) => (l.trim().to_string(), u.trim().to_string()),
                None => {
                    let host = entry
                        .trim_start_matches("http://")
                        .trim_start_matches("https://")
                        .to_string();
                    (host, entry.to_string())
                }
            };
            let kind = if let Some(rest) = target.strip_prefix("mesh:") {
                parse_mesh(rest)?
            } else if target.is_empty() {
                return None;
            } else {
                DockKind::Http {
                    base_url: target.trim_end_matches('/').to_string(),
                }
            };
            Some(DockPeer { label, kind })
        })
        .collect()
}

/// Parse `<agent_pubkey_hex>@<ip>:<port>` into a mesh dial route.
fn parse_mesh(spec: &str) -> Option<DockKind> {
    let (pubkey_hex, addr_str) = spec.split_once('@')?;
    if pubkey_hex.len() != 64 {
        return None;
    }
    let mut pubkey = [0u8; 32];
    for (i, byte) in pubkey.iter_mut().enumerate() {
        *byte = u8::from_str_radix(pubkey_hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    let addr: SocketAddr = addr_str.parse().ok()?;
    Some(DockKind::Mesh { pubkey, addr })
}

// ── transport-agnostic peer operations (dispatch on kind) ───────────────────

fn mesh_to_docked(s: newt_mesh::DockSessionInfo) -> DockedSession {
    DockedSession {
        id: s.id,
        title: s.title,
        workspace: s.workspace,
        turns: s.turns,
        live: s.live,
    }
}

/// A peer's sessions, over whichever transport the peer uses.
async fn peer_sessions(peer: &DockPeer) -> Result<Vec<DockedSession>, String> {
    match &peer.kind {
        DockKind::Http { base_url } => {
            let url = format!("{base_url}/api/sessions");
            tokio::task::spawn_blocking(move || {
                ureq::get(&url)
                    .timeout(std::time::Duration::from_secs(3))
                    .call()
                    .map_err(|e| format!("unreachable: {e}"))?
                    .into_json::<Vec<DockedSession>>()
                    .map_err(|e| format!("bad /api/sessions payload: {e}"))
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        }
        DockKind::Mesh { pubkey, addr } => {
            let client = dock_client().ok_or("mesh dock unavailable (no operator identity)")?;
            let ep = approved_endpoint(pubkey, addr)?;
            client
                .list_sessions(ep)
                .await
                .map(|v| v.into_iter().map(mesh_to_docked).collect())
                .map_err(|e| format!("mesh: {e}"))
        }
    }
}

/// A peer session's transcript, over whichever transport.
async fn peer_transcript(peer: &DockPeer, conv: &str) -> Result<DockedTranscript, String> {
    match &peer.kind {
        DockKind::Http { base_url } => {
            let url = format!("{base_url}/api/sessions/{}/transcript", pct(conv));
            tokio::task::spawn_blocking(move || {
                ureq::get(&url)
                    .timeout(std::time::Duration::from_secs(3))
                    .call()
                    .map_err(|e| format!("unreachable: {e}"))?
                    .into_json::<DockedTranscript>()
                    .map_err(|e| format!("bad transcript payload: {e}"))
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        }
        DockKind::Mesh { pubkey, addr } => {
            let client = dock_client().ok_or("mesh dock unavailable")?;
            let ep = approved_endpoint(pubkey, addr)?;
            client
                .transcript(ep, conv)
                .await
                .map(|t| DockedTranscript {
                    title: t.title,
                    turns: t
                        .turns
                        .into_iter()
                        .map(|x| DockedTurn {
                            user: x.user,
                            assistant: x.assistant,
                        })
                        .collect(),
                })
                .map_err(|e| format!("mesh: {e}"))
        }
    }
}

/// Enqueue a prompt into a peer session (D2 — the peer runs it), over whichever
/// transport.
pub(crate) async fn peer_inject(peer: &DockPeer, conv: &str, text: &str) -> Result<(), String> {
    match &peer.kind {
        DockKind::Http { base_url } => {
            let url = format!("{base_url}/api/sessions/{}/inject", pct(conv));
            let text = text.to_string();
            tokio::task::spawn_blocking(move || {
                ureq::post(&url)
                    .timeout(std::time::Duration::from_secs(3))
                    .send_form(&[("text", &text)])
                    .map(|_| ())
                    .map_err(|e| format!("unreachable: {e}"))
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        }
        DockKind::Mesh { pubkey, addr } => {
            let client = dock_client().ok_or("mesh dock unavailable")?;
            let ep = approved_endpoint(pubkey, addr)?;
            client
                .inject(ep, conv, text)
                .await
                .map_err(|e| format!("mesh: {e}"))
        }
    }
}

/// Mirror a docked session's transcript (the "select" path) — used by the hub's
/// `/dock/panel` route.
pub(crate) async fn fetch_transcript(
    peer: &DockPeer,
    conv: &str,
) -> Result<DockedTranscript, String> {
    peer_transcript(peer, conv).await
}

/// Render a docked remote session as a panel: the transcript **mirrored**
/// read-only, plus an **inject** form (D2 — enqueue to the remote; the remote
/// runs it and stays sole writer).
pub(crate) fn dock_panel(peer_label: &str, conv_id: &str, transcript: &DockedTranscript) -> String {
    let snap = crate::agents::Snapshot {
        messages: transcript
            .turns
            .iter()
            .flat_map(|t| {
                [
                    ("user".to_string(), t.user.clone()),
                    ("assistant".to_string(), t.assistant.clone()),
                ]
            })
            .collect(),
        busy: false,
        closed: false,
    };
    format!(
        r##"<section class="agent dock-remote">
<h2><span>{title} <small>· {label} · remote (mirror + inject, D2)</small></span></h2>
<div class="transcript">{fragment}</div>
<form class="prompt" hx-post="/dock/inject?peer={plabel}&conv={pconv}" hx-target="#panel" hx-swap="innerHTML">
<input name="text" placeholder="prompt the remote session…" autocomplete="off" required>
<button>send</button></form>
<p class="hint">Injected over the dock — the remote host runs it and stays the sole writer (D2).</p>
</section>"##,
        title = crate::shell::escape(&transcript.title),
        label = crate::shell::escape(peer_label),
        fragment = crate::shell::transcript_fragment(&snap),
        plabel = pct(peer_label),
        pconv = pct(conv_id),
    )
}

/// Render the "docked peers" cockpit section: each configured peer with its
/// remote sessions (read-only + selectable). An unreachable peer renders a
/// notice, not a gap. Fetched over each peer's own transport.
pub(crate) async fn docked_section() -> String {
    let peers = configured_peers();
    if peers.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        r#"<section class="docked"><h2>docked peers</h2><p class="hint">Remote newt-agents' sessions (D2 mirror + inject). Transport: HTTP or agent-mesh.</p>"#,
    );
    for peer in &peers {
        let transport = match peer.kind {
            DockKind::Http { .. } => "http",
            DockKind::Mesh { .. } => "mesh",
        };
        match peer_sessions(peer).await {
            Ok(sessions) if sessions.is_empty() => {
                out.push_str(&format!(
                    r#"<div class="peer"><h3>● {label} <small>· {transport}</small></h3><p class="empty">no sessions</p></div>"#,
                    label = crate::shell::escape(&peer.label),
                ));
            }
            Ok(sessions) => {
                out.push_str(&format!(
                    r#"<div class="peer"><h3>● {label} <small>· {transport} · remote</small></h3><ul>"#,
                    label = crate::shell::escape(&peer.label),
                ));
                for s in sessions.iter().take(30) {
                    let dot = if s.live { "▶" } else { "○" };
                    out.push_str(&format!(
                        r##"<li><button class="dock-open" hx-get="/dock/panel?peer={plabel}&conv={pconv}" hx-target="#panel" hx-swap="innerHTML">{dot} {title}</button> <small>({n} turns · {label})</small></li>"##,
                        plabel = pct(&peer.label),
                        pconv = pct(&s.id),
                        dot = dot,
                        title = crate::shell::escape(&s.title),
                        n = s.turns,
                        label = crate::shell::escape(&peer.label),
                    ));
                }
                out.push_str("</ul></div>");
            }
            Err(err) => {
                out.push_str(&format!(
                    r#"<div class="peer"><h3>○ {label} <small>· {transport} · {err}</small></h3></div>"#,
                    label = crate::shell::escape(&peer.label),
                    err = crate::shell::escape(&err),
                ));
            }
        }
    }
    out.push_str("</section>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_and_mesh_and_bare() {
        let peers = parse_peers(&format!(
            " lab-b=http://127.0.0.1:8898/ , nuc=mesh:{}@10.0.0.4:9000 , http://10.0.0.9:8880 ,, ",
            "aa".repeat(32)
        ));
        assert_eq!(peers.len(), 3);
        assert_eq!(peers[0].label, "lab-b");
        assert!(
            matches!(&peers[0].kind, DockKind::Http { base_url } if base_url == "http://127.0.0.1:8898")
        );
        assert_eq!(peers[1].label, "nuc");
        assert!(matches!(&peers[1].kind, DockKind::Mesh { pubkey, addr }
            if pubkey == &[0xaau8; 32] && addr.to_string() == "10.0.0.4:9000"));
        assert!(matches!(&peers[2].kind, DockKind::Http { .. }));
    }

    #[test]
    fn a_malformed_mesh_entry_is_dropped_not_docked_as_http() {
        assert!(parse_peers("bad=mesh:short@1.2.3.4:5").is_empty());
        assert!(parse_peers("bad=mesh:nohost").is_empty());
    }

    #[test]
    fn empty_peers_value_is_no_docks() {
        assert!(parse_peers("").is_empty());
        assert!(parse_peers("   ").is_empty());
    }

    #[serial_test::serial(newt_web_env)]
    #[test]
    fn dock_approval_is_fail_closed_by_default() {
        std::env::remove_var("NEWT_INSECURE_DOCK_NO_APPROVAL");
        assert!(
            require_dock_approval(),
            "the approved-dock gate must be ON by default (fail-closed)"
        );
        std::env::set_var("NEWT_INSECURE_DOCK_NO_APPROVAL", "1");
        assert!(
            !require_dock_approval(),
            "only the explicit unsafe opt-out disables enforcement"
        );
        std::env::remove_var("NEWT_INSECURE_DOCK_NO_APPROVAL");
    }

    #[test]
    fn the_dock_gate_admits_only_an_approved_peer() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let identity = dir.path().join("identity.pem");
        // Write a real operator key via the path-side type — the PEM is a
        // standard PKCS#8 ed25519 key the registry side re-loads verbatim.
        agent_mesh_core::UserKey::generate()
            .save(&identity)
            .unwrap();
        let ident = (config.clone(), identity.clone());
        let pubkey = [9u8; 32];

        // Enforcement off ⇒ dev/loopback dials freely.
        assert!(check_dock_approval(false, None, &pubkey).is_ok());
        // On but no identity configured ⇒ fail closed.
        assert!(check_dock_approval(true, None, &pubkey).is_err());
        // On, identity present, peer not yet approved ⇒ refused.
        assert!(check_dock_approval(true, Some(&ident), &pubkey).is_err());

        // The operator approves this exact pubkey.
        let fp = newt_core::dock_registry::agent_fingerprint_of_pubkey(&pubkey);
        let pubkey_hex: String = pubkey.iter().map(|b| format!("{b:02x}")).collect();
        newt_core::dock_registry::approve_dock_with_identity(
            &config,
            &identity,
            &fp,
            "laptop-b",
            &pubkey_hex,
            newt_core::dock_registry::DockScope::MirrorInject,
            "tx-1",
        )
        .unwrap();

        // Now the approved peer is admitted; a different pubkey still is not.
        assert!(check_dock_approval(true, Some(&ident), &pubkey).is_ok());
        assert!(check_dock_approval(true, Some(&ident), &[8u8; 32]).is_err());
    }

    #[test]
    fn dock_panel_mirrors_turns_with_an_inject_form() {
        let t = DockedTranscript {
            title: "remote work".into(),
            turns: vec![DockedTurn {
                user: "hi".into(),
                assistant: "STUB_REPLY ok".into(),
            }],
        };
        let html = dock_panel("laptop-b", "conv-123", &t);
        assert!(html.contains("mirror + inject"));
        assert!(html.contains("STUB_REPLY ok"));
        assert!(html.contains("hx-post=\"/dock/inject?peer=laptop-b&conv=conv-123\""));
    }
}

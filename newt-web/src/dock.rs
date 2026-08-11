//! dock — surface OTHER newt-agents' sessions in this cockpit (requirement 2).
//!
//! MVP transport: a loopback/LAN **HTTP** pull. A peer newt-web exposes its
//! sessions at `GET /api/sessions`; this hub fetches each configured peer's list
//! and renders them **read-only** (mirror-only, D2 — the hub never writes a
//! remote transcript). Peers are configured with `NEWT_WEB_DOCK_PEERS`, a
//! comma-separated list of `label=base_url` (or bare `base_url`), e.g.
//! `NEWT_WEB_DOCK_PEERS="laptop-b=http://127.0.0.1:8898,nuc=http://10.0.0.4:8880"`.
//!
//! The [`DockSource`] trait is the seam the eventual agent-mesh `session_streams`
//! transport slots behind without touching the Registry, the cockpit render, or
//! the drive harness — the HTTP source is the MVP, the mesh source is the
//! refinement (`docs/decisions/newt_web_docking.md` K7).

use serde::{Deserialize, Serialize};

/// One remote session as exposed by a peer's `GET /api/sessions`. The same shape
/// this instance emits for its own sessions (see `main::api_sessions`), so a hub
/// and a peer speak one wire type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DockedSession {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub turns: usize,
    /// Whether a live process currently owns the conversation (the "Connected"
    /// signal). Best-effort; a peer that can't tell reports `false`.
    #[serde(default)]
    pub live: bool,
}

/// A remote session's transcript, mirrored read-only over the dock (D2). The
/// same shape a peer's `GET /api/sessions/:id/transcript` emits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DockedTranscript {
    pub title: String,
    pub turns: Vec<DockedTurn>,
}

/// One `(user, assistant)` turn pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DockedTurn {
    pub user: String,
    pub assistant: String,
}

/// A configured dock peer: an operator-supplied label + its cockpit base URL.
#[derive(Debug, Clone)]
pub(crate) struct DockPeer {
    pub label: String,
    pub base_url: String,
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

/// Find a configured peer by its label (the hub panel route resolves the peer
/// the operator clicked). `None` ⇒ an unknown/removed peer, refused fail-closed.
pub(crate) fn peer_by_label(label: &str) -> Option<DockPeer> {
    configured_peers().into_iter().find(|p| p.label == label)
}

/// The configured dock peers, from `NEWT_WEB_DOCK_PEERS`. Empty/unset ⇒ no docks
/// (the common single-box case).
pub(crate) fn configured_peers() -> Vec<DockPeer> {
    std::env::var("NEWT_WEB_DOCK_PEERS")
        .map(|raw| parse_peers(&raw))
        .unwrap_or_default()
}

/// Parse a `NEWT_WEB_DOCK_PEERS` value (`label=url,url2,…`) into peers — pure, so
/// the tests need not mutate process env. A bare URL gets its host as the label.
fn parse_peers(raw: &str) -> Vec<DockPeer> {
    raw.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (label, url) = match entry.split_once('=') {
                Some((l, u)) => (l.trim().to_string(), u.trim().to_string()),
                None => {
                    let host = entry
                        .trim_start_matches("http://")
                        .trim_start_matches("https://")
                        .to_string();
                    (host, entry.to_string())
                }
            };
            if url.is_empty() {
                None
            } else {
                Some(DockPeer {
                    label,
                    base_url: url.trim_end_matches('/').to_string(),
                })
            }
        })
        .collect()
}

/// The dock transport seam. The MVP is [`HttpDockSource`]; the agent-mesh
/// `session_streams` source implements the same trait later.
pub(crate) trait DockSource: Send + Sync {
    /// The remote sessions this dock exposes (mirror-only for the MVP).
    fn sessions(&self) -> Result<Vec<DockedSession>, String>;
    /// One remote session's transcript, mirrored read-only (the "select" path).
    fn transcript(&self, conv_id: &str) -> Result<DockedTranscript, String>;
}

/// The MVP HTTP dock source: `GET {base_url}/api/sessions[/{id}/transcript]`.
pub(crate) struct HttpDockSource {
    pub peer: DockPeer,
}

impl DockSource for HttpDockSource {
    fn sessions(&self) -> Result<Vec<DockedSession>, String> {
        let url = format!("{}/api/sessions", self.peer.base_url);
        let resp = ureq::get(&url)
            .timeout(std::time::Duration::from_secs(3))
            .call()
            .map_err(|e| format!("unreachable: {e}"))?;
        resp.into_json::<Vec<DockedSession>>()
            .map_err(|e| format!("bad /api/sessions payload: {e}"))
    }

    fn transcript(&self, conv_id: &str) -> Result<DockedTranscript, String> {
        let url = format!(
            "{}/api/sessions/{}/transcript",
            self.peer.base_url,
            pct(conv_id)
        );
        let resp = ureq::get(&url)
            .timeout(std::time::Duration::from_secs(3))
            .call()
            .map_err(|e| format!("unreachable: {e}"))?;
        resp.into_json::<DockedTranscript>()
            .map_err(|e| format!("bad transcript payload: {e}"))
    }
}

/// Render a docked remote session's transcript as a **read-only** panel (the
/// mirror side of D2 — no prompt form; inject over a dock is a later
/// refinement). Reuses the cockpit's own transcript renderer so a remote
/// session looks identical to a local one, just marked remote.
pub(crate) fn dock_panel(peer_label: &str, transcript: &DockedTranscript) -> String {
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
        r#"<section class="agent dock-remote">
<h2><span>{title} <small>· {label} · remote (read-only)</small></span></h2>
<div class="transcript">{fragment}</div>
<p class="hint">Mirrored over the dock (D2 — the remote host stays the sole writer). Inject over a dock is a refinement.</p>
</section>"#,
        title = crate::shell::escape(&transcript.title),
        label = crate::shell::escape(peer_label),
        fragment = crate::shell::transcript_fragment(&snap),
    )
}

/// Render the "docked peers" cockpit section: each configured peer with its
/// remote sessions (read-only, mirror-only). An unreachable peer renders a
/// notice rather than dropping — the operator sees the dock is down, not that it
/// vanished. Sessions are fetched off the async runtime (blocking HTTP).
pub(crate) async fn docked_section() -> String {
    let peers = configured_peers();
    if peers.is_empty() {
        return String::new(); // no docks configured — render nothing
    }
    let fetched = tokio::task::spawn_blocking(move || {
        peers
            .into_iter()
            .map(|peer| {
                let result = HttpDockSource { peer: peer.clone() }.sessions();
                (peer, result)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();

    let mut out = String::from(
        r#"<section class="docked"><h2>docked peers</h2><p class="hint">Remote newt-agents' sessions, mirrored read-only (D2). MVP transport: HTTP; agent-mesh next.</p>"#,
    );
    for (peer, result) in &fetched {
        match result {
            Ok(sessions) if sessions.is_empty() => {
                out.push_str(&format!(
                    r#"<div class="peer"><h3>● {label}</h3><p class="empty">no sessions</p></div>"#,
                    label = crate::shell::escape(&peer.label),
                ));
            }
            Ok(sessions) => {
                out.push_str(&format!(
                    r#"<div class="peer"><h3>● {label} <small>· remote</small></h3><ul>"#,
                    label = crate::shell::escape(&peer.label),
                ));
                for s in sessions.iter().take(30) {
                    let dot = if s.live { "▶" } else { "○" };
                    // Selectable: clicking mirrors the remote transcript into the
                    // shared #panel (the "select any docked session" path). The
                    // hub resolves (peer,conv) → the peer's transcript endpoint.
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
                    r#"<div class="peer"><h3>○ {label} <small>· {err}</small></h3></div>"#,
                    label = crate::shell::escape(&peer.label),
                    err = crate::shell::escape(err),
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
    fn peers_parse_labelled_and_bare_and_trim() {
        let peers = parse_peers(" lab-b=http://127.0.0.1:8898/ , http://10.0.0.4:8880 ,, ");
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].label, "lab-b");
        assert_eq!(peers[0].base_url, "http://127.0.0.1:8898"); // trailing / trimmed
        assert_eq!(peers[1].label, "10.0.0.4:8880"); // bare URL → host label
        assert_eq!(peers[1].base_url, "http://10.0.0.4:8880");
    }

    #[test]
    fn empty_peers_value_is_no_docks() {
        assert!(parse_peers("").is_empty());
        assert!(parse_peers("   ").is_empty());
    }

    #[test]
    fn dock_panel_mirrors_turns_read_only() {
        let t = DockedTranscript {
            title: "remote work".into(),
            turns: vec![DockedTurn {
                user: "hi".into(),
                assistant: "STUB_REPLY ok".into(),
            }],
        };
        let html = dock_panel("laptop-b", &t);
        assert!(html.contains("remote (read-only)"));
        assert!(html.contains("laptop-b"));
        assert!(html.contains("STUB_REPLY ok"));
        // Mirror-only: no prompt/inject form in a docked panel.
        assert!(!html.contains("hx-post"));
    }
}

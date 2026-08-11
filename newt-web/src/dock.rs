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

/// A configured dock peer: an operator-supplied label + its cockpit base URL.
#[derive(Debug, Clone)]
pub(crate) struct DockPeer {
    pub label: String,
    pub base_url: String,
}

/// Parse `NEWT_WEB_DOCK_PEERS` into peers. Empty/unset ⇒ no docks (the common
/// single-box case). A bare URL gets its host as the label.
pub(crate) fn configured_peers() -> Vec<DockPeer> {
    let raw = match std::env::var("NEWT_WEB_DOCK_PEERS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Vec::new(),
    };
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
}

/// The MVP HTTP dock source: `GET {base_url}/api/sessions`.
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
                    out.push_str(&format!(
                        r#"<li><span class="s-title">{dot} {title}</span> <small>({n} turns · {label})</small></li>"#,
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
        std::env::set_var(
            "NEWT_WEB_DOCK_PEERS",
            " lab-b=http://127.0.0.1:8898/ , http://10.0.0.4:8880 ,, ",
        );
        let peers = configured_peers();
        std::env::remove_var("NEWT_WEB_DOCK_PEERS");
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].label, "lab-b");
        assert_eq!(peers[0].base_url, "http://127.0.0.1:8898"); // trailing / trimmed
        assert_eq!(peers[1].label, "10.0.0.4:8880"); // bare URL → host label
        assert_eq!(peers[1].base_url, "http://10.0.0.4:8880");
    }

    #[test]
    fn unset_peers_is_empty() {
        std::env::remove_var("NEWT_WEB_DOCK_PEERS");
        assert!(configured_peers().is_empty());
    }
}

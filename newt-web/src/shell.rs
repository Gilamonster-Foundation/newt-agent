//! Server-rendered HTML — the whole front end (vendored HTMX + one inline
//! EventSource hook per agent; no JS toolchain). Every piece of user/model
//! text passes through [`escape`] before it reaches a page.

use crate::agents::{Registry, Snapshot};
use axum::extract::State;
use axum::response::Html;
use std::sync::Arc;

/// Minimal HTML escape for text nodes and attribute values.
pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const STYLE: &str = r#"
  :root { color-scheme: light dark; }
  body { font-family: ui-monospace, monospace; margin: 0; }
  header { padding: 0.5rem 1rem; border-bottom: 1px solid color-mix(in srgb, currentColor 25%, transparent); }
  header h1 { font-size: 1rem; margin: 0; }
  #content { padding: 1rem; display: grid; gap: 1rem; }
  #tabs { display: flex; flex-wrap: wrap; gap: 0.4rem; }
  #tabs button { padding: 0.25rem 0.75rem; border-radius: 4px 4px 0 0; border: 1px solid color-mix(in srgb, currentColor 25%, transparent); background: transparent; cursor: pointer; }
  #tabs button.active { border-bottom-color: transparent; font-weight: bold; }
  .empty { opacity: 0.7; }
  form.spawn { display: flex; flex-wrap: wrap; gap: 0.5rem; align-items: end; }
  form.spawn label { display: grid; font-size: 0.75rem; gap: 0.15rem; }
  input, select, button { font: inherit; }
  .agent { border: 1px solid color-mix(in srgb, currentColor 25%, transparent); border-radius: 4px; }
  .agent h2 { font-size: 0.9rem; margin: 0; padding: 0.4rem 0.75rem; display: flex; justify-content: space-between; gap: 0.5rem; border-bottom: 1px solid color-mix(in srgb, currentColor 15%, transparent); }
  .agent h2 .busy { opacity: 0.7; font-weight: normal; }
  .transcript { padding: 0.5rem 0.75rem; max-height: 50vh; overflow-y: auto; display: grid; gap: 0.4rem; }
  .msg { white-space: pre-wrap; overflow-wrap: anywhere; }
  .msg .role { opacity: 0.6; font-size: 0.75rem; display: block; }
  form.prompt { display: flex; gap: 0.5rem; padding: 0.5rem 0.75rem; border-top: 1px solid color-mix(in srgb, currentColor 15%, transparent); }
  form.prompt input[name=text] { flex: 1; }
"#;

/// The transcript fragment — what the SSE stream carries on every snapshot
/// change (the inline hook swaps it into the panel).
pub(crate) fn transcript_fragment(snap: &Snapshot) -> String {
    let mut msgs = String::new();
    for (role, content) in &snap.messages {
        msgs.push_str(&format!(
            r#"<div class="msg"><span class="role">{}</span>{}</div>"#,
            escape(role),
            escape(content)
        ));
    }
    if snap.messages.is_empty() {
        msgs.push_str(r#"<p class="empty">No turns yet.</p>"#);
    }
    if snap.busy {
        msgs.push_str(r#"<p class="empty">thinking…</p>"#);
    }
    if snap.closed {
        msgs.push_str(r#"<p class="empty">agent closed</p>"#);
    }
    msgs
}

/// One agent's panel: header, live transcript, prompt form, and the
/// EventSource hook feeding the transcript from `/agents/{id}/events`.
pub(crate) fn agent_panel(id: u64, name: &str, model: &str, snap: &Snapshot) -> String {
    format!(
        r##"<section class="agent" id="agent-{id}">
<h2><span>{name} <small>({model})</small></span>
<button hx-delete="/agents/{id}" hx-target="#panel" hx-swap="innerHTML">✕</button></h2>
<div class="transcript" id="transcript-{id}">{fragment}</div>
<form class="prompt" hx-post="/agents/{id}/prompt" hx-swap="none" hx-on::after-request="this.reset()">
<input name="text" placeholder="prompt…" autocomplete="off" required>
<button>send</button>
</form>
<script>
(function () {{
  var es = new EventSource("/agents/{id}/events");
  es.onmessage = function (e) {{
    var t = document.getElementById("transcript-{id}");
    if (t) {{ t.innerHTML = e.data; t.scrollTop = t.scrollHeight; }}
    else {{ es.close(); }}
  }};
}})();
</script>
</section>"##,
        id = id,
        name = escape(name),
        model = escape(model),
        fragment = transcript_fragment(snap),
    )
}

/// The tab strip (W3): one button per live agent, HTMX-swapping the single
/// `#panel` region. Rendered whole on every change and swapped out-of-band
/// from the spawn/delete responses so the strip can never drift from the
/// registry. Switching tabs replaces the panel, which closes the old panel's
/// EventSource (its transcript node vanishes) and opens the new one — the
/// per-view attach/detach the ladder asks for.
pub(crate) fn tab_strip(agents: &[(u64, String, String, Snapshot)], active: Option<u64>) -> String {
    let mut out = String::from(r#"<nav id="tabs" hx-swap-oob="true">"#);
    for (id, name, _model, snap) in agents {
        let class = if Some(*id) == active {
            " class=\"active\""
        } else {
            ""
        };
        let dot = if snap.busy { "● " } else { "" };
        out.push_str(&format!(
            r##"<button{class} hx-get="/agents/{id}/panel" hx-target="#panel" hx-swap="innerHTML">{dot}{name}</button>"##,
            class = class,
            id = id,
            dot = dot,
            name = escape(name),
        ));
    }
    out.push_str("</nav>");
    out
}

/// The cockpit page: spawn form + every live agent's panel.
pub(crate) async fn index(State(reg): State<Arc<Registry>>) -> Html<String> {
    let default_url =
        std::env::var("NEWT_WEB_BACKEND_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let default_model = std::env::var("NEWT_WEB_MODEL").unwrap_or_else(|_| "llama3.1:8b".into());
    let default_ws = std::env::var("NEWT_WEB_WORKSPACE").unwrap_or_else(|_| ".".into());

    let agents = reg.list();
    let active = agents.first().map(|(id, ..)| *id);
    let strip = tab_strip(&agents, active).replace(r#" hx-swap-oob="true""#, ""); // in-page render, not OOB
    let panel = match agents.first() {
        Some((id, name, model, snap)) => agent_panel(*id, name, model, snap),
        None => r#"<p class="empty">No agents yet. Spawn one above.</p>"#.to_string(),
    };

    Html(format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>newt-web</title>
<script src="/assets/htmx.min.js"></script>
<style>{STYLE}</style>
</head>
<body>
<header><h1>newt-web</h1></header>
<main id="content">
<form class="spawn" hx-post="/agents" hx-target="#agents" hx-swap="beforeend">
<label>name<input name="name" value="agent" required></label>
<label>backend url<input name="url" value="{url}" required></label>
<label>model<input name="model" value="{model}" required></label>
<label>kind<select name="kind"><option value="ollama">ollama</option><option value="openai">openai</option></select></label>
<label>workspace<input name="workspace" value="{ws}" required></label>
<button>spawn</button>
</form>
{strip}
<div id="panel">{panel}</div>
</main>
</body>
</html>
"##,
        STYLE = STYLE,
        url = escape(&default_url),
        model = escape(&default_model),
        ws = escape(&default_ws),
        strip = strip,
        panel = panel,
    ))
}

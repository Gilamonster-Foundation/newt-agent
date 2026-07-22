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

/// Render message text as Markdown → sanitized HTML.
///
/// Matches Scrybe's `scrybe-render` (pulldown-cmark, all extensions) so the
/// transcript reads the way the rest of the toolchain renders Markdown — but
/// adds the step Scrybe deliberately skips: the transcript carries UNTRUSTED
/// model/user text, so the generated HTML is run through an `ammonia` allowlist
/// before it reaches the page. `ammonia` drops `<script>`/`<style>`, event-
/// handler attributes, and `javascript:`/`data:` URLs, so a model reply cannot
/// smuggle script into the cockpit. Soft line breaks become hard breaks so
/// chat text keeps its newlines instead of Markdown-collapsing them.
pub(crate) fn render_markdown(src: &str) -> String {
    use pulldown_cmark::{html, Event, Options, Parser};
    let parser = Parser::new_ext(src, Options::all()).map(|ev| match ev {
        Event::SoftBreak => Event::HardBreak,
        other => other,
    });
    let mut raw = String::new();
    html::push_html(&mut raw, parser);
    ammonia::clean(&raw)
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
  .msg { overflow-wrap: anywhere; }
  .msg .role { opacity: 0.6; font-size: 0.75rem; display: block; }
  /* Rendered Markdown body (render_markdown -> ammonia). Tight vertical
     rhythm so a reply reads as one chat bubble, code/tables scroll rather
     than widen the page. Mirrors scrybe-render/src/themes/default.css. */
  .md > :first-child { margin-top: 0; }
  .md > :last-child { margin-bottom: 0; }
  .md p { margin: 0.3rem 0; }
  .md code { background: color-mix(in srgb, currentColor 10%, transparent); padding: 0.1em 0.3em; border-radius: 3px; font-size: 0.9em; }
  .md pre { background: color-mix(in srgb, currentColor 10%, transparent); padding: 0.6rem; border-radius: 6px; overflow-x: auto; }
  .md pre code { background: none; padding: 0; }
  .md blockquote { margin: 0.3rem 0; padding-left: 0.75rem; border-left: 3px solid color-mix(in srgb, currentColor 25%, transparent); opacity: 0.85; }
  .md table { border-collapse: collapse; display: block; overflow-x: auto; }
  .md th, .md td { border: 1px solid color-mix(in srgb, currentColor 20%, transparent); padding: 0.2rem 0.5rem; }
  .md a { color: inherit; }
  form.prompt { display: flex; gap: 0.5rem; padding: 0.5rem 0.75rem; border-top: 1px solid color-mix(in srgb, currentColor 15%, transparent); }
  form.prompt input[name=text] { flex: 1; }
"#;

/// The transcript fragment — what the SSE stream carries on every snapshot
/// change (the inline hook swaps it into the panel).
pub(crate) fn transcript_fragment(snap: &Snapshot) -> String {
    let mut msgs = String::new();
    for (role, content) in &snap.messages {
        msgs.push_str(&format!(
            r#"<div class="msg"><span class="role">{}</span><div class="md">{}</div></div>"#,
            escape(role),
            render_markdown(content)
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
pub(crate) fn agent_panel(
    id: u64,
    name: &str,
    model: &str,
    readonly: bool,
    snap: &Snapshot,
) -> String {
    format!(
        r##"<section class="agent" id="agent-{id}">
<h2><span>{name} <small>({model})</small></span>
<button hx-delete="/agents/{id}" hx-target="#panel" hx-swap="innerHTML">✕</button></h2>
<div class="transcript" id="transcript-{id}">{fragment}</div>
{prompt_form}
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
        prompt_form = if readonly {
            r#"<p class="empty" style="padding:0.5rem 0.75rem">read-only follow — the running session owns this conversation (D2)</p>"#.to_string()
        } else {
            format!(
                r##"<form class="prompt" hx-post="/agents/{id}/prompt" hx-swap="none" hx-on::after-request="this.reset()">
<input name="text" placeholder="prompt…" autocomplete="off" required>
<button>send</button>
</form>"##
            )
        },
    )
}

/// The tab strip (W3): one button per live agent, HTMX-swapping the single
/// `#panel` region. Rendered whole on every change and swapped out-of-band
/// from the spawn/delete responses so the strip can never drift from the
/// registry. Switching tabs replaces the panel, which closes the old panel's
/// EventSource (its transcript node vanishes) and opens the new one — the
/// per-view attach/detach the ladder asks for.
pub(crate) fn tab_strip(
    agents: &[(u64, String, String, bool, Snapshot)],
    active: Option<u64>,
) -> String {
    let mut out = String::from(r#"<nav id="tabs" hx-swap-oob="true">"#);
    for (id, name, _model, readonly, snap) in agents {
        let class = if Some(*id) == active {
            " class=\"active\""
        } else {
            ""
        };
        let dot = if snap.busy {
            "● "
        } else if *readonly {
            "◫ "
        } else {
            ""
        };
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
    // The wire protocol the spawn form pre-selects. Without this the dropdown
    // always defaults to `ollama`, so pointing NEWT_WEB_BACKEND_URL at a vLLM
    // (openai) endpoint spawns an agent that speaks the wrong protocol and
    // fails on first turn. Pre-select the configured kind; unknown values fall
    // through to the plain (ollama-first) option order.
    let default_kind = std::env::var("NEWT_WEB_KIND").unwrap_or_else(|_| "ollama".into());
    let kind_options = ["ollama", "openai"]
        .into_iter()
        .map(|k| {
            let sel = if k == default_kind { " selected" } else { "" };
            format!(r#"<option value="{k}"{sel}>{k}</option>"#)
        })
        .collect::<String>();

    let agents = reg.list();
    let active = agents.first().map(|(id, ..)| *id);
    let strip = tab_strip(&agents, active).replace(r#" hx-swap-oob="true""#, ""); // in-page render, not OOB
    let panel = match agents.first() {
        Some((id, name, model, readonly, snap)) => agent_panel(*id, name, model, *readonly, snap),
        None => r#"<p class="empty">No agents yet. Spawn one above.</p>"#.to_string(),
    };
    let sessions = crate::sessions_section().await;

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
<form class="spawn" hx-post="/agents" hx-target="#panel" hx-swap="innerHTML">
<label>name<input name="name" value="agent" required></label>
<label>backend url<input name="url" value="{url}" required></label>
<label>model<input name="model" value="{model}" required></label>
<label>kind<select name="kind">{kind_options}</select></label>
<label>workspace<input name="workspace" value="{ws}" required></label>
<button>spawn</button>
</form>
{sessions}
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
        sessions = sessions,
        strip = strip,
        panel = panel,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_common_formatting() {
        let out = render_markdown("**bold** and `code`\n\n```\nfn x() {}\n```");
        assert!(out.contains("<strong>bold</strong>"), "bold: {out}");
        assert!(out.contains("<code>code</code>"), "inline code: {out}");
        assert!(out.contains("<pre>"), "fenced block: {out}");
    }

    #[test]
    fn markdown_keeps_soft_breaks_as_line_breaks() {
        // Chat text uses single newlines meaningfully; they must not collapse.
        let out = render_markdown("line one\nline two");
        assert!(
            out.contains("<br"),
            "soft break becomes a hard break: {out}"
        );
    }

    #[test]
    fn markdown_sanitizes_every_xss_vector() {
        let out = render_markdown(
            "[x](javascript:alert(1)) <img src=x onerror=alert(2)> <script>alert(3)</script>",
        );
        assert!(!out.contains("<script"), "script stripped: {out}");
        assert!(!out.contains("alert(3)"), "script body dropped: {out}");
        assert!(!out.contains("onerror"), "event handler stripped: {out}");
        assert!(!out.contains("javascript:"), "js url stripped: {out}");
    }
}

//! Server-rendered HTML — the whole front end (vendored HTMX + one inline
//! EventSource hook per agent; no JS build toolchain). User/model text is
//! either escaped directly or rendered from Markdown through the sanitizer.

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
    use pulldown_cmark::{html, CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};
    let mut in_mermaid = false;
    let parser = Parser::new_ext(src, Options::all()).map(|ev| match ev {
        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
            let is_mermaid = info
                .split_whitespace()
                .next()
                .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"));
            if is_mermaid {
                in_mermaid = true;
                Event::Html(CowStr::Borrowed(
                    r#"<pre class="mermaid" data-markdown-extension="mermaid">"#,
                ))
            } else {
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
            }
        }
        Event::End(TagEnd::CodeBlock) if in_mermaid => {
            in_mermaid = false;
            Event::Html(CowStr::Borrowed("</pre>"))
        }
        Event::SoftBreak => Event::HardBreak,
        other => other,
    });
    let mut raw = String::new();
    html::push_html(&mut raw, parser);
    ammonia::Builder::default()
        .add_tag_attributes("pre", &["class", "data-markdown-extension"])
        .add_tag_attributes("code", &["class"])
        .clean(&raw)
        .to_string()
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
  .sessions h2 { font-size: 0.9rem; margin: 0 0 0.25rem; }
  .sessions .hint { opacity: 0.7; font-size: 0.75rem; margin: 0 0 0.5rem; }
  .sessions ul { list-style: none; margin: 0; padding: 0; display: grid; gap: 0.3rem; }
  .sessions li { display: flex; align-items: baseline; gap: 0.5rem; }
  .sessions .s-title { font-weight: bold; }
  .spawn-wrap { border: 1px dashed color-mix(in srgb, currentColor 25%, transparent); border-radius: 4px; padding: 0.4rem 0.6rem; }
  .spawn-wrap summary { cursor: pointer; font-size: 0.85rem; }
  form.spawn { display: flex; flex-wrap: wrap; gap: 0.5rem; align-items: end; margin-top: 0.5rem; }
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
  .md .mermaid { box-sizing: border-box; max-width: 100%; text-align: center; }
  .md .mermaid svg { display: block; height: auto; max-width: 100%; margin-inline: auto; }
  .md .mermaid-error { text-align: left; border: 1px solid #d9534f; white-space: pre-wrap; }
  .md blockquote { margin: 0.3rem 0; padding-left: 0.75rem; border-left: 3px solid color-mix(in srgb, currentColor 25%, transparent); opacity: 0.85; }
  .md table { border-collapse: collapse; display: block; overflow-x: auto; }
  .md th, .md td { border: 1px solid color-mix(in srgb, currentColor 20%, transparent); padding: 0.2rem 0.5rem; }
  .md a { color: inherit; }
  form.prompt { display: flex; gap: 0.5rem; padding: 0.5rem 0.75rem; border-top: 1px solid color-mix(in srgb, currentColor 15%, transparent); }
  form.prompt input[name=text] { flex: 1; }
  /* A4: pending typed permission form. */
  .perm { margin: 0.5rem 0.75rem; padding: 0.5rem 0.75rem; border-radius: 6px; border: 1px solid color-mix(in srgb, currentColor 30%, transparent); display: grid; gap: 0.35rem; }
  .perm-head { display: flex; justify-content: space-between; align-items: baseline; gap: 0.5rem; }
  .perm-actions { display: flex; gap: 0.4rem; flex-wrap: wrap; }
"#;

/// Render exactly the Markdown and actions published by the running gate.
pub(crate) fn pending_permission_card(id: u64, p: &newt_core::PendingPermission) -> String {
    let Ok(question) = p.question() else {
        return String::new();
    };
    let rid = escape(&p.request_id);
    let actions = question
        .actions
        .iter()
        .map(|action| format!(
            r##"<button hx-post="/agents/{id}/decision" hx-vals='{{"request_id":"{rid}","verdict":"{}"}}' hx-swap="none">{}</button>"##,
            action.value.as_str(), escape(&action.label)
        ))
        .collect::<String>();
    let note = question
        .note
        .as_deref()
        .map(render_markdown)
        .unwrap_or_default();
    format!(
        r##"<div class="perm">
<div class="perm-head"><strong>Permission needed</strong></div>
<div class="md">{body}{note}</div>
<div class="perm-actions">{actions}</div>
</div>"##,
        body = render_markdown(&question.markdown),
    )
}

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
    if (t) {{
      t.innerHTML = e.data;
      if (window.newtEnhanceMarkdown) {{ window.newtEnhanceMarkdown(t); }}
      t.scrollTop = t.scrollHeight;
    }}
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
            // A3/W6: a followed tab injects into the running session's inbox
            // (D2 — it stays the writer; the mirror shows the reply once the
            // session consumes it). Same POST target; the route routes it.
            format!(
                r##"<div id="pending-{id}" hx-get="/agents/{id}/pending" hx-trigger="load, every 2s" hx-swap="innerHTML"></div>
<p class="empty" style="padding:0.4rem 0.75rem 0">→ injects into the running session (it stays the writer — D2)</p>
<form class="prompt" hx-post="/agents/{id}/prompt" hx-swap="none" hx-on::after-request="this.reset()">
<input name="text" placeholder="inject a prompt…" autocomplete="off" required>
<button>inject</button>
</form>"##
            )
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
    let docked = crate::dock::docked_section().await;

    Html(format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>newt-web</title>
<script src="/assets/htmx.min.js"></script>
<script defer src="/assets/mermaid.min.js"></script>
<script defer src="/assets/markdown.js"></script>
<style>{STYLE}</style>
</head>
<body>
<header><h1>newt-web</h1></header>
<main id="content">
<div id="overview" hx-get="/overview" hx-trigger="every 3s" hx-swap="innerHTML">{docked}{sessions}</div>
<details class="spawn-wrap">
<summary>+ new scratch agent <small>(not saved — start durable sessions above)</small></summary>
<form class="spawn" hx-post="/agents" hx-target="#panel" hx-swap="innerHTML">
<label>name<input name="name" value="agent" required></label>
<label>backend url<input name="url" value="{url}" required></label>
<label>model<input name="model" value="{model}" required></label>
<label>kind<select name="kind">{kind_options}</select></label>
<label>workspace<input name="workspace" value="{ws}" required></label>
<button>spawn</button>
</form>
</details>
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
        docked = docked,
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
    fn markdown_marks_mermaid_fences_for_progressive_enhancement() {
        let out = render_markdown(
            "before\n\n```mermaid\ngraph TD\n  A[Markdown] --> B[Web or TUI]\n```\n\nafter",
        );
        assert!(
            out.contains(r#"<pre class="mermaid" data-markdown-extension="mermaid">graph TD"#),
            "Mermaid fence becomes an enrichment hook: {out}"
        );
        assert!(
            !out.contains(r#"class="language-mermaid""#),
            "Mermaid must not remain an ordinary code fence: {out}"
        );
        assert!(out.contains("<p>before</p>"), "surrounding Markdown: {out}");
        assert!(out.contains("<p>after</p>"), "surrounding Markdown: {out}");
    }

    #[test]
    fn markdown_keeps_non_mermaid_fences_as_code() {
        let out = render_markdown("```rust\nfn main() {}\n```");
        assert!(
            out.contains(r#"<code class="language-rust">"#),
            "ordinary fences keep their language: {out}"
        );
        assert!(!out.contains(r#"class="mermaid""#), "not Mermaid: {out}");
    }

    #[test]
    fn mermaid_source_is_still_untrusted_text() {
        let out = render_markdown("```mermaid\ngraph TD\n  A[<script>alert(1)</script>]\n```");
        assert!(!out.contains("<script"), "script tag escaped: {out}");
        assert!(
            out.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "diagram source remains inert text: {out}"
        );
    }

    #[test]
    fn live_panel_reenhances_markdown_after_sse_updates() {
        let panel = agent_panel(7, "a", "m", false, &Snapshot::default());
        assert!(
            panel.contains("window.newtEnhanceMarkdown(t)"),
            "SSE replacement must rerun generic Markdown enhancements: {panel}"
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

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
/// Parses in the ONE Newt Markup dialect (`newt_core::markup::dialect::parse`)
/// — the same events the ANSI scroller sees, so the two surfaces cannot drift.
/// Before C3a (#1857) this site chose `Options::all()` to match Scrybe's
/// `scrybe-render`; that matrix silently swallowed newt's own `+++` envelope
/// and re-punctuated text the model is re-sent verbatim, so it is gone. See
/// the dialect module for the full reasoning and the two sanctioned
/// rendering-side divergences.
///
/// The transcript carries UNTRUSTED model, tool, and docked-peer text
/// (epic law 11), so the generated HTML is run through an `ammonia` allowlist
/// before it reaches the page: `<script>`/`<style>`, event-handler attributes,
/// and `javascript:`/`data:` URLs are dropped. The corpus in
/// `tests::c3a_xss` is what keeps that claim honest.
///
/// Soft line breaks become hard breaks so chat text keeps its newlines — one
/// of the two divergences the dialect module names, not a second dialect.
pub(crate) fn render_markdown(src: &str) -> String {
    use pulldown_cmark::{html, CodeBlockKind, Event, Tag, TagEnd};

    // SANITIZE FIRST, THEN WRAP (#1848). The extension marker is built
    // AFTER `ammonia` has run and is never present in its input, so it
    // cannot be forged — not merely filtered. `data-markdown-extension`
    // and `class` are deliberately NOT allowlisted on `pre`: `ammonia`
    // allowlists by TAG, with no way to tell newt's element from one the
    // model typed, and raw HTML in a transcript reaches it as
    // `Event::Html` looking identical (epic law 11).
    let sanitize = |raw: &str| -> String {
        ammonia::Builder::default()
            .add_tag_attributes("code", &["class"])
            .clean(raw)
            .to_string()
    };

    let mut out = String::new();
    let mut buffered: Vec<Event> = Vec::new();
    // Nesting depth of open tags. A fence is enhanceable only at depth 0:
    // splitting the event stream mid-list or mid-blockquote would change
    // what the Markdown MEANS, and a diagram is not worth that.
    let mut depth = 0usize;
    let mut diagram: Option<String> = None;

    let flush = |out: &mut String, buffered: &mut Vec<Event>| {
        if buffered.is_empty() {
            return;
        }
        let mut raw = String::new();
        html::push_html(&mut raw, buffered.drain(..));
        out.push_str(&sanitize(&raw));
    };

    for event in newt_core::markup::dialect::parse(src) {
        // Collecting a diagram: its body is TEXT, kept verbatim for escaping.
        if let Some(source) = diagram.as_mut() {
            match event {
                Event::Text(ref text) => {
                    source.push_str(text);
                    continue;
                }
                Event::End(TagEnd::CodeBlock) => {
                    let source = diagram.take().unwrap_or_default();
                    flush(&mut out, &mut buffered);
                    // Built here, downstream of the sanitizer.
                    out.push_str(r#"<pre class="mermaid" data-markdown-extension="mermaid">"#);
                    out.push_str(&escape(&source));
                    out.push_str("</pre>\n");
                    continue;
                }
                _ => continue,
            }
        }

        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(ref info)))
                if depth == 0 && is_diagram_fence(info) =>
            {
                diagram = Some(String::new());
            }
            Event::SoftBreak => buffered.push(Event::HardBreak),
            other => {
                match &other {
                    Event::Start(_) => depth += 1,
                    Event::End(_) => depth = depth.saturating_sub(1),
                    _ => {}
                }
                buffered.push(other);
            }
        }
    }
    flush(&mut out, &mut buffered);
    out
}

/// Whether a fence's info string names the diagram extension.
fn is_diagram_fence(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
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
pub(crate) fn pending_permission_card(id: u64, p: &newt_core::PendingOffer) -> String {
    let Ok(question) = p.question() else {
        return String::new();
    };
    let rid = escape(&p.instance_id);
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

/// One agent's panel: header, live transcript, and prompt form.
///
/// The transcript carries its stream URL as DATA (`data-agent-stream`);
/// `assets/panel.js` supplies the behaviour. Nothing here is inline script,
/// which is what lets the shell page serve a nonce'd CSP at all (#1854) —
/// a fragment cannot carry a nonce, because it is swapped into a page whose
/// header came from an earlier response.
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
<div class="transcript" id="transcript-{id}" data-agent-stream="/agents/{id}/events"
     aria-live="polite" aria-atomic="false">{fragment}</div>
{prompt_form}
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
<script defer src="/assets/panel.js"></script>
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

    /// **#1848: the extension marker must be unforgeable.**
    ///
    /// The client enhances on `[data-markdown-extension="mermaid"]`
    /// (`assets/markdown.js`). Raw HTML in the transcript passes through
    /// pulldown-cmark as `Event::Html`, so if the sanitizer allowlists that
    /// attribute by TAG, an element the model typed arrives at the client
    /// indistinguishable from the one newt mints — and every bound placed
    /// at the extension fence is routed around. Epic law 11.
    ///
    /// The vectors below are deliberately UNINDENTED: an indented block is
    /// a code block, and the marker would then be inert escaped text
    /// rather than a live attribute — which would make this test pass for
    /// the wrong reason.
    #[test]
    fn authored_extension_marker_is_not_enhanced() {
        let out = render_markdown(
            "here is a diagram\n\n<pre class=\"mermaid\" data-markdown-extension=\"mermaid\">graph TD\nA--&gt;B</pre>\n",
        );
        assert!(
            !out.contains("data-markdown-extension"),
            "authored markup minted the extension marker: {out}"
        );
        assert!(
            !out.contains(r#"<pre class="mermaid""#),
            "authored markup kept the enhancement class: {out}"
        );
    }

    /// The legitimate path is unbroken.
    #[test]
    fn a_real_mermaid_fence_still_enhances() {
        let out = render_markdown("```mermaid\ngraph TD\n  A[x] --> B[y]\n```");
        assert!(
            out.contains(r#"<pre class="mermaid" data-markdown-extension="mermaid">"#),
            "a real fence must still be marked: {out}"
        );
        assert!(
            out.contains("graph TD"),
            "the diagram source must survive: {out}"
        );
    }

    /// `class` is allowlisted on the same line as the data attribute, so it
    /// is the same forgery surface and gets the same treatment — the client
    /// styles `.mermaid`, and a forged one is a rendering claim too.
    #[test]
    fn authored_pre_and_code_classes_cannot_forge_the_marker() {
        let out = render_markdown(
            "<pre class=\"mermaid\">graph TD\nA--&gt;B</pre>\n\n<code class=\"mermaid\">graph TD</code>\n",
        );
        assert!(
            !out.contains("data-markdown-extension"),
            "authored markup minted the marker: {out}"
        );
        assert!(
            !out.contains(r#"<pre class="mermaid""#),
            "authored markup kept the enhancement class on a pre: {out}"
        );
    }

    /// **The documented degradation.** A fence nested inside a list or
    /// blockquote is NOT enhanced: enhancing it would mean splitting the
    /// event stream mid-container, which changes what the Markdown means.
    /// It falls back to an ordinary code block — the source is still
    /// readable, which is law 5's fallback rather than a silent drop.
    #[test]
    fn a_nested_mermaid_fence_falls_back_to_source() {
        let out = render_markdown("- item\n\n  ```mermaid\n  graph TD\n  ```\n");
        assert!(
            !out.contains("data-markdown-extension"),
            "a nested fence must not be enhanced: {out}"
        );
        assert!(
            out.contains("graph TD"),
            "but its source must still be readable: {out}"
        );
        assert!(out.contains("<li>"), "the list structure survives: {out}");
    }

    /// **Anti-vacuous twin.** Every assertion above is a `!contains`, which
    /// a renderer that emitted nothing at all would satisfy. This pins that
    /// the checks can see a marker when one is really there, and that the
    /// authored text they are run over does still render — so they are
    /// measuring a stripped attribute, not an empty string.
    #[test]
    fn the_forged_marker_check_can_fail() {
        let seeded = format!(
            "{}{}",
            render_markdown("ordinary text"),
            r#"<pre class="mermaid" data-markdown-extension="mermaid">graph TD</pre>"#
        );
        assert!(
            seeded.contains("data-markdown-extension"),
            "the check cannot see a marker even when one is present"
        );
        // The authored vector still RENDERS — the attribute is stripped,
        // the content is not silently dropped.
        let out = render_markdown(
            "<pre class=\"mermaid\" data-markdown-extension=\"mermaid\">graph TD</pre>\n",
        );
        assert!(!out.contains("data-markdown-extension"), "{out}");
        assert!(out.contains("graph TD"), "content vanished entirely: {out}");
    }

    /// The panel declares its stream as DATA and carries no script.
    ///
    /// Re-enhancement after an SSE replacement still happens — it moved to
    /// `assets/panel.js`, which is what lets the page serve a CSP (#1854).
    /// Asserting the behaviour is still *reachable* rather than that it is
    /// inline is the point of the move.
    #[test]
    fn live_panel_declares_its_stream_without_inline_script() {
        let panel = agent_panel(7, "a", "m", false, &Snapshot::default());
        assert!(
            panel.contains(r#"data-agent-stream="/agents/7/events""#),
            "the panel must declare its stream as data: {panel}"
        );
        assert!(
            !panel.contains("<script"),
            "a fragment cannot carry a nonce, so it must carry no script: {panel}"
        );
        assert!(
            newt_web::csp::PANEL_JS.contains("window.newtEnhanceMarkdown"),
            "the behaviour must still exist, in the served asset"
        );
        assert!(
            newt_web::csp::PANEL_JS.contains("newtAttachStreams"),
            "the asset must expose its scan entry point"
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

    /// **C3a (#1857): the TUI/web dialect conformance pairs.**
    ///
    /// The A0 inventory's §5.7 gap, closed: *"no test pins the TUI-vs-web
    /// dialect divergence (extension set / soft-break policy)."* Both
    /// surfaces are driven over the SAME source and the relationship
    /// between their outputs is asserted — the parts that must agree
    /// because they are one dialect, and the two parts that are allowed to
    /// differ because they are one dialect projected into two media.
    ///
    /// This is the test that makes "one dialect" checkable rather than
    /// aspirational. The ratchet in `newt-core/tests/markup_sprawl_ratchet.rs`
    /// proves there is one option matrix in the SOURCE; this proves the two
    /// surfaces actually behave like it.
    mod c3a {
        use super::render_markdown as web;

        /// The terminal surface, at the settings the TUI uses for a reply.
        fn tui(src: &str) -> String {
            newt_core::agentic::render_markdown(
                src,
                newt_core::agentic::RenderOpts {
                    color: true,
                    cols: 80,
                },
            )
        }

        /// A construct outside the canonical extension set. Neither surface
        /// may interpret it, and — ADR law 5 — its source text must stay
        /// VISIBLE on both rather than being silently consumed.
        struct Unrecognized {
            what: &'static str,
            src: &'static str,
            /// Text that must survive, legibly, on both surfaces.
            visible: &'static str,
        }

        /// Every extension `Options::all()` used to switch on for the web
        /// and the canonical dialect does not.
        const OUTSIDE_THE_DIALECT: &[Unrecognized] = &[
            // The decisive pair. Newt Markup's `+++` envelope has a
            // sanctioned grammar (`newt_core::markup::strip_newt_metadata`).
            // pulldown's metadata-block extension is a SECOND, competing
            // implementation of it that `push_html` renders as NOTHING
            // (`in_non_writing_block`), so an envelope vanished from the web
            // transcript while the terminal showed it. Silent, and
            // disagreeing with the splitter A1 shipped.
            Unrecognized {
                what: "+++ envelope (pulldown's metadata block, not newt's)",
                src: "+++\ntitle = \"pinned\"\n+++\n\nbody text\n",
                visible: "pinned",
            },
            Unrecognized {
                what: "--- envelope (YAML-style metadata block)",
                src: "---\ntitle: pinned\n---\n\nbody text\n",
                visible: "pinned",
            },
            Unrecognized {
                what: "footnotes",
                src: "claim[^1]\n\n[^1]: the note\n",
                visible: "[^1]",
            },
            Unrecognized {
                what: "heading attributes",
                src: "# Title {#custom-anchor}\n",
                visible: "{#custom-anchor}",
            },
            Unrecognized {
                what: "math",
                src: "the value $x^2 + 1$ here\n",
                visible: "$x^2 + 1$",
            },
            Unrecognized {
                what: "definition lists",
                src: "term\n\n: the definition\n",
                visible: ": the definition",
            },
        ];

        /// **Extensions outside the dialect are inert on BOTH surfaces.**
        #[test]
        fn the_dialect_conformance_pairs_agree() {
            let mut problems = Vec::new();

            for case in OUTSIDE_THE_DIALECT {
                let (t, w) = (tui(case.src), web(case.src));
                if !t.contains(case.visible) {
                    problems.push(format!(
                        "[{}] TUI dropped {:?}:\n{t}",
                        case.what, case.visible
                    ));
                }
                if !w.contains(case.visible) {
                    problems.push(format!(
                        "[{}] WEB dropped {:?} — the surfaces disagree, so \
                         this is a second dialect:\n{w}",
                        case.what, case.visible
                    ));
                }
            }

            // Smart punctuation is its own case: nothing is dropped, the
            // BYTES are rewritten. The canonical dialect excludes it for
            // exactly that reason — the transcript is re-sent to the model
            // verbatim, so the view must not quietly re-punctuate it.
            let punct = "she said \"go\" -- then left...\n";
            for (surface, out) in [("TUI", tui(punct)), ("WEB", web(punct))] {
                for rewritten in ['\u{201c}', '\u{201d}', '\u{2013}', '\u{2026}'] {
                    if out.contains(rewritten) {
                        problems.push(format!(
                            "[smart punctuation] {surface} rewrote {rewritten:?} \
                             into the text; the canonical dialect keeps bytes:\n{out}"
                        ));
                    }
                }
            }

            // The three canonical extensions ARE live on both — otherwise
            // "they agree" could be satisfied by a renderer that interprets
            // nothing at all.
            let strike = ("~~struck~~", tui("~~struck~~"), web("~~struck~~"));
            if !strike.1.contains("\u{1b}[9m") {
                problems.push(format!(
                    "[strikethrough] TUI did not style it:\n{}",
                    strike.1
                ));
            }
            if !strike.2.contains("<del>") {
                problems.push(format!(
                    "[strikethrough] WEB did not mark it:\n{}",
                    strike.2
                ));
            }
            let table_src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
            if !tui(table_src).contains('\u{2500}') {
                problems.push("[tables] TUI did not draw a table".into());
            }
            if !web(table_src).contains("<table>") {
                problems.push("[tables] WEB did not build a table".into());
            }
            let task_src = "- [x] done\n";
            if !tui(task_src).contains('\u{2713}') {
                problems.push("[task lists] TUI did not mark the item".into());
            }
            if web(task_src).contains("[x]") {
                problems.push("[task lists] WEB left the raw marker".into());
            }

            assert!(
                problems.is_empty(),
                "C3a conformance:\n{}",
                problems.join("\n\n")
            );
        }

        /// **The two sanctioned divergences, pinned so they cannot drift.**
        ///
        /// One dialect, two media. These are the only places the surfaces
        /// are allowed to differ, and naming them here is what stops a
        /// third difference appearing as "well, the web already differs".
        #[test]
        fn the_sanctioned_divergences_are_exactly_two() {
            // 1. Soft breaks. CommonMark folds them; chat text uses single
            //    newlines meaningfully, so the web keeps them as <br> while
            //    the scroller — which has real lines — folds to a space.
            let soft = "alpha\nbravo";
            assert!(
                tui(soft).contains("alpha bravo"),
                "TUI folds a soft break: {}",
                tui(soft)
            );
            assert!(
                web(soft).contains("<br"),
                "web keeps a soft break: {}",
                web(soft)
            );

            // 2. Raw HTML. A scroller has no DOM, so it prints markup as
            //    literal text and executes nothing. A page HAS a DOM, so it
            //    must remove what it will not execute. Same event, opposite
            //    correct handling.
            let raw = "<script>alert(1)</script>";
            assert!(
                tui(raw).contains("<script>"),
                "TUI shows raw HTML as text: {}",
                tui(raw)
            );
            assert!(
                !web(raw).contains("<script"),
                "web sanitizes raw HTML: {}",
                web(raw)
            );
        }
    }

    /// **C3a (#1857): a real XSS corpus, not one case.**
    ///
    /// Before this the web surface had three vectors, written twice. The
    /// transcript carries model output, tool output, and — over a dock — a
    /// remote peer's transcript, all of which are untrusted authored markup
    /// (epic law 11). A sanitizer is only as good as the corpus that keeps
    /// it honest, and a corpus of three proves almost nothing about a
    /// tag-based allowlist.
    ///
    /// Every vector is run through one shared battery, so adding a vector
    /// costs one line and inherits every check.
    #[cfg(test)]
    mod c3a_xss {
        use super::render_markdown;

        struct Vector {
            what: &'static str,
            src: &'static str,
        }

        /// Elements that must never appear in the output at all.
        ///
        /// A raw-substring check is already correct for these: sanitized
        /// text nodes escape `<` to `&lt;`, so a surviving `<script` can
        /// only be a real element.
        const FORBIDDEN_ELEMENTS: &[&str] = &[
            // Script execution containers.
            "<script",
            "<iframe",
            "<object",
            "<embed",
            "<template",
            "<noscript",
            // Style/behaviour injection and document-level rewrites.
            "<style",
            "<base",
            "<meta",
            "<link",
            "<form",
            // Foreign content: the classic mXSS reparsing surface.
            "<svg",
            "<math",
        ];

        /// Substrings that must never appear **inside a tag**.
        ///
        /// Attribute position is the whole question. Text nodes are escaped
        /// by construction, so `javascript:` sitting in prose is inert — and
        /// checking the whole document instead is what makes two harmless
        /// outputs look like leaks: `<javascript:alert(1)>`, whose `href`
        /// ammonia removed leaving the scheme as inert TEXT, and any vector
        /// quoted inside a code fence, which must stay readable (law 5).
        ///
        /// The pre-existing web tests check the bare scheme against the
        /// whole page and pass only because their three vectors happen to
        /// lose the text too. That is the weaker property; this is the one
        /// that means "no script-bearing attribute reached the DOM".
        const FORBIDDEN_IN_TAGS: &[&str] = &[
            // Script-bearing URL schemes.
            "javascript:",
            "vbscript:",
            "data:text/html",
            // Event handlers, by delivery not by name.
            "onerror=",
            "onload=",
            "onclick=",
            "onfocus=",
            "onmouseover=",
            "ontoggle=",
            "onanimationstart=",
            "onbegin=",
            "onstart=",
            // Attribute-borne script and framing.
            "srcdoc=",
            "formaction=",
            "xlink:href=",
            // The #1848/#1855 property: authored markup may never mint the
            // client's enhancement marker. Sanitizing is not enough — the
            // marker is appended AFTER the sanitizer, so it is absent from
            // its input and forgery is unrepresentable rather than filtered.
            // This corpus keeps that true as vectors are added.
            "data-markdown-extension",
        ];

        /// Everything between `<` and `>` — every tag's attribute region.
        ///
        /// Deliberately strict in the safe direction: a needle appearing in
        /// an allowlisted *data* attribute (a `title=`, say) would be
        /// reported. For a corpus of chosen vectors that trade is right.
        fn tag_regions(html: &str) -> String {
            let mut out = String::new();
            let mut rest = html;
            while let Some(open) = rest.find('<') {
                rest = &rest[open..];
                let close = rest.find('>').map_or(rest.len(), |i| i + 1);
                out.push_str(&rest[..close]);
                out.push('\n');
                rest = &rest[close..];
            }
            out
        }

        const CORPUS: &[Vector] = &[
            Vector { what: "bare script tag", src: "<script>alert(1)</script>" },
            Vector { what: "case-varied script tag", src: "<ScRiPt>alert(1)</ScRiPt>" },
            Vector { what: "img event handler", src: "<img src=x onerror=alert(1)>" },
            Vector { what: "svg onload", src: "<svg onload=alert(1)>" },
            Vector { what: "svg animate xlink", src: "<svg><a xlink:href=\"javascript:alert(1)\"><text>x</text></a></svg>" },
            Vector { what: "body onload", src: "<body onload=alert(1)>" },
            Vector { what: "details ontoggle", src: "<details open ontoggle=alert(1)>x</details>" },
            Vector { what: "markdown link, js scheme", src: "[x](javascript:alert(1))" },
            Vector { what: "markdown link, case-varied scheme", src: "[x](JaVaScRiPt:alert(1))" },
            Vector { what: "markdown link, entity-encoded scheme", src: "[x](&#106;avascript:alert(1))" },
            Vector { what: "markdown link, vbscript", src: "[x](vbscript:msgbox(1))" },
            Vector { what: "markdown link, data html", src: "[x](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)" },
            Vector { what: "markdown image, js scheme", src: "![x](javascript:alert(1))" },
            Vector { what: "reference link definition", src: "[x][ref]\n\n[ref]: javascript:alert(1)\n" },
            Vector { what: "autolink, js scheme", src: "<javascript:alert(1)>" },
            Vector { what: "anchor with handler", src: "<a href=\"#\" onclick=\"alert(1)\">x</a>" },
            Vector { what: "iframe srcdoc", src: "<iframe srcdoc=\"&lt;script&gt;alert(1)&lt;/script&gt;\"></iframe>" },
            Vector { what: "object data", src: "<object data=\"javascript:alert(1)\"></object>" },
            Vector { what: "embed src", src: "<embed src=\"javascript:alert(1)\">" },
            Vector { what: "base href rewrite", src: "<base href=\"https://evil.test/\">" },
            Vector { what: "meta refresh", src: "<meta http-equiv=\"refresh\" content=\"0;url=javascript:alert(1)\">" },
            Vector { what: "stylesheet link", src: "<link rel=\"stylesheet\" href=\"https://evil.test/x.css\">" },
            Vector { what: "style block", src: "<style>body{background:url('javascript:alert(1)')}</style>" },
            Vector { what: "form with formaction", src: "<form action=\"https://evil.test\"><button formaction=\"javascript:alert(1)\">go</button></form>" },
            Vector { what: "mXSS via noscript title", src: "<noscript><p title=\"</noscript><img src=x onerror=alert(1)>\">" },
            Vector { what: "mXSS via template", src: "<template><script>alert(1)</script></template>" },
            Vector { what: "mXSS via math mglyph", src: "<math><mtext><table><mglyph><style><![CDATA[</style><img src=x onerror=alert(1)>" },
            Vector { what: "mXSS via svg foreignObject", src: "<svg><foreignObject><div><script>alert(1)</script></div></foreignObject></svg>" },
            Vector { what: "comment-wrapped script", src: "<!--><script>alert(1)</script>-->" },
            Vector { what: "forged extension marker", src: "<pre class=\"mermaid\" data-markdown-extension=\"mermaid\">graph TD</pre>" },
            Vector { what: "forged marker inside a table cell", src: "| a |\n|---|\n| <pre data-markdown-extension=\"mermaid\">graph TD</pre> |\n" },
            Vector { what: "forged marker via code class", src: "<code class=\"mermaid\" data-markdown-extension=\"mermaid\">x</code>" },
            Vector { what: "handler split across a soft break", src: "<img src=x\nonerror=alert(1)>" },
            Vector { what: "handler inside a blockquote", src: "> <img src=x onerror=alert(1)>\n" },
            Vector { what: "handler inside a list item", src: "- <img src=x onerror=alert(1)>\n" },
            Vector { what: "nested emphasis around a handler", src: "*<img src=x onerror=alert(1)>*" },
        ];

        /// **Every vector, one battery.**
        #[test]
        fn the_xss_corpus_is_sanitised() {
            let mut problems = Vec::new();
            for v in CORPUS {
                let out = render_markdown(v.src);
                let doc = out.to_ascii_lowercase();
                let tags = tag_regions(&doc);
                for bad in FORBIDDEN_ELEMENTS {
                    if doc.contains(bad) {
                        problems.push(format!("[{}] leaked element {bad:?}:\n{out}", v.what));
                    }
                }
                for bad in FORBIDDEN_IN_TAGS {
                    if tags.contains(bad) {
                        problems.push(format!("[{}] leaked attribute {bad:?}:\n{out}", v.what));
                    }
                }
            }
            assert!(
                problems.is_empty(),
                "{} XSS vector(s) survived sanitation:\n{}",
                problems.len(),
                problems.join("\n\n")
            );
        }

        /// **A fenced block keeps its content as inert text.**
        ///
        /// The corpus above asserts absence, which a renderer that dropped
        /// everything would satisfy. Vectors that appear inside code must
        /// still be READABLE — stripped-to-nothing is a different bug from
        /// sanitized, and law 5 asks for a visible fallback.
        #[test]
        fn a_vector_inside_a_fence_stays_readable_text() {
            let out = render_markdown("```\n<img src=x onerror=alert(1)>\n```");
            assert!(
                out.contains("&lt;img src=x onerror=alert(1)&gt;"),
                "fenced source must survive as escaped text: {out}"
            );
            assert!(
                !tag_regions(&out).contains("onerror="),
                "…but never as an attribute: {out}"
            );
        }

        /// **Anti-vacuous twin.** The battery is all `!contains`, so a
        /// renderer returning `""` would pass every vector. This proves the
        /// battery can see what it is looking for, and that ordinary
        /// content is not being silently eaten alongside the attacks.
        #[test]
        fn the_xss_battery_can_fail() {
            // The battery fires on a genuinely dangerous string…
            let seeded = "<script>alert(1)</script>".to_ascii_lowercase();
            assert!(
                FORBIDDEN_ELEMENTS.iter().any(|bad| seeded.contains(bad)),
                "the battery cannot see a live script tag"
            );
            let tags = tag_regions(&"<img src=x onerror=alert(1)>".to_ascii_lowercase());
            assert!(
                FORBIDDEN_IN_TAGS.iter().any(|bad| tags.contains(bad)),
                "the battery cannot see a live event handler"
            );
            // …and it does NOT fire on the same bytes as inert text, which
            // is the false positive that motivated tag_regions.
            let inert = tag_regions("<p>javascript:alert(1)</p>");
            assert!(
                !FORBIDDEN_IN_TAGS.iter().any(|bad| inert.contains(bad)),
                "an inert scheme in a text node must not read as a leak"
            );
            // …and the corpus is not silently empty.
            assert!(
                CORPUS.len() >= 30,
                "a corpus of {} is not a corpus",
                CORPUS.len()
            );
            // Benign content in the same shapes still renders.
            for (src, want) in [
                ("**bold**", "<strong>bold</strong>"),
                ("[link](https://example.test/)", "https://example.test/"),
                ("`code`", "<code>code</code>"),
                ("| a |\n|---|\n| 1 |\n", "<table>"),
            ] {
                let out = render_markdown(src);
                assert!(out.contains(want), "benign {src:?} lost {want:?}: {out}");
            }
        }
    }
}

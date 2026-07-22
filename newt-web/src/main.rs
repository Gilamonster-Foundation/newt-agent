//! newt-web — the HTMX web cockpit (#1331, decision record
//! `docs/decisions/newt_web_htmx.md`).
//!
//! W2: spawn-and-drive. Agents are `TurnDriver`s owned by pump tasks
//! (`agents.rs`); the front end is server-rendered HTML (`shell.rs`); this
//! file is the composition root — routes, state, and the SSE bridge only.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Form, Router};
use std::convert::Infallible;
use std::sync::Arc;

mod agents;
mod shell;

use agents::{Registry, Spec};

fn app() -> Router {
    let reg = Arc::new(Registry::default());
    Router::new()
        .route("/", get(shell::index))
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/assets/htmx.min.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../assets/htmx.min.js"),
                )
            }),
        )
        .route("/agents", post(spawn_agent))
        .route("/follow", post(follow_session))
        .route("/agents/:id/panel", get(agent_panel_route))
        .route("/agents/:id/prompt", post(prompt_agent))
        .route("/agents/:id/events", get(agent_events))
        .route("/agents/:id", axum::routing::delete(delete_agent))
        .with_state(reg)
}

#[derive(serde::Deserialize)]
struct SpawnForm {
    name: String,
    url: String,
    model: String,
    kind: String,
    workspace: String,
}

/// POST /agents — spawn; respond with the new agent's panel (targeted at
/// `#panel`, activating the tab) plus an out-of-band refresh of the strip.
async fn spawn_agent(
    State(reg): State<Arc<Registry>>,
    Form(form): Form<SpawnForm>,
) -> Html<String> {
    let kind = match form.kind.as_str() {
        "openai" => newt_core::BackendKind::Openai,
        _ => newt_core::BackendKind::Ollama,
    };
    let id = reg.spawn(Spec {
        name: form.name.clone(),
        url: form.url,
        model: form.model.clone(),
        kind,
        workspace: form.workspace,
    });
    let panel = shell::agent_panel(
        id,
        &form.name,
        &form.model,
        false,
        &agents::Snapshot::default(),
    );
    let strip = shell::tab_strip(&reg.list(), Some(id));
    Html(format!("{panel}\n{strip}"))
}

/// GET /agents/:id/panel — the tab body (view attach: opening a tab opens its
/// SSE; the replaced panel's EventSource closes itself when its node vanishes).
async fn agent_panel_route(
    State(reg): State<Arc<Registry>>,
    Path(id): Path<u64>,
) -> Result<Html<String>, StatusCode> {
    let agents = reg.list();
    let (aid, name, model, readonly, snap) = agents
        .iter()
        .find(|(aid, ..)| *aid == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let panel = shell::agent_panel(*aid, name, model, *readonly, snap);
    let strip = shell::tab_strip(&agents, Some(id));
    Ok(Html(format!("{panel}\n{strip}")))
}

#[derive(serde::Deserialize)]
struct PromptForm {
    text: String,
}

/// POST /agents/:id/prompt — submit a prompt; 204 (the SSE stream carries the
/// visible effect), 404 for an unknown agent.
async fn prompt_agent(
    State(reg): State<Arc<Registry>>,
    Path(id): Path<u64>,
    Form(form): Form<PromptForm>,
) -> StatusCode {
    if reg.prompt(id, form.text) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// DELETE /agents/:id — shut the agent down; the response clears the panel
/// region and refreshes the strip out-of-band.
async fn delete_agent(State(reg): State<Arc<Registry>>, Path(id): Path<u64>) -> impl IntoResponse {
    if reg.remove(id) {
        let agents = reg.list();
        let body = format!(
            r#"<p class="empty">Agent closed. Pick a tab or spawn a new one.</p>
{}"#,
            shell::tab_strip(&agents, None)
        );
        (StatusCode::OK, Html(body))
    } else {
        (StatusCode::NOT_FOUND, Html(String::new()))
    }
}

/// GET /agents/:id/events — the SSE bridge: one event per snapshot change,
/// carrying the rendered transcript fragment. Ends when the agent closes.
async fn agent_events(
    State(reg): State<Arc<Registry>>,
    Path(id): Path<u64>,
) -> Result<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let mut rx = reg.subscribe(id).ok_or(StatusCode::NOT_FOUND)?;
    let stream = async_stream::stream! {
        // Initial frame so a late subscriber renders current state at once.
        let mut last = rx.borrow().clone();
        yield Ok(Event::default().data(shell::transcript_fragment(&last)));
        loop {
            if last.closed {
                break;
            }
            if rx.changed().await.is_err() {
                break;
            }
            last = rx.borrow().clone();
            yield Ok(Event::default().data(shell::transcript_fragment(&last)));
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Where the shared conversation store lives (W4). Env-driven so the
/// deployment points at the box's real state dir; tests point at a tempdir.
fn store_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let state = std::env::var("NEWT_WEB_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".newt")
        });
    let ws = std::env::var("NEWT_WEB_WORKSPACE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    (state, ws)
}

/// The "sessions on this box" section: conversations in the shared store,
/// each followable read-only (W4). Store errors render as an empty section —
/// the cockpit must not die because the store isn't there yet.
pub(crate) async fn sessions_section() -> String {
    let (state, ws) = store_paths();
    let list = tokio::task::spawn_blocking(move || {
        newt_core::ConversationStore::new(&state, &ws, 1000)
            .and_then(|s| s.list())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    if list.is_empty() {
        return String::new();
    }
    let mut out =
        String::from(r#"<details class="sessions"><summary>sessions on this box</summary><ul>"#);
    for c in list.iter().take(20) {
        out.push_str(&format!(
            r##"<li>{title} <small>({n} turns)</small>
<form style="display:inline" hx-post="/follow" hx-target="#panel" hx-swap="innerHTML">
<input type="hidden" name="conv_id" value="{id}"><input type="hidden" name="title" value="{title}">
<button>follow</button></form></li>"##,
            title = shell::escape(&c.title),
            n = c.turn_count,
            id = shell::escape(&c.id),
        ));
    }
    out.push_str("</ul></details>");
    out
}

#[derive(serde::Deserialize)]
struct FollowForm {
    conv_id: String,
    title: String,
}

/// POST /follow — open a read-only store-follow tab (W4).
async fn follow_session(
    State(reg): State<Arc<Registry>>,
    Form(form): Form<FollowForm>,
) -> Html<String> {
    let (state, ws) = store_paths();
    let id = reg.spawn_follow(state, ws, form.conv_id, form.title.clone());
    let panel = shell::agent_panel(
        id,
        &form.title,
        "follow",
        true,
        &agents::Snapshot::default(),
    );
    let strip = shell::tab_strip(&reg.list(), Some(id));
    Html(format!("{panel}\n{strip}"))
}

#[tokio::main]
async fn main() {
    // D3 (LAN-bind posture): bind address comes from NEWT_WEB_BIND, defaulting
    // to loopback — the DEPLOYMENT opts into the LAN bind explicitly
    // (deploy/newt-web-dev/), never the binary by default.
    let bind = std::env::var("NEWT_WEB_BIND").unwrap_or_else(|_| "127.0.0.1:8880".to_string());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|e| panic!("newt-web: cannot bind {bind}: {e}"));
    eprintln!("newt-web listening on http://{bind}");
    axum::serve(listener, app()).await.expect("serve");
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    async fn req(
        app: &Router,
        method: &str,
        path: &str,
        form: Option<&str>,
    ) -> (StatusCode, String) {
        let mut b = axum::http::Request::builder().method(method).uri(path);
        let body = match form {
            Some(f) => {
                b = b.header("content-type", "application/x-www-form-urlencoded");
                axum::body::Body::from(f.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Poll the index until `needle` appears (the pump publishes async).
    async fn wait_for(app: &Router, needle: &str) -> String {
        for _ in 0..100 {
            let (_, body) = req(app, "GET", "/", None).await;
            if body.contains(needle) {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (_, body) = req(app, "GET", "/", None).await;
        panic!("never saw {needle:?}; last body:\n{body}");
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let (status, body) = req(&app(), "GET", "/healthz", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn htmx_asset_is_served() {
        let (status, body) = req(&app(), "GET", "/assets/htmx.min.js", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("htmx"), "vendored htmx served");
    }

    /// The W2 acceptance: spawn → prompt → a full mocked turn lands in the
    /// transcript (wiremock ollama, the TurnDriver test shape) — through the
    /// web seam end to end.
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_prompt_and_the_turn_lands_in_the_transcript() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "mock-llama",
                "message": { "role": "assistant", "content": "REPLY-FROM-MOCK" },
                "done": true,
            })))
            .mount(&mock)
            .await;

        let app = app();
        let form = format!(
            "name=t1&url={}&model=mock-llama&kind=ollama&workspace=.",
            urlencode(&mock.uri())
        );
        let (status, panel) = req(&app, "POST", "/agents", Some(&form)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            panel.contains("agent-1"),
            "panel fragment returned: {panel}"
        );

        let (status, _) = req(&app, "POST", "/agents/1/prompt", Some("text=say+hi")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let body = wait_for(&app, "REPLY-FROM-MOCK").await;
        assert!(body.contains("say hi"), "user prompt rendered");
    }

    /// The SSE bridge serves an event stream whose first frame is the current
    /// transcript fragment.
    #[tokio::test(flavor = "multi_thread")]
    async fn events_stream_opens_with_the_current_fragment() {
        let app = app();
        let form = "name=t2&url=http%3A%2F%2F127.0.0.1%3A1&model=m&kind=ollama&workspace=.";
        let (status, _) = req(&app, "POST", "/agents", Some(form)).await;
        assert_eq!(status, StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/agents/1/events")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let mut body = resp.into_body();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .expect("first SSE frame within 5s")
            .expect("stream not ended")
            .expect("frame ok");
        let text = String::from_utf8_lossy(frame.data_ref().expect("data frame"));
        assert!(text.starts_with("data:"), "SSE frame: {text}");
        assert!(text.contains("No turns yet"), "initial fragment: {text}");
    }

    #[tokio::test]
    async fn delete_removes_the_agent_and_unknown_ids_404() {
        let app = app();
        let form = "name=t3&url=http%3A%2F%2F127.0.0.1%3A1&model=m&kind=ollama&workspace=.";
        req(&app, "POST", "/agents", Some(form)).await;
        let (status, _) = req(&app, "DELETE", "/agents/1", None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = req(&app, "DELETE", "/agents/1", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = req(&app, "POST", "/agents/1/prompt", Some("text=x")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Web hygiene regression: model/user text renders as Markdown but every
    /// XSS vector is sanitized away (render_markdown -> ammonia). The transcript
    /// carries untrusted model output, so a reply must be able to format itself
    /// (bold, code) yet must never smuggle script, an event handler, or a
    /// `javascript:` URL into the cockpit.
    #[tokio::test(flavor = "multi_thread")]
    async fn transcript_renders_markdown_and_sanitizes_xss() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        // One reply mixing a benign sentinel, real Markdown, and three attacks.
        let reply = "SENTINEL **bold** [x](javascript:alert(1)) \
             <img src=x onerror=alert(2)> <script>alert(3)</script>";
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "m",
                "message": { "role": "assistant", "content": reply },
                "done": true,
            })))
            .mount(&mock)
            .await;
        let app = app();
        let form = format!(
            "name=x&url={}&model=m&kind=ollama&workspace=.",
            urlencode(&mock.uri())
        );
        req(&app, "POST", "/agents", Some(&form)).await;
        req(&app, "POST", "/agents/1/prompt", Some("text=go")).await;
        let body = wait_for(&app, "SENTINEL").await;

        // Markdown renders.
        assert!(
            body.contains("<strong>bold</strong>"),
            "Markdown must render (bold): {body}"
        );
        // Every XSS vector is neutralized. (The page carries its own legitimate
        // EventSource <script> hook, so we assert on the ATTACK payload, not the
        // substring "<script": the model's script tag and body are gone.)
        assert!(
            !body.contains("alert(3)"),
            "injected script tag + body must be dropped"
        );
        assert!(
            !body.contains("onerror"),
            "event-handler attr must be stripped"
        );
        assert!(
            !body.contains("javascript:"),
            "javascript: URL must be stripped"
        );
    }

    /// The shell golden — #1319 discipline (missing-fails, double-render
    /// determinism, negative control). The W2 shell adds the spawn form.
    #[serial_test::serial(newt_web_env)]
    #[tokio::test]
    async fn shell_matches_its_golden() {
        // Pin the env-derived form defaults so the golden is machine-independent.
        std::env::remove_var("NEWT_WEB_BACKEND_URL");
        std::env::remove_var("NEWT_WEB_MODEL");
        std::env::remove_var("NEWT_WEB_WORKSPACE");
        std::env::remove_var("NEWT_WEB_STATE_DIR");
        std::env::remove_var("NEWT_WEB_KIND");
        let (status, a) = req(&app(), "GET", "/", None).await;
        assert_eq!(status, StatusCode::OK);
        let (_, b) = req(&app(), "GET", "/", None).await;
        assert_eq!(a, b, "shell render is nondeterministic");

        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/shell.golden");
        if std::env::var("NEWT_GOLDEN_UPDATE").as_deref() == Ok("1") {
            std::fs::write(&path, &a).expect("write golden");
            eprintln!("[golden] UPDATED {}", path.display());
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "golden missing at {} — capture with NEWT_GOLDEN_UPDATE=1 and \
                 commit it (a missing master must never pass)",
                path.display()
            )
        });
        assert_eq!(
            expected, a,
            "shell golden MISMATCH — re-baseline intentionally"
        );
        let perturbed = format!("{a}\nPERTURBED-MUST-FAIL");
        assert_ne!(expected, perturbed, "negative control failed to fail");
    }

    /// W3 acceptance: two agents, two backends, concurrent turns — each
    /// transcript gets ITS OWN reply; deleting one leaves the other driving.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_agents_drive_concurrently_and_die_independently() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let mut mocks = Vec::new();
        for reply in ["REPLY-ALPHA", "REPLY-BETA"] {
            let m = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/chat"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "m",
                    "message": { "role": "assistant", "content": reply },
                    "done": true,
                })))
                .mount(&m)
                .await;
            mocks.push(m);
        }
        let app = app();
        for (i, m) in mocks.iter().enumerate() {
            let form = format!(
                "name=a{i}&url={}&model=m&kind=ollama&workspace=.",
                urlencode(&m.uri())
            );
            let (status, _) = req(&app, "POST", "/agents", Some(&form)).await;
            assert_eq!(status, StatusCode::OK);
        }
        // Prompt both back-to-back — turns run concurrently in their pumps.
        req(&app, "POST", "/agents/1/prompt", Some("text=go")).await;
        req(&app, "POST", "/agents/2/prompt", Some("text=go")).await;
        // Each panel shows ITS reply (and not the sibling's).
        let p1 = wait_for_path(&app, "/agents/1/panel", "REPLY-ALPHA").await;
        assert!(!p1.contains("REPLY-BETA"), "no cross-talk into panel 1");
        let p2 = wait_for_path(&app, "/agents/2/panel", "REPLY-BETA").await;
        assert!(!p2.contains("REPLY-ALPHA"), "no cross-talk into panel 2");
        // Independent lifecycles: kill 1; 2 still drives.
        let (status, _) = req(&app, "DELETE", "/agents/1", None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = req(&app, "GET", "/agents/1/panel", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "dead tab 404s");
        let (status, _) = req(&app, "POST", "/agents/2/prompt", Some("text=again")).await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "survivor still accepts prompts"
        );
    }

    /// The strip lists every agent; the index shows one active panel.
    #[tokio::test(flavor = "multi_thread")]
    async fn tab_strip_lists_agents_and_index_shows_one_panel() {
        let app = app();
        for name in ["one", "two"] {
            let form = format!(
                "name={name}&url=http%3A%2F%2F127.0.0.1%3A1&model=m&kind=ollama&workspace=."
            );
            req(&app, "POST", "/agents", Some(&form)).await;
        }
        let (_, body) = req(&app, "GET", "/", None).await;
        assert!(
            body.contains(">one</button>") && body.contains(">two</button>"),
            "strip lists both"
        );
        assert!(body.contains("agent-1"), "first agent's panel active");
        assert!(
            !body.contains("agent-2\""),
            "second panel not rendered inline"
        );
        // Switching tabs serves the other panel (+ an OOB strip refresh).
        let (status, p2) = req(&app, "GET", "/agents/2/panel", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            p2.contains("agent-2") && p2.contains("hx-swap-oob"),
            "panel + OOB strip"
        );
    }

    /// W4 acceptance (BAT tier — a real store in a tempdir stands in for the
    /// box's shared db): a conversation written by ANOTHER writer becomes a
    /// followable read-only tab whose panel mirrors new turns; prompts are
    /// refused (D2: the running session stays the sole writer).
    #[serial_test::serial(newt_web_env)]
    #[tokio::test(flavor = "multi_thread")]
    async fn store_follow_mirrors_a_conversation_read_only() {
        let state = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::env::set_var("NEWT_WEB_STATE_DIR", state.path());
        std::env::set_var("NEWT_WEB_WORKSPACE", ws.path());

        // Another process's session writes a turn.
        let store = newt_core::ConversationStore::new(state.path(), ws.path(), 100).unwrap();
        let conv = store.create("terminal session", None).unwrap();
        store
            .append_turn(&conv, "hello from the terminal", "hi from the model")
            .unwrap();

        let app = app();
        // The sessions section lists it.
        let (_, body) = req(&app, "GET", "/", None).await;
        assert!(body.contains("terminal session"), "session listed: {body}");
        // Follow it.
        let form = format!("conv_id={conv}&title=terminal+session");
        let (status, panel) = req(&app, "POST", "/follow", Some(&form)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            panel.contains("read-only follow"),
            "readonly badge: {panel}"
        );
        // The mirror catches up with the existing turn...
        let p = wait_for_path(&app, "/agents/1/panel", "hi from the model").await;
        assert!(p.contains("hello from the terminal"));
        // ...and with turns appended AFTER the follow began.
        store
            .append_turn(&conv, "second question", "second answer")
            .unwrap();
        wait_for_path(&app, "/agents/1/panel", "second answer").await;
        // Prompts are refused on a follow (the session owns the claim).
        let (status, _) = req(&app, "POST", "/agents/1/prompt", Some("text=hijack")).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "readonly tab refuses prompts"
        );
        std::env::remove_var("NEWT_WEB_STATE_DIR");
        std::env::remove_var("NEWT_WEB_WORKSPACE");
    }

    /// Regression (#1331 live testing, 2026-07-22): the spawn button "did
    /// nothing" — the form's hx-target pointed at #agents, an element W3 had
    /// replaced with #panel. HTMX silently no-ops on a missing target, and the
    /// route tests never exercise the DOM wiring. This pins the invariant:
    /// every hx-target="#x" in the rendered surface must resolve to an id="x"
    /// present in the same document (the index composed with a live panel).
    #[serial_test::serial(newt_web_env)]
    #[tokio::test(flavor = "multi_thread")]
    async fn every_hx_target_resolves_within_the_rendered_page() {
        std::env::remove_var("NEWT_WEB_STATE_DIR");
        let app = app();
        // Compose the fullest surface: one agent spawned so panel + strip +
        // delete button + prompt form are all present.
        let form = "name=t&url=http%3A%2F%2F127.0.0.1%3A1&model=m&kind=ollama&workspace=.";
        req(&app, "POST", "/agents", Some(form)).await;
        let (_, page) = req(&app, "GET", "/", None).await;

        let mut missing = Vec::new();
        for part in page.split("hx-target=\"#").skip(1) {
            let target = part.split('"').next().unwrap_or_default();
            let id_attr = format!("id=\"{target}\"");
            if !page.contains(&id_attr) {
                missing.push(target.to_string());
            }
        }
        assert!(
            missing.is_empty(),
            "hx-target(s) with no matching id in the page: {missing:?}"
        );
    }

    /// The spawn form must pre-select the configured wire protocol. Without
    /// NEWT_WEB_KIND the dropdown always defaulted to `ollama`, so an operator
    /// whose backend URL points at a vLLM (openai) endpoint would spawn an
    /// agent that speaks the wrong protocol and fails on the first turn. This
    /// pins that NEWT_WEB_KIND=openai renders the openai option `selected`,
    /// while the ollama option is not — and that an unset kind is ollama-first.
    #[serial_test::serial(newt_web_env)]
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_form_preselects_configured_kind() {
        std::env::remove_var("NEWT_WEB_STATE_DIR");
        std::env::set_var("NEWT_WEB_KIND", "openai");
        let (_, page) = req(&app(), "GET", "/", None).await;
        std::env::remove_var("NEWT_WEB_KIND");
        assert!(
            page.contains(r#"<option value="openai" selected>openai</option>"#),
            "openai must be pre-selected when NEWT_WEB_KIND=openai"
        );
        assert!(
            page.contains(r#"<option value="ollama">ollama</option>"#),
            "the non-configured option must not carry `selected`"
        );

        // Unset ⇒ ollama-first (the historical default) stays selected.
        let (_, page) = req(&app(), "GET", "/", None).await;
        assert!(
            page.contains(r#"<option value="ollama" selected>ollama</option>"#),
            "ollama must be the default pre-selection with NEWT_WEB_KIND unset"
        );
    }

    /// Poll an arbitrary GET path until `needle` appears.
    async fn wait_for_path(app: &Router, path: &str, needle: &str) -> String {
        for _ in 0..100 {
            let (_, body) = req(app, "GET", path, None).await;
            if body.contains(needle) {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (_, body) = req(app, "GET", path, None).await;
        panic!("never saw {needle:?} at {path}; last body:\n{body}");
    }

    fn urlencode(s: &str) -> String {
        s.replace(':', "%3A").replace('/', "%2F")
    }
}

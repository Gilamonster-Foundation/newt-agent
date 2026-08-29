//! newt-web — the HTMX web cockpit (#1331, decision record
//! `docs/decisions/newt_web_htmx.md`).
//!
//! W2: spawn-and-drive. Agents are `TurnDriver`s owned by pump tasks
//! (`agents.rs`); the front end is server-rendered HTML (`shell.rs`); this
//! file is the composition root — routes, state, and the SSE bridge only.

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Form, Router};
use std::convert::Infallible;
use std::sync::Arc;

mod agents;
mod dock;
mod shell;

use agents::{Registry, Spec};

/// The trusted forward-auth identity header, from `NEWT_WEB_AUTH_HEADER`
/// (config, three-Cs). Unset/blank (loopback dev + the mocked test tier) ⇒ no
/// gate; set (the deployment pins `X-Auth-Request-Email`, injected by the
/// cluster's oauth2-proxy/Authentik forward-auth) ⇒ every route but `/healthz`
/// demands it (#1355). Header-trust is sound ONLY because the NetworkPolicy
/// (`deploy/newt-web-dev/networkpolicy.yaml`) forces every request through
/// Traefik → oauth2-proxy first, so a direct in-cluster caller cannot forge it.
fn required_auth_header() -> Option<String> {
    normalized_auth_header(std::env::var("NEWT_WEB_AUTH_HEADER").ok())
}

/// Config-parse rule for the trusted identity header: trim, and treat a blank
/// value as unset. Pure, so it is tested WITHOUT mutating process env — a
/// global-env test races the parallel suite (the gate flips other tests' status
/// codes), which is exactly why `app_with_auth` takes the parsed value directly.
fn normalized_auth_header(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Fail-closed identity gate: reject any request whose trusted identity header
/// is absent or blank. Never wraps `/healthz` — the readiness probe carries no
/// oauth2-proxy identity.
async fn require_identity(
    State(header): State<String>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let present = req
        .headers()
        .get(header.as_str())
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.trim().is_empty());
    if present {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

/// The CSRF token this request carries, for re-emission into a fragment's
/// forms. Empty when the browser holds none — the form then renders with an
/// empty field and is refused on submit, which is the fail-closed direction.
fn csrf_of(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(newt_web::csrf::from_cookie_header)
        .unwrap_or_default()
}

/// Whether the caller can consume an HTML fragment.
///
/// HTMX sets `HX-Request` on everything it sends. A plain browser form post
/// does not, and cannot swap a fragment into anything — it must be sent back
/// to a page (POST-Redirect-GET), or the operator is left staring at a
/// fragment as a whole document.
fn is_htmx(headers: &axum::http::HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// POST-Redirect-GET: the scriptless answer to a successful form submission.
///
/// 303 specifically, so the follow-up is a GET regardless of the method that
/// produced it, and a reload cannot resubmit the form.
fn see_other(to: &str) -> axum::response::Response {
    (StatusCode::SEE_OTHER, [("location", to)]).into_response()
}

/// Reject a state-changing browser request that is cross-site or carries no
/// matching CSRF token.
///
/// **Both checks, on every browser POST.** The cockpit sits behind a
/// forward-auth proxy, so a cross-site request arrives ALREADY authenticated —
/// the browser attaches the proxy's cookie whether or not the operator meant
/// to send anything. Authentication answers "who", never "did they ask for
/// this"; that is what this is for.
///
/// Applied only to the browser router. The machine dock API is deliberately
/// outside it: a peer cockpit posts with `ureq`, which sends neither an
/// `Origin` nor a cookie, and its boundary is the forward-auth gate plus the
/// signed approved-dock registry. That exclusion is pinned by
/// `c3b::the_machine_dock_api_is_not_behind_the_browser_gate` so it reads as a
/// decision rather than an oversight.
async fn require_same_origin_and_csrf(
    State(expected_origin): State<Option<String>>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    // Safe methods change nothing, so neither check applies to them.
    if req.method() != axum::http::Method::POST {
        return next.run(req).await;
    }
    let header = |parts: &axum::http::HeaderMap, name: &str| {
        parts
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (parts, body) = req.into_parts();

    if newt_web::origin::check(
        header(&parts.headers, "origin").as_deref(),
        header(&parts.headers, "referer").as_deref(),
        header(&parts.headers, "host").as_deref(),
        expected_origin.as_deref(),
    ) != newt_web::origin::OriginVerdict::SameOrigin
    {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }

    // The token travels in the body, so the body must be read here and put
    // back. Forms are small; the cap is what stops an unbounded read.
    const MAX_FORM: usize = 1 << 20;
    let Ok(bytes) = axum::body::to_bytes(body, MAX_FORM).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "form too large").into_response();
    };
    let submitted = std::str::from_utf8(&bytes)
        .ok()
        .and_then(newt_web::csrf::from_form_body)
        .unwrap_or_default();
    let cookie = header(&parts.headers, "cookie")
        .and_then(|c| newt_web::csrf::from_cookie_header(&c))
        .unwrap_or_default();
    if !newt_web::csrf::matches(&cookie, &submitted) {
        return (StatusCode::FORBIDDEN, "missing or mismatched CSRF token").into_response();
    }

    next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
        .await
}

fn app() -> Router {
    app_with_auth(required_auth_header())
}

/// Compose the cockpit. `auth_header = Some(name)` fences every route except
/// `/healthz` behind that trusted identity header (fail-closed, #1355); `None`
/// leaves the surface open — the loopback-dev + fully-mocked-test posture.
fn app_with_auth(auth_header: Option<String>) -> Router {
    let reg = Arc::new(Registry::default());
    // Resolved ONCE, at composition, not per request — the same reason
    // `normalized_auth_header` is pure: an env read on the hot path is a read
    // that races whatever a parallel test is writing, and #1853's lock covers
    // writers, not unguarded readers. A deployment behind the SSO ingress sets
    // this because the browser's origin is the public HTTPS one and bears no
    // relation to the pod's `Host`; unset falls back to comparing against
    // `Host`, which is what the loopback and LAN binds need.
    let expected_origin = normalized_auth_header(std::env::var("NEWT_WEB_ORIGIN").ok());
    // Static assets: same-origin GETs, no state, and the SRI digests on the
    // page are computed over exactly these bytes.
    let assets = Router::new()
        .route(
            "/assets/htmx.min.js",
            get(|| async { js(newt_web::csp::HTMX_JS) }),
        )
        .route(
            "/assets/mermaid.min.js",
            get(|| async { js(newt_web::csp::MERMAID_JS) }),
        )
        .route(
            "/assets/markdown.js",
            get(|| async { js(newt_web::csp::MARKDOWN_JS) }),
        )
        .route(
            "/assets/panel.js",
            get(|| async { js(newt_web::csp::PANEL_JS) }),
        )
        // Referenced by the enrollment page's SRI-bound tag. It was never
        // routed, so that tag 404'd — one of the things an unrouted page hides.
        .route(
            "/assets/webauthn.js",
            get(|| async { js(newt_web::csp::WEBAUTHN_JS) }),
        );

    // Everything a BROWSER drives. Every POST here must be same-origin and
    // carry the double-submit token.
    let browser = Router::new()
        .route("/", get(shell::index))
        // #1854 step 2: the enrollment page is routed rather than left
        // dangling. It was unrouted, which is precisely why the missing CSP
        // went unnoticed for so long. Its ceremony cannot COMPLETE yet — the
        // `/enroll/finish` staging route needs a store handle that is not
        // wired — and the page says so, fail-closed, when the relying party is
        // unconfigured. Visible and incomplete beats invisible.
        .route("/enroll", get(newt_web::enroll::page))
        .route("/agents", post(spawn_agent))
        .route("/follow", post(follow_session))
        .route("/agents/:id/panel", get(agent_panel_route))
        .route("/agents/:id/prompt", post(prompt_agent))
        .route("/agents/:id/pending", get(pending_decision_route))
        .route("/agents/:id/decision", post(decide_route))
        .route("/agents/:id/events", get(agent_events))
        .route("/agents/:id/delete", post(delete_agent))
        .route("/dock/panel", get(dock_panel_route))
        .route("/dock/inject", post(dock_inject_route))
        .route("/overview", get(overview_route))
        .layer(middleware::from_fn_with_state(
            expected_origin,
            require_same_origin_and_csrf,
        ));

    // The MACHINE dock API. A peer cockpit reaches this with `ureq`, which
    // sends no Origin and holds no cookie, so the browser gate would refuse
    // every legitimate call. Its boundary is the forward-auth gate plus the
    // signed approved-dock registry (`dock::check_dock_approval`), and the
    // operator kill-switch (`dock_exposure_disabled`).
    let machine = Router::new()
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/:id/transcript", get(api_transcript))
        .route("/api/sessions/:id/inject", post(api_inject));

    let mut gated = browser.merge(machine).merge(assets);
    if let Some(header) = auth_header {
        gated = gated.layer(middleware::from_fn_with_state(header, require_identity));
    }
    // `/healthz` stays outside the gate: the kubelet probe has no identity.
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(gated)
        .with_state(reg)
}

/// One served JavaScript asset, with the content type its SRI tag expects.
fn js(body: &'static str) -> impl IntoResponse {
    ([("content-type", "text/javascript")], body)
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
    headers: axum::http::HeaderMap,
    Form(form): Form<SpawnForm>,
) -> axum::response::Response {
    let kind = match form.kind.as_str() {
        "openai" => newt_core::BackendKind::Openai,
        "anthropic" => newt_core::BackendKind::Anthropic,
        _ => newt_core::BackendKind::Ollama,
    };
    let id = reg.spawn(Spec {
        name: form.name.clone(),
        url: form.url,
        model: form.model.clone(),
        kind,
        workspace: form.workspace,
    });
    if !is_htmx(&headers) {
        return see_other(&format!("/?tab={id}"));
    }
    let panel = shell::agent_panel(
        id,
        &form.name,
        &form.model,
        false,
        &agents::Snapshot::default(),
        &csrf_of(&headers),
    );
    let strip = shell::tab_strip(&reg.list(), Some(id));
    Html(format!("{panel}\n{strip}")).into_response()
}

/// GET /agents/:id/panel — the tab body (view attach: opening a tab opens its
/// SSE; the replaced panel's EventSource closes itself when its node vanishes).
async fn agent_panel_route(
    State(reg): State<Arc<Registry>>,
    Path(id): Path<u64>,
    headers: axum::http::HeaderMap,
) -> Result<Html<String>, StatusCode> {
    let agents = reg.list();
    let (aid, name, model, readonly, snap) = agents
        .iter()
        .find(|(aid, ..)| *aid == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let panel = shell::agent_panel(*aid, name, model, *readonly, snap, &csrf_of(&headers));
    let strip = shell::tab_strip(&agents, Some(id));
    Ok(Html(format!("{panel}\n{strip}")))
}

#[derive(serde::Deserialize)]
struct PromptForm {
    text: String,
}

/// POST /agents/:id/prompt — submit a prompt. For a followed (attach) tab this
/// INJECTS into the running session's store inbox (A3/W6) — the web never
/// writes a turn, so the running session stays the sole writer (D2); the mirror
/// shows the result once that session consumes it. For a pump-backed spawned
/// agent it drives the in-process driver. 204 either way (the SSE stream
/// carries the visible effect), 404 for an unknown agent.
async fn prompt_agent(
    State(reg): State<Arc<Registry>>,
    Path(id): Path<u64>,
    headers: axum::http::HeaderMap,
    Form(form): Form<PromptForm>,
) -> axum::response::Response {
    if let Some(attach) = reg.attach_of(id) {
        let (state, _) = store_paths();
        let text = form.text;
        let injected = tokio::task::spawn_blocking(move || {
            newt_core::ConversationStore::new(&state, &attach.workspace, 1000)
                .and_then(|s| s.inject_prompt(&attach.conv_id, &text, None))
                .is_ok()
        })
        .await
        .unwrap_or(false);
        return match (injected, is_htmx(&headers)) {
            (true, true) => StatusCode::NO_CONTENT.into_response(),
            (true, false) => see_other(&format!("/?tab={id}")),
            (false, _) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    match (reg.prompt(id, form.text), is_htmx(&headers)) {
        (true, true) => StatusCode::NO_CONTENT.into_response(),
        (true, false) => see_other(&format!("/?tab={id}")),
        (false, _) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// GET /agents/:id/pending — the pending permission decision for an attach tab
/// (A4/W6), or empty when there is none. The attach panel polls this; when the
/// running session's gate publishes a decision, the card appears with buttons.
async fn pending_decision_route(
    State(reg): State<Arc<Registry>>,
    Path(id): Path<u64>,
    headers: axum::http::HeaderMap,
) -> Html<String> {
    let Some(attach) = reg.attach_of(id) else {
        return Html(String::new());
    };
    let (state, _) = store_paths();
    let conv = attach.conv_id.clone();
    let pending = tokio::task::spawn_blocking(move || {
        newt_core::ConversationStore::new(&state, &attach.workspace, 1000)
            .and_then(|s| s.pending_interaction_offer(&conv))
            .ok()
            .flatten()
    })
    .await
    .ok()
    .flatten();
    Html(match pending {
        Some(p) => shell::pending_permission_card(id, &p, &csrf_of(&headers)),
        None => String::new(),
    })
}

#[derive(serde::Deserialize)]
struct DecisionForm {
    request_id: String,
    verdict: String,
}

/// POST /agents/:id/decision — answer a pending permission decision (A4/W6).
/// The web NAMES a verdict; the running gate mints the caveats (the web never
/// carries authority). A web grant is ephemeral — there is no durable
/// "always-allow" (that is terminal-audit-only). 204 on accept, 404 for a tab
/// that isn't an attach tab.
/// The web decision boundary's typed result, preserved end to end so each case
/// maps to a distinct, truthful HTTP status. Collapsing this to a bool is the
/// bug #1536 fixes: a *losing* web answer (`AlreadyResolved`, or a request that
/// is no longer the live one) must NOT report the 204 that a *winning* answer
/// (`Answered`) does. Reporting 204 would let the browser tell the operator
/// their decision was accepted when the terminal — or another tab — actually
/// won the race, violating the single-winner authority contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecideOutcome {
    /// The request the browser is answering is no longer the live pending one —
    /// the terminal or a prior web answer already resolved it: a lost race.
    NoLiveRequest,
    /// The submitted verdict is not a displayed action of the current question.
    Unparsable,
    /// The store's authoritative verdict for this answer attempt.
    Resolved(newt_core::AnswerOutcome),
}

/// Resolve a submitted web decision against the store, preserving the store's
/// authoritative [`newt_core::AnswerOutcome`]. This is exactly what
/// `decide_route` does over the wire, factored out to take an injected store so
/// the single-winner / stale-answer behavior is unit-testable without the HTTP
/// plumbing. The store's `answer_permission_action` re-validates, inside its own
/// immediate transaction, that the action was actually displayed and that the
/// request is still open — so this is authoritative even under a TOCTOU race
/// with the terminal between the `pending` read and the answer.
fn classify_decision(
    store: &newt_core::ConversationStore,
    conv: &str,
    request_id: &str,
    submitted: &str,
) -> Result<DecideOutcome, ()> {
    // Is the offer this answer names still the live one? A stale card whose
    // offer the terminal already resolved must LOSE here rather than be
    // handed to the store, so the browser is told 409 and not 204 (#1536).
    // The store would refuse it too; this is what makes the refusal legible
    // as a lost race rather than an invalid action.
    if store
        .pending_interaction_offer(conv)
        .map_err(|_| ())?
        .filter(|p| p.instance_id == request_id)
        .is_none()
    {
        return Ok(DecideOutcome::NoLiveRequest);
    }
    // C3c (#1867): name the action, and let the STORE decide whether it was
    // offered.
    //
    // This used to reconstruct a `Question` and call `Question::parse` — a
    // SECOND opinion on a question `answer_interaction_offer` already answers
    // authoritatively. That call runs `interaction_gate::authorized_response`
    // → the one `newt_interaction::validate_response`, with
    // `permission_registry(Audience::Web)`, inside its own immediate
    // transaction and before the CAS. Membership, audience scoping (the web is
    // registered for no durable grant), digest and revision binding, the
    // workspace fence and expiry are all decided there. Removing the web's
    // pre-check deletes a duplicate, not a check.
    //
    // `action_for_option` is the shared wire-name table B0b-1 made public for
    // exactly this ("the interaction gate resolves an accepted option back to
    // the action it authorizes, and a second copy of this table would be the
    // duplication this epic deletes"). It is a lookup, not a parser: there is
    // deliberately no third answer-validation implementation here.
    //
    // NARROWING, stated: `Question::parse` also matched an action's hotkey
    // (`a`) and its aliases — affordances for a terminal, where a keystroke is
    // the input. Every button this surface renders carries the full wire id,
    // so a hotkey could only arrive from something that was not our form.
    // `c3c::the_web_answers_by_wire_id_and_not_by_hotkey` pins that.
    let Some(action) = newt_core::interaction_adapter::action_for_option(submitted) else {
        return Ok(DecideOutcome::Unparsable);
    };
    Ok(DecideOutcome::Resolved(
        store
            .answer_interaction_offer(conv, request_id, action, newt_core::Audience::Web)
            .map_err(|_| ())?,
    ))
}

/// Map each decision outcome to a truthful HTTP status. `Answered` is the ONLY
/// success (204); every non-winning outcome gets its own honest code so the
/// browser never removes the card believing it won a race it lost:
/// `AlreadyResolved` / stale request → 409 ("your decision lost a race with
/// current state"), a non-displayed submission → 400, an unknown/expired
/// request → 404.
fn decision_status(outcome: DecideOutcome) -> StatusCode {
    match outcome {
        DecideOutcome::Resolved(newt_core::AnswerOutcome::Answered) => StatusCode::NO_CONTENT,
        DecideOutcome::Resolved(newt_core::AnswerOutcome::AlreadyResolved)
        | DecideOutcome::NoLiveRequest => StatusCode::CONFLICT,
        DecideOutcome::Resolved(newt_core::AnswerOutcome::InvalidAction)
        | DecideOutcome::Unparsable => StatusCode::BAD_REQUEST,
        DecideOutcome::Resolved(newt_core::AnswerOutcome::Unknown) => StatusCode::NOT_FOUND,
    }
}

async fn decide_route(
    State(reg): State<Arc<Registry>>,
    Path(id): Path<u64>,
    headers: axum::http::HeaderMap,
    Form(form): Form<DecisionForm>,
) -> axum::response::Response {
    let Some(attach) = reg.attach_of(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (state, _) = store_paths();
    let conv = attach.conv_id.clone();
    let request_id = form.request_id;
    let submitted = form.verdict;
    let result = tokio::task::spawn_blocking(move || {
        let store =
            newt_core::ConversationStore::new(&state, &attach.workspace, 1000).map_err(|_| ())?;
        classify_decision(&store, &conv, &request_id, &submitted)
    })
    .await;
    let status = match result {
        Ok(Ok(outcome)) => decision_status(outcome),
        // A join failure or a store/DB error is the only 500 path — every
        // domain outcome resolves to an explicit 2xx/4xx above.
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // A scriptless browser has nowhere to put a 204, so a WINNING answer
    // sends it back to the page. Every losing outcome keeps its own honest
    // code (#1536): redirecting a 409 would tell the operator their decision
    // was accepted when the terminal actually won the race.
    if status == StatusCode::NO_CONTENT && !is_htmx(&headers) {
        return see_other(&format!("/?tab={id}"));
    }
    status.into_response()
}

/// DELETE /agents/:id — shut the agent down; the response clears the panel
/// region and refreshes the strip out-of-band.
async fn delete_agent(
    State(reg): State<Arc<Registry>>,
    Path(id): Path<u64>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    if reg.remove(id) {
        if !is_htmx(&headers) {
            return see_other("/");
        }
        let agents = reg.list();
        let body = format!(
            r#"<p class="empty">Agent closed. Pick a tab or spawn a new one.</p>
{}"#,
            shell::tab_strip(&agents, None)
        );
        (StatusCode::OK, Html(body)).into_response()
    } else {
        (StatusCode::NOT_FOUND, Html(String::new())).into_response()
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

/// The operator's "stop exposing my sessions to any hub" kill-switch
/// (requirement 7 / `newt_web_docking` K5). MVP: a marker file in the state dir
/// that the operator (the TUI `/dock disable`) creates; **fail-closed** — while
/// present, every dock-read surface (`/api/sessions*`) refuses. The signed,
/// root-key + `PromptWindow`-gated, live-terminating version is the Phase-5
/// hardening; the mechanism (the peer refusing to be docked) is proven here.
fn dock_exposure_disabled() -> bool {
    let (state, _) = store_paths();
    state.join("dock-exposure-disabled").exists()
}

/// `GET /api/sessions` — this cockpit's sessions as JSON, the machine-readable
/// twin of `sessions_section`. It is the surface a **hub** reads to dock this
/// instance's sessions (`dock::HttpDockSource`); a hub and a peer speak one wire
/// type ([`dock::DockedSession`]). Behind the same auth gate as the rest — a
/// dock must authenticate. Store errors render an empty list, never a 500.
async fn api_sessions() -> axum::response::Response {
    if dock_exposure_disabled() {
        return (
            StatusCode::FORBIDDEN,
            "dock exposure disabled by the operator",
        )
            .into_response();
    }
    let (state, ws) = store_paths();
    let sessions = tokio::task::spawn_blocking(move || {
        let Ok(store) = newt_core::ConversationStore::new(&state, &ws, 1000) else {
            return Vec::new();
        };
        store
            .list_all()
            .unwrap_or_default()
            .into_iter()
            .take(30)
            .map(|(c, workspace)| {
                let live = store
                    .live_owner(&c.id)
                    .ok()
                    .flatten()
                    .is_some_and(|owner| store.is_owner_live(&owner));
                dock::DockedSession {
                    id: c.id,
                    title: c.title,
                    workspace,
                    turns: c.turn_count,
                    live,
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    axum::Json(sessions).into_response()
}

/// `GET /api/sessions/:id/transcript` — one session's transcript as JSON, the
/// surface a hub reads to MIRROR a docked session (mirror-only, D2). Resolves
/// the conversation's own workspace (store `load` is workspace-fenced) so the
/// caller need not know it. 404 if the conversation is unknown here.
async fn api_transcript(Path(id): Path<String>) -> impl IntoResponse {
    if dock_exposure_disabled() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (state, ws) = store_paths();
    let transcript = tokio::task::spawn_blocking(move || {
        let store = newt_core::ConversationStore::new(&state, &ws, 1000).ok()?;
        let wspath = store
            .list_all()
            .ok()?
            .into_iter()
            .find(|(c, _)| c.id == id)
            .map(|(_, w)| w)?;
        let fenced =
            newt_core::ConversationStore::new(&state, std::path::PathBuf::from(&wspath), 1000)
                .ok()?;
        let rec = fenced.load(&id).ok()?;
        Some(dock::DockedTranscript {
            title: rec.title,
            turns: rec
                .turns
                .iter()
                .map(|t| dock::DockedTurn {
                    user: t.user.clone(),
                    assistant: t.assistant.clone(),
                })
                .collect(),
        })
    })
    .await
    .ok()
    .flatten();
    match transcript {
        Some(t) => axum::Json(t).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(serde::Deserialize)]
struct DockPanelQuery {
    peer: String,
    conv: String,
}

/// `GET /dock/panel?peer=&conv=` — the hub side of SELECT: resolve the clicked
/// peer, mirror its session's transcript into the shared `#panel` read-only. An
/// unknown peer is refused (fail-closed); an unreachable one renders a notice.
async fn dock_panel_route(
    Query(q): Query<DockPanelQuery>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let Some(peer) = dock::peer_by_label(&q.peer) else {
        return (StatusCode::NOT_FOUND, "unknown dock peer").into_response();
    };
    match dock::fetch_transcript(&peer, &q.conv).await {
        Ok(t) => Html(dock::dock_panel(&q.peer, &q.conv, &t, &csrf_of(&headers))).into_response(),
        Err(e) => Html(format!(
            r#"<p class="empty">dock unreachable: {}</p>"#,
            shell::escape(&e)
        ))
        .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct InjectForm {
    text: String,
}

/// `POST /api/sessions/:id/inject` — enqueue a prompt into THIS instance's
/// session (the remote side of a dock inject). It is the exact D2 seam the local
/// attach uses (`ConversationStore::inject_prompt`), exposed over HTTP: the
/// running REPL here stays the sole writer, this only enqueues. Resolves the
/// conversation's own workspace (inject is workspace-fenced). 404 if unknown.
async fn api_inject(Path(id): Path<String>, Form(form): Form<InjectForm>) -> impl IntoResponse {
    if dock_exposure_disabled() {
        return StatusCode::FORBIDDEN;
    }
    let (state, ws) = store_paths();
    let ok = tokio::task::spawn_blocking(move || {
        let store = newt_core::ConversationStore::new(&state, &ws, 1000).ok()?;
        let wspath = store
            .list_all()
            .ok()?
            .into_iter()
            .find(|(c, _)| c.id == id)
            .map(|(_, w)| w)?;
        let fenced =
            newt_core::ConversationStore::new(&state, std::path::PathBuf::from(&wspath), 1000)
                .ok()?;
        fenced.inject_prompt(&id, &form.text, None).ok().map(|_| ())
    })
    .await
    .ok()
    .flatten()
    .is_some();
    if ok {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// `POST /dock/inject?peer=&conv=` — the hub side of inject-over-dock: ask the
/// clicked peer to enqueue a prompt into its session (D2 — the remote host runs
/// it and stays sole writer), then re-mirror the docked panel so the operator
/// sees the enqueue land and the transcript catch up as the remote consumes it.
async fn dock_inject_route(
    Query(q): Query<DockPanelQuery>,
    headers: axum::http::HeaderMap,
    Form(form): Form<InjectForm>,
) -> impl IntoResponse {
    let Some(peer) = dock::peer_by_label(&q.peer) else {
        return (StatusCode::NOT_FOUND, "unknown dock peer").into_response();
    };
    if let Err(e) = dock::peer_inject(&peer, &q.conv, &form.text).await {
        return Html(format!(
            r#"<p class="empty">dock inject failed: {}</p>"#,
            shell::escape(&e)
        ))
        .into_response();
    }
    // Re-mirror: the remote may not have consumed yet; the operator sees the ask
    // land and the transcript catches up on the next select/refresh.
    match dock::fetch_transcript(&peer, &q.conv).await {
        Ok(t) => Html(dock::dock_panel(&q.peer, &q.conv, &t, &csrf_of(&headers))).into_response(),
        Err(e) => Html(format!(r#"<p class="empty">{}</p>"#, shell::escape(&e))).into_response(),
    }
}

/// `GET /overview` — the self-refreshing docked + sessions region (req 3: the
/// web stays coequal with the TUI). The page polls this every few seconds so a
/// terminal-started session, or a docked peer's new turns, appear without an F5.
/// View-only (D2). The open `#panel` is a sibling, so a refresh never disturbs
/// the transcript the operator is reading.
async fn overview_route(headers: axum::http::HeaderMap) -> Html<String> {
    Html(overview_fragment(&csrf_of(&headers)).await)
}

/// The docked + sessions sections, in the page's order.
pub(crate) async fn overview_fragment(csrf: &str) -> String {
    format!(
        "{}{}",
        dock::docked_section(csrf).await,
        sessions_section(csrf).await
    )
}

/// The "sessions on this box" section: conversations in the shared store,
/// each followable read-only (W4). Store errors render as an empty section —
/// the cockpit must not die because the store isn't there yet.
pub(crate) async fn sessions_section(csrf: &str) -> String {
    let (state, ws) = store_paths();
    // list_all spans EVERY workspace (A2) — the operator runs newt in many
    // dirs, so "my sessions" is not one workspace's. Each row carries the
    // workspace path a follow re-opens the store at (load is workspace-fenced).
    let list = tokio::task::spawn_blocking(move || {
        newt_core::ConversationStore::new(&state, &ws, 1000)
            .and_then(|s| s.list_all())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    let mut out = String::from(
        r#"<section class="sessions"><h2>your sessions</h2><p class="hint">Durable conversations in the store — attach from anywhere; the running session stays the writer (D2).</p>"#,
    );
    if list.is_empty() {
        out.push_str(
            r#"<p class="empty">No sessions yet. Start one in a newt shell (SSH), or spawn a scratch agent below.</p></section>"#,
        );
        return out;
    }
    out.push_str("<ul>");
    for (c, workspace) in list.iter().take(30) {
        // The workspace basename orients the operator ("kyln" vs "newt-agent").
        let wsname = std::path::Path::new(workspace)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| workspace.clone());
        out.push_str(&format!(
            r##"<li><span class="s-title">{title}</span> <small>({n} turns · {wsname})</small>
<form class="attach" method="post" action="/follow" hx-post="/follow" hx-target="#panel" hx-swap="innerHTML">
{csrf_field}<input type="hidden" name="conv_id" value="{id}"><input type="hidden" name="title" value="{title}">
<input type="hidden" name="workspace" value="{workspace}">
<button>attach</button></form></li>"##,
            csrf_field = newt_web::csrf::hidden_field(csrf),
            title = shell::escape(&c.title),
            n = c.turn_count,
            wsname = shell::escape(&wsname),
            id = shell::escape(&c.id),
            workspace = shell::escape(workspace),
        ));
    }
    out.push_str("</ul></section>");
    out
}

#[derive(serde::Deserialize)]
struct FollowForm {
    conv_id: String,
    title: String,
    /// The conversation's own workspace path (from list_all): store `load` is
    /// workspace-fenced, so the follow re-opens the store here, not at the
    /// web's default workspace.
    workspace: String,
}

/// POST /follow — open a read-only store-follow tab (W4). The workspace comes
/// from the session row (A2 cross-workspace attach), not the web's default.
async fn follow_session(
    State(reg): State<Arc<Registry>>,
    headers: axum::http::HeaderMap,
    Form(form): Form<FollowForm>,
) -> axum::response::Response {
    let (state, _) = store_paths();
    let id = reg.spawn_follow(
        state,
        std::path::PathBuf::from(&form.workspace),
        form.conv_id,
        form.title.clone(),
    );
    if !is_htmx(&headers) {
        return see_other(&format!("/?tab={id}"));
    }
    let panel = shell::agent_panel(
        id,
        &form.title,
        "follow",
        true,
        &agents::Snapshot::default(),
        &csrf_of(&headers),
    );
    let strip = shell::tab_strip(&reg.list(), Some(id));
    Html(format!("{panel}\n{strip}")).into_response()
}

/// Mint a short-lived agent key for a mesh role under the operator's `UserKey`.
fn mint_agent(
    user: &agent_mesh_core::UserKey,
    role: &str,
    caps: Vec<String>,
) -> agent_mesh_core::AgentKey {
    agent_mesh_core::AgentKey::issue(
        user,
        agent_mesh_core::AgentMetadata {
            role: role.into(),
            host: "newt-web".into(),
            capabilities: caps,
            issued_at: "2026-01-01T00:00:00Z".into(), // a claim; expiry is generation-based
            expires_at: None,
            caveats: agent_mesh_core::Caveats::top(),
        },
    )
}

/// Bring up the agent-mesh dock (Phase 2). Loads the operator `UserKey` from the
/// state dir (the SAME identity the TUI signs under, so a same-operator peer
/// auto-teams); binds a dial `DockClient` so `/dock` can reach mesh peers; and,
/// if `NEWT_WEB_MESH_BIND` is set, binds a `NewtDockService` responder so THIS
/// cockpit's sessions are dockable over the mesh. Returns the responder to keep
/// it alive. Fail-soft: no identity ⇒ mesh dock disabled (HTTP docks still work).
async fn init_mesh_dock() -> Option<newt_mesh::NewtDockService> {
    let (state, _) = store_paths();
    let id_path = state.join("identity.pem");
    let user = match agent_mesh_core::UserKey::load(&id_path) {
        Ok(u) => u,
        Err(why) => {
            eprintln!(
                "newt-web: mesh dock DISABLED — no operator identity at {} ({why})",
                id_path.display()
            );
            return None;
        }
    };
    // Tell the dock gate where the operator config + identity live so it can
    // resolve the signed approved-dock registry (state/ocap/docks.d) before a
    // mesh dial. The gate is fail-closed by default; NEWT_INSECURE_DOCK_NO_APPROVAL
    // is the only (named, unsafe) way off.
    dock::set_dock_identity(state.join("config.toml"), id_path.clone());
    match newt_mesh::DockClient::bind(&user, mint_agent(&user, "newt-web-dock-client", vec![]), 0)
        .await
    {
        Ok(client) => {
            dock::set_dock_client(std::sync::Arc::new(client));
            eprintln!("newt-web: mesh dock dial client bound");
        }
        Err(why) => eprintln!("newt-web: mesh dock client bind failed: {why}"),
    }
    let Ok(port_str) = std::env::var("NEWT_WEB_MESH_BIND") else {
        return None; // not opted in to being dockable over the mesh
    };
    let port: u16 = port_str.trim().parse().unwrap_or(0);
    let agent = mint_agent(
        &user,
        "newt-web-dock",
        vec![newt_mesh::DOCK_CAPABILITY_TAG.to_string()],
    );
    match newt_mesh::NewtDockService::bind(&user, agent, state.clone(), port).await {
        Ok(svc) => {
            eprintln!(
                "newt-web: mesh dock service on udp/{} (agent {}, pubkey {})",
                svc.local_port(),
                svc.agent_fingerprint().short(),
                hex_lower(&svc.agent_pubkey()),
            );
            // The peer-side half of the dock cross-check: print this key's 6-word
            // mnemonic so the operator running `newt dock approve` elsewhere can
            // confirm the SAME words — a fingerprint match in friendly form.
            eprintln!(
                "newt-web: dock key words: {}",
                newt_core::dock_registry::pubkey_words(&svc.agent_pubkey()).join(" ")
            );
            Some(svc)
        }
        Err(why) => {
            eprintln!("newt-web: mesh dock service bind failed: {why}");
            None
        }
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
    // Fail-closed posture, stated at startup so an operator can see whether
    // passkey verification is armed rather than discovering it at answer time.
    match newt_web::webauthn::RelyingParty::from_env() {
        Ok(rp) => eprintln!(
            "newt-web: passkey relying party {} @ {}",
            rp.rp_id(),
            rp.origin()
        ),
        Err(why) => eprintln!("newt-web: passkey verification DISABLED — {why}"),
    }
    // Phase 2: bring up the agent-mesh dock; hold the responder alive for the
    // life of the process (dropping it would tear the bus down).
    let _dock_service = init_mesh_dock().await;
    axum::serve(listener, app()).await.expect("serve");
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    /// A request as the ENHANCED (HTMX) client makes it.
    ///
    /// C3b made the surface work without script, which means a plain form post
    /// now gets a 303 back to the page instead of a fragment. These suites all
    /// assert on fragments, so they are HTMX callers — stated here once, in
    /// the header HTMX itself sends, rather than by threading a flag through
    /// forty call sites. `full` is the helper for everything else.
    /// The fixed token these suites present as both cookie and form field.
    ///
    /// This does NOT weaken the gate: double-submit asks only that the cookie
    /// and the field agree, which is exactly what a real browser produces and
    /// exactly what a cross-site attacker cannot. `c3b::a_post_without_the_csrf_token_is_refused`
    /// drives the other side.
    const TEST_CSRF: &str = "test-csrf-token";

    async fn req(
        app: &Router,
        method: &str,
        path: &str,
        form: Option<&str>,
    ) -> (StatusCode, String) {
        let mut b = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("hx-request", "true")
            // A same-origin browser POST, which is what these suites model.
            .header("origin", "http://127.0.0.1:8880")
            .header("host", "127.0.0.1:8880")
            .header("cookie", format!("newt_csrf={TEST_CSRF}"));
        let post = method.eq_ignore_ascii_case("post");
        let body = match (form, post) {
            (Some(f), _) => {
                b = b.header("content-type", "application/x-www-form-urlencoded");
                axum::body::Body::from(format!("{f}&csrf={TEST_CSRF}"))
            }
            // A bodyless POST still has to carry the token.
            (None, true) => {
                b = b.header("content-type", "application/x-www-form-urlencoded");
                axum::body::Body::from(format!("csrf={TEST_CSRF}"))
            }
            (None, false) => axum::body::Body::empty(),
        };
        let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Like `req` but carries extra request headers — used to simulate the
    /// oauth2-proxy identity header the forward-auth gate trusts.
    async fn req_with_headers(
        app: &Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> StatusCode {
        let mut b = axum::http::Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let resp = app
            .clone()
            .oneshot(b.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        resp.status()
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

    /// #1355: with a trusted identity header configured, every sensitive route
    /// fail-closes on a request that lacks it — this is the app-layer half that
    /// (with the NetworkPolicy) closes the in-cluster bypass of oauth2-proxy.
    /// Uses storeless routes so the gate is proven without any fs/store touch.
    #[tokio::test]
    async fn require_identity_gate_rejects_the_unauthenticated_and_admits_the_header() {
        let app = app_with_auth(Some("X-Auth-Request-Email".into()));
        // No identity → 403, on both a write path and a GET.
        let (status, _) = req(&app, "POST", "/agents/1/prompt", Some("text=hi")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "write path fail-closes");
        let (status, _) = req(&app, "GET", "/assets/htmx.min.js", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "reads are gated too");
        // A blank header value is not an identity.
        let status = req_with_headers(
            &app,
            "GET",
            "/assets/htmx.min.js",
            &[("X-Auth-Request-Email", "  ")],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "blank identity is no identity"
        );
        // The trusted header admits the request — it reaches the handler (a
        // static asset → 200; NOT a 403), proving the gate opened.
        let status = req_with_headers(
            &app,
            "GET",
            "/assets/htmx.min.js",
            &[("X-Auth-Request-Email", "op@example.com")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "trusted identity is admitted");
    }

    /// #1355: `/healthz` must NEVER require identity — the kubelet readiness
    /// probe carries no oauth2-proxy header. (The deployment also moves to an
    /// exec probe, but the route staying public is the invariant.)
    #[tokio::test]
    async fn healthz_stays_public_even_when_identity_is_required() {
        let app = app_with_auth(Some("X-Auth-Request-Email".into()));
        let (status, body) = req(&app, "GET", "/healthz", None).await;
        assert_eq!(status, StatusCode::OK, "the probe must not require auth");
        assert_eq!(body, "ok");
    }

    /// #1355: unconfigured (the loopback-dev + mocked-test default) leaves the
    /// surface open — no identity header is demanded. This pins that the gate
    /// is strictly opt-in, so the existing test suite (which never sets one)
    /// exercises the real, ungated handlers.
    #[tokio::test]
    async fn no_auth_header_configured_leaves_the_surface_open() {
        let app = app_with_auth(None);
        let (status, _) = req(&app, "GET", "/assets/htmx.min.js", None).await;
        assert_eq!(status, StatusCode::OK, "open when unconfigured");
        let (status, _) = req(&app, "GET", "/healthz", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// #1355 config parse: the trusted-header rule trims and treats blank as
    /// unset. Pure — no process-env mutation, so it cannot race the parallel
    /// suite (an earlier env-mutating version flipped other tests' status codes).
    #[test]
    fn normalized_auth_header_trims_and_treats_blank_as_unset() {
        assert_eq!(
            normalized_auth_header(Some("X-Auth-Request-Email".into())).as_deref(),
            Some("X-Auth-Request-Email")
        );
        assert_eq!(
            normalized_auth_header(Some("  X-H  ".into())).as_deref(),
            Some("X-H")
        );
        assert_eq!(normalized_auth_header(Some("   ".into())), None);
        assert_eq!(normalized_auth_header(None), None);
    }

    #[tokio::test]
    async fn htmx_asset_is_served() {
        let (status, body) = req(&app(), "GET", "/assets/htmx.min.js", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("htmx"), "vendored htmx served");
    }

    #[tokio::test]
    async fn markdown_enrichment_assets_are_served_locally() {
        let app = app();
        let (status, mermaid) = req(&app, "GET", "/assets/mermaid.min.js", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(mermaid.contains("mermaid"), "vendored Mermaid served");

        let (status, enhancer) = req(&app, "GET", "/assets/markdown.js", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            enhancer.contains("newtEnhanceMarkdown"),
            "generic Markdown enhancement hook served"
        );
        assert!(
            enhancer.contains(r#"securityLevel: "strict""#),
            "untrusted diagrams must use Mermaid strict mode"
        );
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
        let (status, _) = req(&app, "POST", "/agents/1/delete", None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = req(&app, "POST", "/agents/1/delete", None).await;
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
        std::env::remove_var("NEWT_WEB_KIND");
        // Pin an EMPTY store dir so the sessions section is deterministically
        // the empty-state hint — otherwise list_all would surface whatever
        // conversations happen to live in the dev box's real ~/.newt.
        let empty_state = tempfile::tempdir().unwrap();
        std::env::set_var("NEWT_WEB_STATE_DIR", empty_state.path());
        let (status, a) = req(&app(), "GET", "/", None).await;
        assert_eq!(status, StatusCode::OK);
        let (_, b) = req(&app(), "GET", "/", None).await;

        // C3b: the page now carries two values that MUST differ per response —
        // the CSP nonce and the CSRF token. A byte-identical double render
        // would mean one of them is a reused constant, which is the exact
        // failure `csp.rs` calls unrepresentable. So the determinism check
        // splits in two, and is stronger for it:
        //
        //   1. everything EXCEPT those values is byte-deterministic, and
        //   2. those values are genuinely fresh.
        //
        // Checking only (1) would let a hardcoded nonce through; checking only
        // (2) would stop pinning the page.
        assert_eq!(
            scrub_volatile(&a),
            scrub_volatile(&b),
            "shell render is nondeterministic outside the nonce and CSRF token"
        );
        assert_ne!(
            a, b,
            "the nonce and CSRF token must be fresh per response; an identical \
             render means one of them is a constant"
        );
        let a = scrub_volatile(&a);

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

    /// Replace the two deliberately-fresh values with fixed placeholders, so
    /// the golden pins the page's SHAPE without pinning a secret that must
    /// never repeat. Everything else stays byte-exact.
    fn scrub_volatile(html: &str) -> String {
        let mut out = String::with_capacity(html.len());
        let mut rest = html;
        // Both are `attr="value"` with a value from a fixed alphabet, so a
        // scan to the closing quote is exact — no regex dependency, and no
        // chance of swallowing markup.
        loop {
            let next = ["nonce=\"", "name=\"csrf\" value=\""]
                .iter()
                .filter_map(|marker| rest.find(marker).map(|at| (at, *marker)))
                .min_by_key(|(at, _)| *at);
            let Some((at, marker)) = next else {
                out.push_str(rest);
                return out;
            };
            let after = at + marker.len();
            let Some(close) = rest[after..].find('"') else {
                out.push_str(rest);
                return out;
            };
            out.push_str(&rest[..after]);
            out.push_str(if marker.starts_with("nonce") {
                "{NONCE}"
            } else {
                "{CSRF}"
            });
            rest = &rest[after + close..];
        }
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
        let (status, _) = req(&app, "POST", "/agents/1/delete", None).await;
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
            body.contains(">one</a>") && body.contains(">two</a>"),
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
        // Follow it — the workspace comes from the session row (A2).
        let form = format!(
            "conv_id={conv}&title=terminal+session&workspace={}",
            urlencode(&ws.path().to_string_lossy())
        );
        let (status, panel) = req(&app, "POST", "/follow", Some(&form)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            panel.contains("injects into the running session"),
            "attach tab shows the inject affordance (A3): {panel}"
        );
        // The mirror catches up with the existing turn...
        let p = wait_for_path(&app, "/agents/1/panel", "hi from the model").await;
        assert!(p.contains("hello from the terminal"));
        // ...and with turns appended AFTER the follow began.
        store
            .append_turn(&conv, "second question", "second answer")
            .unwrap();
        wait_for_path(&app, "/agents/1/panel", "second answer").await;
        // A3/W6: a prompt on the attach tab INJECTS into the running session's
        // store inbox — it does NOT drive a local pump and, crucially, does NOT
        // write a turn (D2: the running session stays the sole writer).
        let (status, _) = req(&app, "POST", "/agents/1/prompt", Some("text=please+fix+it")).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "attach prompt is accepted");
        // The running session would dequeue exactly this from its inbox…
        let taken = store
            .take_injected_prompt(&conv)
            .unwrap()
            .expect("the attach prompt was enqueued");
        assert_eq!(taken.body, "please fix it");
        // …and the web wrote NO turn — the transcript is unchanged (D2).
        assert_eq!(
            store.load(&conv).unwrap().turns.len(),
            2,
            "D2: injecting never writes a turn (still the two the session wrote)"
        );
        std::env::remove_var("NEWT_WEB_STATE_DIR");
        std::env::remove_var("NEWT_WEB_WORKSPACE");
    }

    /// A4/W6 web half: an attach tab renders a pending permission decision the
    /// running gate published, danger-gated (a HIGH-danger target offers only
    /// allow-once/deny — never a standing session grant), and answering it
    /// records the verdict the gate then consumes. The web NAMES a verdict; it
    /// never writes caveats.
    #[serial_test::serial(newt_web_env)]
    #[tokio::test(flavor = "multi_thread")]
    async fn attach_tab_renders_and_answers_a_danger_gated_permission_decision() {
        let state = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::env::set_var("NEWT_WEB_STATE_DIR", state.path());
        std::env::set_var("NEWT_WEB_WORKSPACE", ws.path());
        let store = newt_core::ConversationStore::new(state.path(), ws.path(), 100).unwrap();
        let conv = store.create("ssh session", None).unwrap();
        store.append_turn(&conv, "hi", "hello").unwrap();

        let app = app();
        let form = format!(
            "conv_id={conv}&title=s&workspace={}",
            urlencode(&ws.path().to_string_lossy())
        );
        req(&app, "POST", "/follow", Some(&form)).await;

        // Nothing pending yet → empty.
        let (_, empty) = req(&app, "GET", "/agents/1/pending", None).await;
        assert!(
            empty.trim().is_empty(),
            "no card before a request: {empty:?}"
        );

        // The running gate publishes the exact HIGH-danger form to render.
        let question = newt_core::Question {
            markdown: "⊘ run_command wants to run `bash` — needs a shell.".into(),
            actions: vec![
                newt_core::Action::new(newt_core::PermissionAction::AllowOnce, "a", "allow once"),
                newt_core::Action::new(newt_core::PermissionAction::Deny, "d", "deny"),
            ],
            note: Some("High danger: session authorization is unavailable.".into()),
        };
        let definition = newt_core::interaction_adapter::question_to_definition(&question).unwrap();
        let rid = store
            .publish_interaction_offer(
                &conv,
                &definition,
                newt_core::OfferDanger::High,
                newt_core::Audience::Web,
            )
            .unwrap();

        // The card renders — allow-once + deny, but NOT allow-session (high).
        let (_, card) = req(&app, "GET", "/agents/1/pending", None).await;
        assert!(card.contains("Permission needed"), "card: {card}");
        assert!(card.contains("<code>bash</code>"), "target shown: {card}");
        assert!(
            card.contains(r#"name="verdict" value="allow_once""#),
            "allow-once offered: {card}"
        );
        assert!(
            card.contains(r#"name="verdict" value="deny""#),
            "deny offered: {card}"
        );
        assert!(
            !card.contains("allow_session"),
            "high-danger must NOT offer a standing session grant: {card}"
        );

        // The action list is enforcement, not decoration: an HTMX request may
        // not submit a high-danger session grant the rendered form omitted.
        let forged = format!("request_id={rid}&verdict=allow_session");
        let (status, _) = req(&app, "POST", "/agents/1/decision", Some(&forged)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            store.pending_interaction_offer(&conv).unwrap().is_some(),
            "an action absent from the form must leave the decision pending"
        );

        // Answer allow-once → 204, and the gate's poll takes exactly that.
        let dform = format!("request_id={rid}&verdict=allow_once");
        let (status, _) = req(&app, "POST", "/agents/1/decision", Some(&dform)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            store.take_interaction_decision(&conv, &rid).unwrap(),
            Some(newt_core::PermissionAction::AllowOnce),
            "the gate consumes the web verdict"
        );
        std::env::remove_var("NEWT_WEB_STATE_DIR");
        std::env::remove_var("NEWT_WEB_WORKSPACE");
    }

    /// Seed a followed agent (id 1) in `app` with a pending high-danger
    /// allow_once/deny permission form; returns (store, conv, request_id).
    /// The caller owns the tempdirs (they must outlive the returned store) and
    /// the NEWT_WEB_* env, so every caller is `#[serial(newt_web_env)]`.
    async fn seed_followed_pending_decision(
        app: &Router,
        state: &std::path::Path,
        ws: &std::path::Path,
    ) -> (newt_core::ConversationStore, String, String) {
        std::env::set_var("NEWT_WEB_STATE_DIR", state);
        std::env::set_var("NEWT_WEB_WORKSPACE", ws);
        let store = newt_core::ConversationStore::new(state, ws, 100).unwrap();
        let conv = store.create("ssh session", None).unwrap();
        store.append_turn(&conv, "hi", "hello").unwrap();
        let form = format!(
            "conv_id={conv}&title=s&workspace={}",
            urlencode(&ws.to_string_lossy())
        );
        req(app, "POST", "/follow", Some(&form)).await;
        let question = newt_core::Question {
            markdown: "⊘ run_command wants to run `bash`.".into(),
            actions: vec![
                newt_core::Action::new(newt_core::PermissionAction::AllowOnce, "a", "allow once"),
                newt_core::Action::new(newt_core::PermissionAction::Deny, "d", "deny"),
            ],
            note: None,
        };
        let definition = newt_core::interaction_adapter::question_to_definition(&question).unwrap();
        let rid = store
            .publish_interaction_offer(
                &conv,
                &definition,
                newt_core::OfferDanger::High,
                newt_core::Audience::Web,
            )
            .unwrap();
        (store, conv, rid)
    }

    fn clear_web_env() {
        std::env::remove_var("NEWT_WEB_STATE_DIR");
        std::env::remove_var("NEWT_WEB_WORKSPACE");
    }

    /// #1536 P1 — the pure mapping, and the fail-on-old anchor. A *losing*
    /// answer must never be reported as the 204 a *winner* gets. `AlreadyResolved`
    /// — the store's verdict when the terminal or another tab already won — must
    /// map to 409, NOT the 204 the old bool-collapse
    /// (`!matches!(_, InvalidAction | Unknown)`) produced. Every non-winning
    /// outcome gets its own truthful code.
    ///
    /// This is the SOLE direct guard of the `Resolved(AlreadyResolved)` /
    /// `Resolved(InvalidAction)` / `Resolved(Unknown)` arms. The pending
    /// pre-filter (`resolved = 0 AND verdict IS NULL`) means a serialized
    /// already-resolved request never reaches `answer_permission_action` through
    /// `classify_decision` — it short-circuits to `NoLiveRequest` — so those
    /// store arms are reachable at the HTTP boundary only under a genuine TOCTOU
    /// race. The boundary tests below therefore drive the observably-resolved
    /// `NoLiveRequest` gate (also 409); the store actually returning
    /// `AlreadyResolved` is covered by the newt-core store tests, and
    /// `classify_decision` passes that outcome straight through to this mapping.
    #[test]
    fn decision_status_maps_each_outcome_to_a_truthful_status() {
        use newt_core::AnswerOutcome::{AlreadyResolved, Answered, InvalidAction, Unknown};
        assert_eq!(
            decision_status(DecideOutcome::Resolved(Answered)),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            decision_status(DecideOutcome::Resolved(AlreadyResolved)),
            StatusCode::CONFLICT,
            "a losing/stale answer must NOT report the winner's 204"
        );
        assert_eq!(
            decision_status(DecideOutcome::Resolved(InvalidAction)),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            decision_status(DecideOutcome::Resolved(Unknown)),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            decision_status(DecideOutcome::NoLiveRequest),
            StatusCode::CONFLICT
        );
        assert_eq!(
            decision_status(DecideOutcome::Unparsable),
            StatusCode::BAD_REQUEST
        );
    }

    /// #1536 P1 scenario 1 — the terminal resolves the request, THEN a stale
    /// browser submits its now-losing answer. It must get 409 (not 204, and not
    /// the old 400) and record NO new authorization: the terminal's decision
    /// stands untouched.
    #[serial_test::serial(newt_web_env)]
    #[tokio::test(flavor = "multi_thread")]
    async fn stale_web_answer_after_local_resolution_conflicts() {
        let state = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let app = app();
        let (store, conv, rid) =
            seed_followed_pending_decision(&app, state.path(), ws.path()).await;

        // The terminal wins first (CAS 0->1); the browser's card is now stale.
        assert!(
            store.cancel_interaction_offer(&conv, &rid).unwrap(),
            "the terminal resolves the live request"
        );

        let (status, _) = req(
            &app,
            "POST",
            "/agents/1/decision",
            Some(&format!("request_id={rid}&verdict=allow_once")),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a stale web answer that lost to the terminal must 409, not 204"
        );
        assert_eq!(
            store.take_interaction_decision(&conv, &rid).unwrap(),
            None,
            "the losing web answer recorded no authorization"
        );
        clear_web_env();
    }

    /// #1536 P1 scenario 2 — a second web answer after a successful first must
    /// 409, never re-report 204; the first answer's verdict is what stands.
    #[serial_test::serial(newt_web_env)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_web_answer_conflicts_rather_than_succeeding() {
        let state = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let app = app();
        let (store, conv, rid) =
            seed_followed_pending_decision(&app, state.path(), ws.path()).await;

        let (first, _) = req(
            &app,
            "POST",
            "/agents/1/decision",
            Some(&format!("request_id={rid}&verdict=allow_once")),
        )
        .await;
        assert_eq!(first, StatusCode::NO_CONTENT, "the first answer wins");

        let (second, _) = req(
            &app,
            "POST",
            "/agents/1/decision",
            Some(&format!("request_id={rid}&verdict=deny")),
        )
        .await;
        assert_eq!(
            second,
            StatusCode::CONFLICT,
            "the second answer lost the race — 409, not a second 204"
        );
        assert_eq!(
            store.take_interaction_decision(&conv, &rid).unwrap(),
            Some(newt_core::PermissionAction::AllowOnce),
            "exactly the winning verdict is recorded; the loser did not overwrite"
        );
        clear_web_env();
    }

    /// #1536 P1 scenario 3 — a web Allow racing a local Deny records EXACTLY one
    /// verdict, whichever wins. Proven deterministically in BOTH orders (a real
    /// thread race would be flaky): the second mover is always rejected and only
    /// the first mover's decision is recorded.
    #[serial_test::serial(newt_web_env)]
    #[tokio::test(flavor = "multi_thread")]
    async fn web_allow_racing_local_deny_records_exactly_one_verdict() {
        // Order A: the web answer lands first; the terminal then loses the CAS.
        {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;
            let (web, _) = req(
                &app,
                "POST",
                "/agents/1/decision",
                Some(&format!("request_id={rid}&verdict=allow_once")),
            )
            .await;
            assert_eq!(web, StatusCode::NO_CONTENT);
            assert!(
                !store.cancel_interaction_offer(&conv, &rid).unwrap(),
                "the terminal loses: the web answer already holds the verdict"
            );
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                Some(newt_core::PermissionAction::AllowOnce),
                "exactly the web verdict is recorded"
            );
            clear_web_env();
        }
        // Order B: the terminal resolves first; the web answer then loses.
        {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;
            assert!(store.cancel_interaction_offer(&conv, &rid).unwrap());
            let (web, _) = req(
                &app,
                "POST",
                "/agents/1/decision",
                Some(&format!("request_id={rid}&verdict=allow_once")),
            )
            .await;
            assert_eq!(
                web,
                StatusCode::CONFLICT,
                "the web answer lost to the terminal"
            );
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                None,
                "no web verdict recorded: the terminal's decision is the only one"
            );
            clear_web_env();
        }
    }

    /// #1536 P1 scenario 4 — two different web actions racing: the loser receives
    /// a conflict, NEVER a 204, and only the winner's verdict is recorded.
    #[serial_test::serial(newt_web_env)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_losing_web_action_never_reports_204() {
        let state = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let app = app();
        let (store, conv, rid) =
            seed_followed_pending_decision(&app, state.path(), ws.path()).await;

        let (winner, _) = req(
            &app,
            "POST",
            "/agents/1/decision",
            Some(&format!("request_id={rid}&verdict=deny")),
        )
        .await;
        assert_eq!(winner, StatusCode::NO_CONTENT);

        let (loser, _) = req(
            &app,
            "POST",
            "/agents/1/decision",
            Some(&format!("request_id={rid}&verdict=allow_once")),
        )
        .await;
        assert_ne!(
            loser,
            StatusCode::NO_CONTENT,
            "the loser must never report the winner's 204"
        );
        assert_eq!(loser, StatusCode::CONFLICT);
        assert_eq!(
            store.take_interaction_decision(&conv, &rid).unwrap(),
            Some(newt_core::PermissionAction::Deny),
            "only the winning verdict stands"
        );
        clear_web_env();
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

    // ── C3b (#1861, closes #1854) ──────────────────────────────────────────
    //
    // Red-first guards for the semantic no-JS form surface, the CSRF/Origin
    // gate, and the shell page's Content-Security-Policy.

    /// A full request: method, path, headers, optional form body — returning
    /// status, response headers, and body. The existing helpers return only
    /// one of those, and every C3b guard needs at least two.
    async fn full(
        app: &Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        form: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut b = axum::http::Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let body = match form {
            Some(f) => {
                b = b.header("content-type", "application/x-www-form-urlencoded");
                axum::body::Body::from(f.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let hdrs = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, hdrs, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The CSRF token the shell page issued, read from its `set-cookie`.
    fn csrf_of(headers: &axum::http::HeaderMap) -> String {
        headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|c| {
                c.split(';')
                    .next()?
                    .strip_prefix("newt_csrf=")
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    mod c3b {
        use super::*;

        /// **An offer is answerable with NO JavaScript.**
        ///
        /// The load-bearing test of this slice, and the one the issue says
        /// must not be vacuous: it never sends `HX-Request`, never parses a
        /// fragment, and never runs a script. It does what a browser with
        /// scripting disabled does — GET the page, read the form out of the
        /// HTML, and POST it — and requires that the offer is actually
        /// resolved in the store afterwards.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn an_offer_is_answerable_with_no_javascript() {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;

            // 1. GET the page the way a scriptless browser does.
            let (status, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            assert_eq!(status, StatusCode::OK);
            let token = csrf_of(&page_headers);
            assert!(!token.is_empty(), "the page must issue a CSRF token");

            // 2. Read the offer's form straight out of the HTML — no HTMX,
            //    no JSON, no hx-vals. A scriptless client can only submit
            //    what the markup declares.
            let (_, _, card) = full(&app, "GET", "/agents/1/pending", &[], None).await;
            assert!(
                card.contains(r#"method="post""#)
                    && card.contains(r#"action="/agents/1/decision""#),
                "the offer must be a real form a browser can submit unaided: {card}"
            );
            assert!(
                card.contains(r#"name="verdict""#) && card.contains(r#"value="allow_once""#),
                "the action must be a submit button's value, not a script payload: {card}"
            );

            // 3. Submit it as a browser would: form encoding, an Origin, the
            //    cookie the page set — and NO HX-Request header.
            let (status, _, _) = full(
                &app,
                "POST",
                "/agents/1/decision",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", &format!("newt_csrf={token}")),
                ],
                Some(&format!("csrf={token}&request_id={rid}&verdict=allow_once")),
            )
            .await;

            // 4. A scriptless client cannot swap a fragment, so the answer
            //    must land it back on a page: POST-Redirect-GET.
            assert_eq!(
                status,
                StatusCode::SEE_OTHER,
                "a non-HTMX form post must redirect, not return a fragment"
            );

            // 5. And it must have actually resolved the offer.
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                Some(newt_core::PermissionAction::AllowOnce),
                "the no-JS answer did not reach the store"
            );
            clear_web_env();
        }

        /// **A state-changing POST without the CSRF token is refused.**
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn a_post_without_the_csrf_token_is_refused() {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;

            let (status, _, _) = full(
                &app,
                "POST",
                "/agents/1/decision",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                ],
                Some(&format!("request_id={rid}&verdict=allow_once")),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "no token must fail closed");
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                None,
                "a CSRF-refused request must record nothing"
            );

            // A token that does not match the cookie is equally refused —
            // otherwise "present" would be the whole check.
            let (status, _, _) = full(
                &app,
                "POST",
                "/agents/1/decision",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", "newt_csrf=the-real-one"),
                ],
                Some(&format!(
                    "csrf=a-different-one&request_id={rid}&verdict=allow_once"
                )),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "mismatched token must fail closed"
            );
            clear_web_env();
        }

        /// **Anti-vacuous twin:** the same request WITH a matching token is
        /// accepted, so the guard above is measuring the token and not some
        /// unrelated refusal.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn the_same_post_with_a_matching_csrf_token_is_accepted() {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);

            let (status, _, _) = full(
                &app,
                "POST",
                "/agents/1/decision",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", &format!("newt_csrf={token}")),
                    ("hx-request", "true"),
                ],
                Some(&format!("csrf={token}&request_id={rid}&verdict=allow_once")),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NO_CONTENT,
                "a valid token must be accepted"
            );
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                Some(newt_core::PermissionAction::AllowOnce)
            );
            clear_web_env();
        }

        /// **A cross-site Origin is refused**, even carrying a valid token —
        /// the two checks are independent layers, not one check spelled twice.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn a_foreign_origin_is_refused() {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);

            for hostile in ["https://evil.test", "http://127.0.0.1:9999", "null"] {
                let (status, _, _) = full(
                    &app,
                    "POST",
                    "/agents/1/decision",
                    &[
                        ("origin", hostile),
                        ("host", "127.0.0.1:8880"),
                        ("cookie", &format!("newt_csrf={token}")),
                    ],
                    Some(&format!("csrf={token}&request_id={rid}&verdict=allow_once")),
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "Origin {hostile:?} must be refused even with a valid token"
                );
            }
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                None,
                "no cross-site request may record a verdict"
            );
            clear_web_env();
        }

        /// **The shell response carries a Content-Security-Policy** — #1854.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test]
        async fn the_shell_response_carries_a_content_security_policy() {
            std::env::remove_var("NEWT_WEB_STATE_DIR");
            let (status, headers, _) = full(&app(), "GET", "/", &[], None).await;
            assert_eq!(status, StatusCode::OK);
            let csp = headers
                .get("content-security-policy")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(
                !csp.is_empty(),
                "the page that renders untrusted model text must carry a CSP"
            );
            assert!(csp.contains("default-src 'none'"), "CSP: {csp}");
            assert!(csp.contains("script-src 'nonce-"), "CSP: {csp}");
            for (name, want) in [
                ("x-content-type-options", "nosniff"),
                ("x-frame-options", "DENY"),
                ("referrer-policy", "no-referrer"),
            ] {
                assert_eq!(
                    headers.get(name).and_then(|v| v.to_str().ok()),
                    Some(want),
                    "hardening header {name} missing"
                );
            }
        }

        /// **`'unsafe-inline'` appears in exactly one directive, and it is
        /// the one where it grants nothing.**
        ///
        /// A blanket "the string does not appear" check is the wrong guard in
        /// both directions: it fails on the measured, defensible
        /// `style-src-attr` relaxation, and — worse — a future author who
        /// needed it would delete the check rather than narrow it, and then
        /// nothing would stop `script-src 'unsafe-inline'`. So this asserts
        /// PER DIRECTIVE.
        ///
        /// Why `style-src-attr` is the exception, measured rather than
        /// assumed (see `csp::policy`): Mermaid styles its generated SVG
        /// almost entirely through per-node `style=` attributes — 49 blocked
        /// attribute applications against 4 blocked `<style>` elements on a
        /// real page — and ammonia's default allowlist has no `style`
        /// attribute on any tag, so untrusted content cannot emit one. The
        /// permission is unreachable by an attacker.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test]
        async fn the_policy_never_permits_unsafe_inline() {
            std::env::remove_var("NEWT_WEB_STATE_DIR");
            let (_, headers, _) = full(&app(), "GET", "/", &[], None).await;
            let csp = headers
                .get("content-security-policy")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            // Non-vacuity first. Every check below is a `!contains`, which an
            // ABSENT header satisfies perfectly — so without this line the
            // guard reports "no unsafe-inline" about a page that has no
            // policy at all, which is the exact false green #1854 warns of.
            assert!(
                csp.contains("script-src"),
                "no policy to check — this guard would pass vacuously"
            );

            let directive = |name: &str| {
                csp.split(';')
                    .map(str::trim)
                    .find(|d| d.split_whitespace().next() == Some(name))
                    .unwrap_or("")
                    .to_string()
            };

            // Script may never be relaxed, by any keyword.
            let script = directive("script-src");
            for forbidden in [
                "'unsafe-inline'",
                "'unsafe-eval'",
                "'unsafe-hashes'",
                "'strict-dynamic'",
                "*",
                "http:",
                "https:",
            ] {
                assert!(
                    !script.contains(forbidden),
                    "script-src must not contain {forbidden}: {script}"
                );
            }
            // Nor may a style ELEMENT or the fallback, which is where style
            // injection actually has teeth.
            for name in ["style-src", "style-src-elem", "default-src"] {
                let d = directive(name);
                assert!(
                    !d.contains("'unsafe-inline'"),
                    "{name} must stay strict: {d}"
                );
            }
            assert!(
                directive("default-src").contains("'none'"),
                "an unnamed fetch directive must fail closed: {csp}"
            );

            // …and the ONE relaxation is exactly where it was measured to be
            // needed and proven unreachable. Counting occurrences is what
            // stops a second one being added quietly.
            assert_eq!(
                csp.matches("'unsafe-inline'").count(),
                1,
                "exactly one directive may carry it: {csp}"
            );
            assert!(
                directive("style-src-attr").contains("'unsafe-inline'"),
                "…and it must be style-src-attr: {csp}"
            );
        }

        /// **No `'unsafe-eval'`, and nothing on the page needs it.**
        ///
        /// Regression for a real measured violation: the prompt form carried
        /// `hx-on::after-request="this.reset()"`, and htmx EVALUATES that
        /// attribute's value as JavaScript — inline script wearing an
        /// attribute, which needs `script-src 'unsafe-eval'`. It was the only
        /// `script-src :: eval` violation on the page. The behaviour moved to
        /// `assets/panel.js`, which needs neither eval nor a nonce.
        ///
        /// `hx-on:` is banned outright rather than just this one instance:
        /// every spelling of it is an evaluated string, so a new one would
        /// silently require the same relaxation.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn no_markup_requires_eval() {
            std::env::remove_var("NEWT_WEB_STATE_DIR");
            let app = app();
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);
            full(
                &app,
                "POST",
                "/agents",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", &format!("newt_csrf={token}")),
                    ("hx-request", "true"),
                ],
                Some(&format!(
                    "name=t&url=http%3A%2F%2F127.0.0.1%3A1&model=m&kind=ollama&workspace=.&csrf={token}"
                )),
            )
            .await;
            for path in ["/", "/agents/1/panel", "/overview"] {
                let (_, _, body) = full(&app, "GET", path, &[], None).await;
                assert!(
                    !body.contains("hx-on:"),
                    "{path} carries an htmx-evaluated attribute, which needs \
                     'unsafe-eval': {body}"
                );
            }
        }

        /// **No fragment carries inline script.**
        ///
        /// This is the structural fact that MAKES the CSP correct, and the
        /// reason #1854 could not be closed inside #1848. A fragment is
        /// swapped into a page whose CSP header came from an earlier
        /// response, so its inline script would need that page's nonce —
        /// a nonce outliving its response, which `csp.rs` rightly refuses
        /// to allow. With no inline script in any fragment, the question
        /// does not arise.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn no_fragment_carries_inline_script() {
            std::env::remove_var("NEWT_WEB_STATE_DIR");
            let app = app();
            let form = "name=t&url=http%3A%2F%2F127.0.0.1%3A1&model=m&kind=ollama&workspace=.";
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);
            full(
                &app,
                "POST",
                "/agents",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", &format!("newt_csrf={token}")),
                    ("hx-request", "true"),
                ],
                Some(&format!("{form}&csrf={token}")),
            )
            .await;

            for path in ["/agents/1/panel", "/overview", "/agents/1/pending"] {
                let (_, _, body) = full(&app, "GET", path, &[], None).await;
                assert!(
                    !body.contains("<script"),
                    "fragment {path} carries inline script, which cannot be nonced: {body}"
                );
                // An inline style ATTRIBUTE is governed by style-src too, and
                // is blocked just as an inline <style> element is.
                assert!(
                    !body.contains(" style=\""),
                    "fragment {path} carries an inline style attribute, which the CSP blocks: {body}"
                );
            }
        }

        /// **Every control is reachable without script.**
        ///
        /// Each state-changing control on the page is a real `<form>` with a
        /// `method` and an `action`, so a scriptless browser can submit it.
        /// Before C3b every one of them was an `hx-post`/`hx-delete`
        /// attribute with no `action` at all — inert without HTMX.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn every_control_is_reachable_without_script() {
            std::env::remove_var("NEWT_WEB_STATE_DIR");
            let app = app();
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);
            full(
                &app,
                "POST",
                "/agents",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", &format!("newt_csrf={token}")),
                    ("hx-request", "true"),
                ],
                Some(&format!(
                    "name=t&url=http%3A%2F%2F127.0.0.1%3A1&model=m&kind=ollama&workspace=.&csrf={token}"
                )),
            )
            .await;
            let (_, _, page) = full(&app, "GET", "/", &[], None).await;

            // Every <form> declares how to submit itself.
            let mut bad = Vec::new();
            for frag in page.split("<form").skip(1) {
                let tag = frag.split('>').next().unwrap_or_default();
                if !tag.contains("method=\"post\"") || !tag.contains("action=\"") {
                    bad.push(tag.to_string());
                }
            }
            assert!(
                bad.is_empty(),
                "forms with no no-JS submission path: {bad:?}"
            );

            // …and every form carries the CSRF field, or it would be refused
            // the moment a scriptless browser submitted it.
            for frag in page.split("<form").skip(1) {
                let body = frag.split("</form>").next().unwrap_or_default();
                assert!(
                    body.contains(r#"name="csrf""#),
                    "a form with no CSRF field cannot be submitted: {body}"
                );
            }
        }

        /// **Labels are bound to their controls, and choices are grouped.**
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn controls_carry_accessible_semantics() {
            std::env::remove_var("NEWT_WEB_STATE_DIR");
            let app = app();
            // The live region belongs to a transcript, so the page must have
            // one — asserting it on the empty shell would pass or fail for
            // reasons that have nothing to do with accessibility.
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);
            full(
                &app,
                "POST",
                "/agents",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", &format!("newt_csrf={token}")),
                    ("hx-request", "true"),
                ],
                Some(&format!(
                    "name=t&url=http%3A%2F%2F127.0.0.1%3A1&model=m&kind=ollama&workspace=.&csrf={token}"
                )),
            )
            .await;
            let (_, _, page) = full(&app, "GET", "/", &[], None).await;
            assert!(
                page.contains("<fieldset") && page.contains("<legend"),
                "grouped controls need a fieldset/legend: {page}"
            );
            // Every label points at an id that exists on the page.
            let mut dangling = Vec::new();
            for frag in page.split("<label for=\"").skip(1) {
                let target = frag.split('"').next().unwrap_or_default();
                if !page.contains(&format!("id=\"{target}\"")) {
                    dangling.push(target.to_string());
                }
            }
            assert!(
                dangling.is_empty(),
                "labels pointing at nothing: {dangling:?}"
            );
            assert!(
                page.contains("aria-live"),
                "the transcript changes under the reader; it needs a live region"
            );
            assert!(
                page.contains("prefers-reduced-motion"),
                "motion must be opt-out"
            );
            assert!(page.contains(":focus-visible"), "focus must be visible");
        }

        /// **A losing no-JS answer is NOT redirected as if it had won.**
        ///
        /// The trap this exists for: the scriptless path needs
        /// POST-Redirect-GET because a browser with no script cannot consume a
        /// fragment — but if that redirect is applied to EVERY outcome rather
        /// than only success, an operator whose answer lost the race to the
        /// terminal is sent to a normal-looking page and told, in the only
        /// language the surface has, that they won.
        ///
        /// That is a silent violation of #1536's single-winner contract,
        /// which exists precisely so a losing answer can never report success.
        /// It survives review because the happy path looks right: the winning
        /// case redirects correctly, and nothing about the code says the
        /// branch is outcome-sensitive.
        ///
        /// **Do not "simplify" `decide_route` by redirecting unconditionally.**
        /// The twin below is what proves this test is measuring the loss and
        /// not merely "no-JS never redirects".
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn a_losing_no_js_answer_is_not_redirected_as_success() {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);

            // The TERMINAL wins first (the same CAS the #1536 race tests
            // drive). The browser's card is now stale.
            assert!(
                store.cancel_interaction_offer(&conv, &rid).unwrap(),
                "the terminal resolves the live offer"
            );

            // A scriptless browser submits its now-losing answer: same origin,
            // valid token, and crucially NO HX-Request header.
            let (status, headers, _) = full(
                &app,
                "POST",
                "/agents/1/decision",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", &format!("newt_csrf={token}")),
                ],
                Some(&format!("csrf={token}&request_id={rid}&verdict=allow_once")),
            )
            .await;

            assert_eq!(
                status,
                StatusCode::CONFLICT,
                "a losing answer must keep its honest 409, even with no script"
            );
            assert!(
                !status.is_redirection(),
                "a losing answer must NEVER be redirected: {status}"
            );
            assert!(
                headers.get("location").is_none(),
                "a losing answer must carry no Location — that is what would \
                 tell a scriptless operator they won"
            );
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                None,
                "the losing answer recorded no authorization"
            );
            clear_web_env();
        }

        /// **Anti-vacuous twin for the test above.**
        ///
        /// The guard above is satisfied by a handler that never redirects at
        /// all — which would break the scriptless path entirely while looking
        /// green. This proves the redirect branch EXISTS and fires, so the
        /// absence of a redirect in the losing case is a decision rather than
        /// an omission. The two together pin the rule: redirect success, and
        /// only success.
        ///
        /// Every non-success outcome reachable at this boundary is checked,
        /// not just the one the sibling drives.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn a_winning_no_js_answer_is_redirected_but_no_other_outcome_is() {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);
            let browser = |body: String| {
                let token = token.clone();
                let app = app.clone();
                async move {
                    full(
                        &app,
                        "POST",
                        "/agents/1/decision",
                        &[
                            ("origin", "http://127.0.0.1:8880"),
                            ("host", "127.0.0.1:8880"),
                            ("cookie", &format!("newt_csrf={token}")),
                        ],
                        Some(&body),
                    )
                    .await
                }
            };

            // An action that was never displayed: refused, and NOT redirected.
            let (status, headers, _) = browser(format!(
                "csrf={token}&request_id={rid}&verdict=allow_session"
            ))
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "a non-offered action must be refused server-side"
            );
            assert!(headers.get("location").is_none(), "and not redirected");

            // An unknown request id: refused, and NOT redirected.
            let (status, headers, _) = browser(format!(
                "csrf={token}&request_id=not-a-real-id&verdict=deny"
            ))
            .await;
            assert!(!status.is_success() && !status.is_redirection(), "{status}");
            assert!(headers.get("location").is_none(), "and not redirected");

            // …and the WINNING answer IS redirected, which is what makes the
            // assertions above mean something.
            let (status, headers, _) =
                browser(format!("csrf={token}&request_id={rid}&verdict=allow_once")).await;
            assert_eq!(
                status,
                StatusCode::SEE_OTHER,
                "the success path must redirect, or the scriptless client is stranded"
            );
            assert!(
                headers.get("location").is_some(),
                "a redirect with no Location is not a redirect"
            );
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                Some(newt_core::PermissionAction::AllowOnce),
                "exactly the winning verdict is recorded"
            );
            clear_web_env();
        }

        /// **The machine dock API is deliberately NOT behind the browser
        /// CSRF/Origin gate**, and this pins that as a decision rather than
        /// an oversight. A peer cockpit posts with `ureq`, which sends
        /// neither an `Origin` nor a cookie; gating it would break docking
        /// silently. Its boundary is the forward-auth gate plus the signed
        /// approved-dock registry, not a browser token.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn the_machine_dock_api_is_not_behind_the_browser_gate() {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            std::env::set_var("NEWT_WEB_STATE_DIR", state.path());
            std::env::set_var("NEWT_WEB_WORKSPACE", ws.path());
            let store = newt_core::ConversationStore::new(state.path(), ws.path(), 100).unwrap();
            let conv = store.create("peer session", None).unwrap();
            store.append_turn(&conv, "hi", "hello").unwrap();

            let (status, _, _) = full(
                &app(),
                "POST",
                &format!("/api/sessions/{conv}/inject"),
                &[],
                Some("text=from+a+docked+hub"),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NO_CONTENT,
                "a dock inject carries no Origin and no cookie by design"
            );
            clear_web_env();
        }
    }

    /// **C3c (#1867): the decode path, with the duplicate membership
    /// test deleted.**
    mod c3c {
        use super::*;

        /// **An unoffered action is refused by the STORE, not by a web
        /// pre-check.**
        ///
        /// C3c deletes the web's own membership test. That is a deletion of a
        /// DUPLICATE, not of a check: `answer_interaction_offer` runs
        /// `interaction_gate::authorized_response` — the one
        /// `newt_interaction::validate_response` — against the offer as
        /// PUBLISHED, inside its own immediate transaction, before the CAS.
        /// The web's `Question::parse` was a second opinion on a question the
        /// store already answers authoritatively.
        ///
        /// This pins that removing it changed nothing an attacker can reach:
        /// a hand-crafted POST naming a high-danger session grant is still
        /// refused, still with 400, and the offer is still pending afterwards.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn an_unoffered_action_is_refused_by_the_store_not_a_web_precheck() {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);

            // `allow_session` is a real PermissionAction wire name, so it
            // survives the shared action table — the store is what refuses it,
            // because a HIGH-danger offer never registered it for this
            // audience.
            let (status, _, _) = full(
                &app,
                "POST",
                "/agents/1/decision",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", &format!("newt_csrf={token}")),
                    ("hx-request", "true"),
                ],
                Some(&format!(
                    "csrf={token}&request_id={rid}&verdict=allow_session"
                )),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "an action the offer never published must be refused"
            );
            assert!(
                store.pending_interaction_offer(&conv).unwrap().is_some(),
                "a refused action must leave the offer PENDING — otherwise the \
                 refusal consumed the operator's decision"
            );
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                None,
                "and it must record no authorization"
            );

            // Anti-vacuous: the same route DOES accept an offered action, so
            // the refusal above is about membership and not about the route
            // being broken.
            let (status, _, _) = full(
                &app,
                "POST",
                "/agents/1/decision",
                &[
                    ("origin", "http://127.0.0.1:8880"),
                    ("host", "127.0.0.1:8880"),
                    ("cookie", &format!("newt_csrf={token}")),
                    ("hx-request", "true"),
                ],
                Some(&format!("csrf={token}&request_id={rid}&verdict=allow_once")),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NO_CONTENT,
                "an offered action is accepted"
            );
            assert_eq!(
                store.take_interaction_decision(&conv, &rid).unwrap(),
                Some(newt_core::PermissionAction::AllowOnce)
            );
            clear_web_env();
        }

        /// **The web answers with the wire ids it rendered, and nothing else.**
        ///
        /// A deliberate, stated NARROWING. `Question::parse` also matched an
        /// action's single-character HOTKEY (`a`, `d`) and its aliases —
        /// presentation affordances for a terminal, where a keystroke is the
        /// input. A browser has no keystroke to submit: every button this
        /// surface renders carries the option's full wire id as its value, so
        /// a hotkey could only ever arrive from something that was not our
        /// form.
        ///
        /// Accepting only what was rendered is the tighter contract, and it is
        /// the one the store enforces anyway.
        #[serial_test::serial(newt_web_env)]
        #[tokio::test(flavor = "multi_thread")]
        async fn the_web_answers_by_wire_id_and_not_by_hotkey() {
            let state = tempfile::tempdir().unwrap();
            let ws = tempfile::tempdir().unwrap();
            let app = app();
            let (store, conv, rid) =
                seed_followed_pending_decision(&app, state.path(), ws.path()).await;
            let (_, page_headers, _) = full(&app, "GET", "/", &[], None).await;
            let token = csrf_of(&page_headers);
            let answer = |verdict: &str| {
                let (token, app) = (token.clone(), app.clone());
                let body = format!("csrf={token}&request_id={rid}&verdict={verdict}");
                async move {
                    full(
                        &app,
                        "POST",
                        "/agents/1/decision",
                        &[
                            ("origin", "http://127.0.0.1:8880"),
                            ("host", "127.0.0.1:8880"),
                            ("cookie", &format!("newt_csrf={token}")),
                            ("hx-request", "true"),
                        ],
                        Some(&body),
                    )
                    .await
                    .0
                }
            };

            // The hotkey the terminal would accept is not an answer here.
            assert_eq!(answer("a").await, StatusCode::BAD_REQUEST, "hotkey refused");
            assert_eq!(answer("").await, StatusCode::BAD_REQUEST, "empty refused");
            assert_eq!(
                answer("banana").await,
                StatusCode::BAD_REQUEST,
                "nonsense refused"
            );
            assert!(
                store.pending_interaction_offer(&conv).unwrap().is_some(),
                "none of those may consume the offer"
            );
            // The wire id it rendered IS.
            assert_eq!(answer("allow_once").await, StatusCode::NO_CONTENT);
            clear_web_env();
        }
    }
}

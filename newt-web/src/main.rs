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
#[path = "main_tests.rs"]
mod tests;

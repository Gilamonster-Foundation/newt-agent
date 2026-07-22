//! newt-web — the HTMX web cockpit (#1331, decision record
//! `docs/decisions/newt_web_htmx.md`).
//!
//! W1 scaffold: the server shell. Tabs/agents arrive in W2+; this rung is the
//! bindable surface + its characterization golden, so every later rung lands
//! against a pinned baseline. Composition only: newt-web owns no agent logic —
//! agents are driven through `newt_core::TurnDriver` (W2) and followed through
//! the shared `ConversationStore` (W4).

use axum::routing::get;
use axum::Router;

mod shell;

fn app() -> Router {
    Router::new()
        .route("/", get(shell::index))
        .route("/healthz", get(|| async { "ok" }))
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

    async fn get_body(path: &str) -> (axum::http::StatusCode, String) {
        let resp = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let (status, body) = get_body("/healthz").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body, "ok");
    }

    /// The shell golden — the #1319 characterization discipline applied to the
    /// web surface from birth: a MISSING golden fails (never silently passes),
    /// the render must agree with itself (double-render determinism), and the
    /// negative control proves the comparator rejects a perturbed expectation.
    /// Update intentionally: NEWT_GOLDEN_UPDATE=1 cargo test, commit the file.
    #[tokio::test]
    async fn shell_matches_its_golden() {
        let (status, a) = get_body("/").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let (_, b) = get_body("/").await;
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
        // Negative control: the comparator must reject a perturbation.
        let perturbed = format!("{a}\nPERTURBED-MUST-FAIL");
        assert_ne!(expected, perturbed, "negative control failed to fail");
    }
}

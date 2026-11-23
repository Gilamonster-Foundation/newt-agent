//! Integration tests for the `web_fetch` net leash, against a localhost mock
//! server (so CI needs no external network).
//!
//! These exercise the headline guarantees of DESIGN §7 end-to-end through the
//! real fetch path and a real (loopback) HTTP server:
//!
//! - a fetch to the mock SUCCEEDS and returns extracted markdown **only** when
//!   the loopback host is reachable (`net`) AND opted into private space via the
//!   separate `net_private` ceiling (AB-007, #270);
//! - the **same** fetch is DENIED when the grant is `net: Only{example.com}`
//!   (loopback neither permitted nor opted in) — proving both the host
//!   allowlist *and* the SSRF block;
//! - naming the loopback host in `net` alone (the public `fetch`) does NOT open
//!   private space — the SSRF screen still denies it (the AB-007 regression);
//! - a 302 redirect to a disallowed host is DENIED (the redirect target is
//!   re-screened, never blindly followed).
//!
//! These five are agent-bridle-tool-web 0.8.0-rc.7's `tests/net_leash.rs`, moved from
//! `httpmock` to newt-core's `wiremock` and from the `Tool` trait to `fetch` and
//! `fetch_inner`; their assertions are bridle's. The tests after them are newt's: the size
//! and hop limits bridle's suite does not reach, and the user agent.

use super::{fetch, fetch_inner, WebFetchError};
use agent_mesh_protocol::{Caveats, CountBound, Scope};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A grant that makes the loopback host **reachable** (`net`) with a small call
/// budget. Reachability alone no longer opens private space (AB-007, #270) —
/// that needs the separate `net_private` opt-in ([`loopback_private`]).
fn loopback_grant() -> Caveats {
    Caveats {
        net: Scope::only(["127.0.0.1".to_string()]),
        max_calls: CountBound::AtMost(5),
        ..Caveats::top()
    }
}

/// The ceiling that opts the loopback host into private-address resolution — the
/// explicit, separate SSRF escape hatch for local testing (AB-007, #270).
fn loopback_private() -> Scope<String> {
    Scope::only(["127.0.0.1".to_string()])
}

#[tokio::test]
async fn loopback_allowlisted_fetch_succeeds_and_returns_markdown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/article"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<html><head><title>Leashed Page</title></head><body>\
                 <article><h1>Net Enforcer</h1>\
                 <p>This body is <b>data</b>, never an instruction.</p>\
                 <p>The leash screened the host and pinned the IP before fetching.</p>\
                 </article></body></html>",
            "text/html; charset=utf-8",
        ))
        .expect(1)
        .mount(&server)
        .await;

    // host = 127.0.0.1: reachable via `net` AND opted into loopback space via
    // the `net_private` ceiling.
    let url = format!("http://127.0.0.1:{}/article", server.address().port());

    let out = fetch_inner(
        &loopback_grant(),
        &loopback_private(),
        serde_json::json!({ "url": url }),
    )
    .await
    .expect("fetch should succeed for a reachable, private-opted-in loopback host");

    server.verify().await;
    assert_eq!(out["status"], 200);
    let md = out["markdown"].as_str().unwrap();
    assert!(md.contains("Net Enforcer"), "markdown was {md:?}");
    assert!(md.contains("data"), "markdown was {md:?}");
    // The body is returned as structured data, not framed as instructions.
    assert!(out["url"].as_str().unwrap().contains("/article"));
    assert!(out["final_url"].as_str().unwrap().contains("/article"));
}

#[tokio::test]
async fn loopback_denied_when_only_example_com_granted() {
    // Same loopback mock, but the grant only permits example.com. This proves
    // BOTH protections in one test: the host allowlist rejects 127.0.0.1 (it is
    // not in Only{example.com}), and even had it been permitted, the SSRF screen
    // would block the loopback address. The request must never reach the server.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/article"))
        .respond_with(ResponseTemplate::new(200).set_body_string("should never be served"))
        .expect(0)
        .mount(&server)
        .await;

    let granted = Caveats {
        net: Scope::only(["example.com".to_string()]),
        ..Caveats::top()
    };
    let url = format!("http://127.0.0.1:{}/article", server.address().port());

    let err = fetch(&granted, serde_json::json!({ "url": url }))
        .await
        .expect_err("loopback fetch must be denied when only example.com is granted");

    assert!(matches!(err, WebFetchError::Denied { .. }), "got {err:?}");
    // The mock was never hit — the leash denied before any request.
    server.verify().await;
}

#[tokio::test]
async fn ab007_net_allowlist_alone_does_not_open_private_space() {
    // THE AB-007 regression, end-to-end. The grant makes 127.0.0.1 *reachable*
    // (it is in `net`), but the public `fetch` opts NO host into private space
    // (`net_private = none`). So the fetch to the loopback mock must be
    // SSRF-denied and the server never contacted — even though the host is on
    // the `net` allowlist. On the pre-#270 code, `net` membership *was* the
    // private-space opt-in, so this fetch succeeded.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/article"))
        .respond_with(ResponseTemplate::new(200).set_body_string("should never be served"))
        .expect(0)
        .mount(&server)
        .await;

    let url = format!("http://127.0.0.1:{}/article", server.address().port());

    let err = fetch(&loopback_grant(), serde_json::json!({ "url": url })) // net_private = none
        .await
        .expect_err("an allowlisted-but-not-private-opted-in loopback host must be SSRF-denied");

    assert!(matches!(err, WebFetchError::Denied { .. }), "got {err:?}");
    if let WebFetchError::Denied { reason } = &err {
        assert!(
            reason.contains("SSRF") || reason.contains("private"),
            "denial must be the private-address screen, got {reason:?}"
        );
    }
    // The screen denied before any request left the process.
    server.verify().await;
}

#[tokio::test]
async fn redirect_to_disallowed_host_is_denied() {
    // The mock returns a 302 whose Location points at a host NOT in the grant.
    // The leash re-screens the redirect target and denies it; the disallowed
    // host is never contacted. We allowlist the loopback host so the FIRST hop
    // is permitted — the denial must come from the SECOND (redirect) hop.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/go"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "http://evil.disallowed.example/secret"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let url = format!("http://127.0.0.1:{}/go", server.address().port());

    let err = fetch_inner(
        &loopback_grant(), // permits 127.0.0.1, NOT evil.*
        &loopback_private(),
        serde_json::json!({ "url": url }),
    )
    .await
    .expect_err("redirect to a disallowed host must be denied");

    server.verify().await; // first hop was made...
    assert!(matches!(err, WebFetchError::Denied { .. }), "got {err:?}");
    // ...and the denial names the disallowed redirect host.
    if let WebFetchError::Denied { reason } = &err {
        assert!(
            reason.contains("evil.disallowed.example"),
            "expected the denial to name the redirect host, got {reason:?}"
        );
    }
}

#[tokio::test]
async fn redirect_to_allowed_loopback_path_is_followed() {
    // A redirect WITHIN the allowlisted host is followed (each hop re-screened
    // and allowed), proving the manual redirect loop also permits, not only
    // denies.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/from"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/to"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/to"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<html><head><title>Arrived</title></head><body><p>Followed safely.</p></body></html>",
            "text/html",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let url = format!("http://127.0.0.1:{}/from", server.address().port());

    let out = fetch_inner(
        &loopback_grant(),
        &loopback_private(),
        serde_json::json!({ "url": url }),
    )
    .await
    .expect("a redirect within the allowlisted host should be followed");

    server.verify().await;
    assert_eq!(out["status"], 200);
    assert!(out["final_url"].as_str().unwrap().contains("/to"));
    assert!(out["markdown"]
        .as_str()
        .unwrap()
        .contains("Followed safely"));
}

/// The early-out in `read_capped`: a body that declares more than `max_bytes` is
/// refused, not read.
#[tokio::test]
async fn a_declared_length_over_the_cap_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(64)))
        .mount(&server)
        .await;
    let url = format!("http://127.0.0.1:{}/big", server.address().port());

    let err = fetch_inner(
        &loopback_grant(),
        &loopback_private(),
        serde_json::json!({ "url": url, "max_bytes": 16 }),
    )
    .await
    .unwrap_err();

    assert_eq!(
        err.to_string(),
        "denied: response body 64 bytes exceeds max_bytes 16"
    );
}

/// The streaming cap in `read_capped`: a body with no `Content-Length` is cut at
/// `max_bytes`, not refused. wiremock always declares a length, so the server is a
/// raw loopback listener that writes the body and closes.
#[tokio::test]
async fn an_undeclared_length_is_cut_at_the_cap() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Not awaited: the runtime drops it with the test. Write errors are the client
    // having stopped reading, which is what the cap does.
    let _server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let _ = sock.read(&mut request).await;
        let mut response =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(&[b'A'; 16]);
        response.extend_from_slice(&[b'B'; 48]);
        let _ = sock.write_all(&response).await;
        let _ = sock.shutdown().await;
    });

    let out = fetch_inner(
        &loopback_grant(),
        &loopback_private(),
        serde_json::json!({ "url": format!("http://127.0.0.1:{port}/"), "max_bytes": 16 }),
    )
    .await
    .expect("a body with no declared length is cut, not refused");

    let md = out["markdown"].as_str().unwrap();
    assert!(
        md.contains(&"A".repeat(16)) && !md.contains('B'),
        "markdown was {md:?}"
    );
}

/// `MAX_REDIRECTS`: ten redirects are followed and the eleventh is refused. bridle's
/// suite follows one.
#[tokio::test]
async fn the_eleventh_redirect_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .expect(11)
        .mount(&server)
        .await;
    let url = format!("http://127.0.0.1:{}/loop", server.address().port());

    let err = fetch_inner(
        &loopback_grant(),
        &loopback_private(),
        serde_json::json!({ "url": url }),
    )
    .await
    .unwrap_err();

    assert_eq!(err.to_string(), "denied: too many redirects (>10)");
    server.verify().await;
}

/// The one request header that is not bridle's: the user agent names newt.
#[tokio::test]
async fn the_user_agent_names_newt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ua"))
        .and(header(
            "user-agent",
            concat!("newt-agent/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;
    let url = format!("http://127.0.0.1:{}/ua", server.address().port());

    let out = fetch_inner(
        &loopback_grant(),
        &loopback_private(),
        serde_json::json!({ "url": url }),
    )
    .await
    .unwrap();

    assert_eq!(out["status"], 200, "{out}");
    server.verify().await;
}

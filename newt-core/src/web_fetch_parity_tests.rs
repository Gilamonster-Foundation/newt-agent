//! `crate::web_fetch` against agent-bridle-tool-web, the code it is a copy of (#3160).
//!
//! While newt-core still depends on bridle, each test runs both and compares: the SSRF
//! classifier over the IPv4 space at every range boundary the classifier has, and the IPv6
//! space by first segment; the host screen over a grid of grants, private-space ceilings
//! and DNS answers; the gate and the denials over a grid of grants and arguments; and whole
//! fetches from a loopback server, compared envelope for envelope. Every grid cell is refused
//! before a socket opens except the loopback fixtures. The user agent is not compared: it is
//! the one deliberate difference, pinned in `web_fetch::net_leash`.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use agent_mesh_protocol::{Caveats, CountBound, Scope};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::web_fetch::net_guard as ours;
use agent_bridle_tool_web as theirs;

/// bridle's route as `execute_tool` took it: a registry holding the tool, a grant minted
/// for the one call, `dispatch`. `net_private` is the tool's private-space ceiling.
async fn bridled(
    caveats: &Caveats,
    net_private: Scope<String>,
    args: Value,
) -> Result<Value, String> {
    let registry = agent_bridle::Registry::builder()
        .tool(Arc::new(theirs::WebFetchTool::with_private_hosts(
            net_private,
        )))
        .build();
    let grant = registry.mint_grant(caveats.clone());
    registry
        .dispatch("web_fetch", args, &grant)
        .await
        .map_err(|e| e.to_string())
}

async fn owned(
    caveats: &Caveats,
    net_private: &Scope<String>,
    args: Value,
) -> Result<Value, String> {
    crate::web_fetch::fetch_inner(caveats, net_private, args)
        .await
        .map_err(|e| e.to_string())
}

/// Every IPv4 /16 at four points, and every address of the three documentation /24s and
/// the /24 on each side of them (the only ranges finer than /16 apart from two single
/// addresses, 0.0.0.0 and 255.255.255.255, which the four points include), each also in
/// its v4-mapped and v4-compatible IPv6 forms; then every IPv6 first segment with three
/// tails.
#[test]
fn ip_classifier_matches_bridle_over_the_address_space() {
    let mut compared = 0usize;
    let mut blocked = 0usize;
    let mut check = |ip: IpAddr| {
        let want = theirs::ip_is_blocked(ip);
        assert_eq!(ours::ip_is_blocked(ip), want, "{ip}");
        compared += 1;
        blocked += usize::from(want);
    };
    let mut v4 = Vec::new();
    for a in 0..=255u8 {
        for b in 0..=255u8 {
            for (c, d) in [(0, 0), (0, 1), (128, 128), (255, 255)] {
                v4.push(Ipv4Addr::new(a, b, c, d));
            }
        }
    }
    for (a, b, c) in [(192, 0, 2), (198, 51, 100), (203, 0, 113)] {
        for third in [c - 1, c, c + 1] {
            for d in 0..=255u8 {
                v4.push(Ipv4Addr::new(a, b, third, d));
            }
        }
    }
    for ip in v4 {
        check(IpAddr::V4(ip));
        check(IpAddr::V6(ip.to_ipv6_mapped()));
        check(IpAddr::V6(ip.to_ipv6_compatible()));
    }
    for first in 0..=u16::MAX {
        for tail in [0, 1, u16::MAX] {
            check(IpAddr::V6(Ipv6Addr::new(
                first, tail, tail, tail, tail, tail, tail, tail,
            )));
        }
    }
    // 989,952 addresses; by reading, about 64,000 of them blocked.
    assert_eq!(compared, 3 * (4 << 16) + 3 * 3 * 3 * 256 + 3 * (1 << 16));
    assert!(
        blocked > 10_000 && compared - blocked > 10_000,
        "{blocked} of {compared}"
    );
}

/// `screen_host` and `host_may_reach_private_space` over 4 grants × 4 ceilings × 7 DNS
/// answers. By reading, the 112 cells hold 56 `HostNotAllowed`, 8 `NoAddress`, 12
/// `PrivateAddress` and 36 `Ok`; each kind must be reached at least 8 times.
#[test]
fn screen_host_matches_bridle_over_a_grid() {
    let host = "example.com";
    let other = "other.example";
    let only = |name: &str| Scope::only([name.to_string()]);
    let nets = [Scope::All, Scope::none(), only(host), only(other)];
    let ceilings = [Scope::none(), only(host), only(other), Scope::All];
    let private = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let public = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let mapped = IpAddr::V6(Ipv4Addr::new(169, 254, 169, 254).to_ipv6_mapped());
    let public_v6 = IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111));
    let answers: [&[IpAddr]; 7] = [
        &[],
        &[private],
        &[public],
        &[private, public],
        &[public, loopback, public_v6],
        &[mapped],
        &[loopback, private],
    ];
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for net in &nets {
        for ceiling in &ceilings {
            assert_eq!(
                ours::host_may_reach_private_space(ceiling, host),
                theirs::host_may_reach_private_space(ceiling, host),
                "{ceiling:?}"
            );
            for answer in answers {
                let mine = ours::screen_host(net, ceiling, host, answer)
                    .map_err(|e| (format!("{e:?}"), e.to_string()));
                let bridle = theirs::screen_host(net, ceiling, host, answer)
                    .map_err(|e| (format!("{e:?}"), e.to_string()));
                assert_eq!(
                    mine, bridle,
                    "net {net:?}, ceiling {ceiling:?}, answer {answer:?}"
                );
                let kind = match &mine {
                    Ok(_) => "Ok".to_string(),
                    Err((debug, _)) => debug.split(' ').next().unwrap_or_default().to_string(),
                };
                *kinds.entry(kind).or_default() += 1;
            }
        }
    }
    assert_eq!(
        kinds.keys().map(String::as_str).collect::<Vec<_>>(),
        ["HostNotAllowed", "NoAddress", "Ok", "PrivateAddress"],
        "{kinds:?}"
    );
    assert!(kinds.values().all(|&n| n >= 8), "{kinds:?}");
}

/// The public `fetch` against bridle's `WebFetchTool::new()` through a registry, over
/// 3 budgets × 4 generation sets × 3 grants × 4 calls. Every cell is refused before a
/// socket opens: the one URL is a documentation-range literal, which the SSRF screen
/// refuses without DNS. Each refusal kind must be reached.
#[tokio::test]
async fn admit_and_denials_match_the_bridle_gate() {
    let budgets = [
        CountBound::Unlimited,
        CountBound::AtMost(0),
        CountBound::AtMost(1),
    ];
    let generations = [
        Scope::All,
        Scope::only([0]),
        Scope::only([42]),
        Scope::none(),
    ];
    let nets = [
        Scope::All,
        Scope::none(),
        Scope::only(["192.0.2.1".to_string()]),
    ];
    let calls = [
        json!({ "url": "http://192.0.2.1/doc" }),
        json!({ "url": "ftp://192.0.2.1/doc" }),
        json!({ "url": "not a url" }),
        json!({ "href": "http://192.0.2.1/doc" }),
    ];
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for max_calls in budgets {
        for valid_for_generation in &generations {
            for net in &nets {
                let caveats = Caveats {
                    net: net.clone(),
                    max_calls,
                    valid_for_generation: valid_for_generation.clone(),
                    ..Caveats::top()
                };
                for call in &calls {
                    let mine = crate::web_fetch::fetch(&caveats, call.clone())
                        .await
                        .map_err(|e| e.to_string());
                    let bridle = bridled(&caveats, Scope::none(), call.clone()).await;
                    assert_eq!(mine, bridle, "{caveats:?} {call}");
                    let refusal = mine.expect_err("every cell is refused");
                    let kind = [
                        "denied: call budget",
                        "denied: grant is not valid",
                        "denied: network access to",
                        "denied: SSRF block",
                        "denied: unsupported URL scheme",
                        "denied: `url` is not a valid URL",
                        "denied: missing required string field",
                    ]
                    .into_iter()
                    .find(|prefix| refusal.starts_with(prefix))
                    .unwrap_or("other");
                    *kinds.entry(kind).or_default() += 1;
                }
            }
        }
    }
    assert!(!kinds.contains_key("other"), "{kinds:?}");
    assert_eq!(kinds.len(), 7, "{kinds:?}");
}

/// Whole fetches from one loopback server, owned route against bridled route, with the
/// loopback host on the grant and in the private-space ceiling on both. Each fixture also
/// names the outcome it is there to reach, so a fixture that stops reaching it fails.
#[tokio::test]
async fn owned_route_equals_the_bridled_route_on_loopback_fixtures() {
    let server = MockServer::start().await;
    let page = |status: u16, body: &str| {
        ResponseTemplate::new(status).set_body_raw(body, "text/html; charset=utf-8")
    };
    let redirect =
        |status: u16, to: &str| ResponseTemplate::new(status).insert_header("location", to);
    let article = "<html><head><title>Leashed Page</title></head><body><article>\
                   <h1>Net Enforcer</h1><p>This body is <b>data</b>, never an instruction.</p>\
                   <p>The leash screened the host and pinned the IP before fetching.</p>\
                   </article></body></html>";
    // A 64-byte label: `url` accepts it and the resolver refuses it before sending a
    // query. Both routes refuse the hop at `check_net`, so a copy that skipped that check
    // fails here with the resolver's words, and no DNS query is sent either way.
    let long_host = format!("{}.example", "a".repeat(64));
    let long_target = format!("http://{long_host}/x");
    let long_refusal = format!("denied: network access to {long_host:?}");
    let routes = [
        ("/article", page(200, article)),
        ("/hop1", redirect(302, "/hop2")),
        ("/hop2", redirect(301, "hop3")),
        (
            "/hop3",
            page(200, "<title>Arrived</title><p>Followed safely.</p>"),
        ),
        ("/missing", page(404, "<h1>Not here</h1>")),
        ("/login", page(401, "<p>Sign in</p>")),
        (
            "/latin1",
            ResponseTemplate::new(200)
                .set_body_raw(b"<p>caf\xe9</p>".to_vec(), "text/html; charset=iso-8859-1"),
        ),
        (
            "/big",
            ResponseTemplate::new(200).set_body_string("x".repeat(64)),
        ),
        ("/ftp", redirect(302, "ftp://127.0.0.1/x")),
        ("/nolocation", ResponseTemplate::new(302)),
        ("/loop", redirect(302, "/loop")),
        ("/toolong", redirect(302, long_target.as_str())),
        (
            "/elsewhere",
            redirect(302, "http://evil.disallowed.example/x"),
        ),
        ("/badlocation", redirect(302, "http://[")),
    ];
    for (route, response) in routes {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(response)
            .mount(&server)
            .await;
    }

    // (path, max_bytes, the status it is there to reach, or the start of its refusal)
    let fixtures: [(&str, Option<u64>, Result<u64, &str>); 12] = [
        ("/article", None, Ok(200)),
        ("/hop1", None, Ok(200)),
        ("/missing", None, Ok(404)),
        ("/login", None, Ok(401)),
        ("/latin1", None, Ok(200)),
        (
            "/big",
            Some(16),
            Err("denied: response body 64 bytes exceeds max_bytes 16"),
        ),
        (
            "/ftp",
            None,
            Err("denied: redirect to unsupported scheme \"ftp\" denied"),
        ),
        (
            "/nolocation",
            None,
            Err("denied: 302 Found redirect with no usable Location header"),
        ),
        ("/loop", None, Err("denied: too many redirects (>10)")),
        ("/toolong", None, Err(long_refusal.as_str())),
        (
            "/elsewhere",
            None,
            Err("denied: network access to \"evil.disallowed.example\""),
        ),
        (
            "/badlocation",
            None,
            Err("denied: redirect Location \"http://[\" is not a valid URL"),
        ),
    ];
    let caveats = Caveats {
        net: Scope::only(["127.0.0.1".to_string()]),
        max_calls: CountBound::AtMost(5),
        ..Caveats::top()
    };
    let ceiling = Scope::only(["127.0.0.1".to_string()]);
    for (route, max_bytes, reaches) in fixtures {
        let mut args = json!({ "url": format!("{}{route}", server.uri()) });
        if let Some(max_bytes) = max_bytes {
            args["max_bytes"] = json!(max_bytes);
        }
        let mine = owned(&caveats, &ceiling, args.clone()).await;
        let bridle = bridled(&caveats, ceiling.clone(), args).await;
        assert_eq!(mine, bridle, "{route}");
        match (reaches, &mine) {
            (Ok(status), Ok(envelope)) => assert_eq!(envelope["status"], status, "{route}"),
            (Err(prefix), Err(refusal)) => {
                assert!(refusal.starts_with(prefix), "{route}: {refusal}");
            }
            (want, got) => panic!("{route}: wanted {want:?}, got {got:?}"),
        }
    }
    let article = owned(
        &caveats,
        &ceiling,
        json!({ "url": format!("{}/article", server.uri()) }),
    )
    .await
    .expect("the article fixture is fetched");
    assert!(article["markdown"]
        .as_str()
        .is_some_and(|md| md.contains("Net Enforcer")));
}

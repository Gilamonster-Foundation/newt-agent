//! The confined `web_fetch` — `web_fetch(url)` → structured markdown.
//!
//! Copied from agent-bridle-tool-web 0.8.0-rc.7 (`src/web_fetch.rs`, Apache-2.0, git
//! e932a92229eb7a36423d837d84cbbd4dfb2755de) so newt-core fetches without a bridle crate
//! (#3160, the `confine` feature). The fetch loop, the resolver, the client, the body cap and
//! the extractor are bridle's lines. What differs:
//!
//! - No `WebFetchTool`, `Tool` impl or schema. [`fetch`] is the entry; it passes the
//!   private-space ceiling `WebFetchTool::new()` has, `Scope::none()`, and `fetch_inner`
//!   takes the one `WebFetchTool::with_private_hosts` held.
//! - What the gate refuses for a grant minted per call is `admit` (the budget, then
//!   generation 0), and `ToolContext::check_net` is `check_net`, word for word. The three
//!   `ToolError` variants a fetch can return are [`WebFetchError`], with agent-bridle-core
//!   `error.rs`'s lines for them, so the same `Display`.
//! - The user agent is `newt-agent/<version>`; bridle sends `agent-bridle-web/<version>`.
//! - `reqwest::Url` for `url::Url` (one type), and one format argument inlined for the
//!   workspace lint `uninlined_format_args` (same text).
//!
//! Kept as bridle has it, each a follow-up: the client honours `HTTP(S)_PROXY`, and a proxy
//! resolves the host itself, so the DNS pin does not bind it; the resolver falls back to
//! Google's public DNS when the system configuration cannot be read; a bracketed IPv6
//! literal reaches the resolver as a name, so it is refused; the 30 s limit is per request,
//! up to 11 of them; a resolver is built per call. `DESIGN §7`, `AB-007` and `#270` are
//! agent-bridle's. `web_fetch_parity_tests` holds this module equal to bridle's while bridle
//! is a dependency.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use agent_mesh_protocol::{Caveats, CountBound, Scope};
use hickory_resolver::TokioResolver;
use reqwest::Url;

pub(crate) mod net_guard;
#[cfg(test)]
#[path = "web_fetch/net_leash.rs"]
mod net_leash;

use net_guard::{host_is_permitted, screen_host, NetGuardError};

/// Maximum redirect hops followed before giving up. Each hop is independently
/// leash-screened (host allowlist + SSRF + IP pin).
const MAX_REDIRECTS: usize = 10;
/// Default cap on bytes read from a response body. Overridable per call via
/// `max_bytes`, but never above [`HARD_MAX_BYTES`].
const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;
/// Absolute ceiling on `max_bytes` regardless of the request.
const HARD_MAX_BYTES: usize = 25 * 1024 * 1024;
/// Per-request wall-clock timeout (bounds work; not a coordination primitive).
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Why a fetch was refused: the three `agent_bridle::ToolError` variants bridle's
/// `web_fetch` could return. `execute_tool` shows the model the `Display`, and journals a
/// host denial by parsing a `Denied` reason.
#[derive(Debug)]
pub enum WebFetchError {
    /// The leash denied the fetch, or the fetch failed (SSRF screen, DNS, transport,
    /// redirect, body cap).
    Denied {
        /// Human-readable reason (safe to surface to the agent).
        reason: String,
    },
    /// The grant's `max_calls` budget is exhausted.
    Budget,
    /// The gate's generation is not in the grant's `valid_for_generation` set.
    Generation,
}

impl WebFetchError {
    /// Convenience constructor for a denial with a formatted reason.
    pub(crate) fn denied(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for WebFetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { reason } => write!(f, "denied: {reason}"),
            Self::Budget => write!(f, "denied: call budget (max_calls) exhausted"),
            Self::Generation => {
                write!(f, "denied: grant is not valid for the gate's generation")
            }
        }
    }
}

impl std::error::Error for WebFetchError {}

type FetchResult<T> = Result<T, WebFetchError>;

/// Fetch a URL, follow redirects **under the leash**, and return the page's main
/// content as markdown: bridle's `web_fetch` as `agent_bridle::registry()` registered it.
///
/// The `net` enforcement (DESIGN §7) runs before the first request and on every
/// redirect hop:
///
/// 1. the hop's host must satisfy the effective `net` caveat (`check_net`);
/// 2. the host is resolved and its addresses are SSRF-screened — private /
///    loopback / link-local / unique-local addresses are rejected unless the
///    host is named in the separate `net_private` opt-in (AB-007, #270);
///    membership in the `net` reachability allowlist never opens private space;
/// 3. the connection is **pinned** to a screened address (anti-rebinding);
/// 4. redirects are followed manually so each `Location` is re-screened, never
///    blindly trusted.
///
/// The returned body is **untrusted data**: `{ url, final_url, status, title,
/// markdown }`, never framed as instructions.
///
/// ## Private-space authority model (AB-007, #270)
///
/// Permission to resolve into private / loopback / link-local / unique-local /
/// metadata address space is an **immutable ceiling**, not a [`Caveats`] axis. Here it is
/// `Scope::none()`, as `WebFetchTool::new()` had it: private-address resolution is fully
/// blocked, and no `net` grant, however permissive, widens it.
pub async fn fetch(
    caveats: &Caveats,
    args: serde_json::Value,
) -> Result<serde_json::Value, WebFetchError> {
    fetch_inner(caveats, &Scope::none(), args).await
}

/// [`fetch`] with the private-space ceiling as a parameter (what
/// `WebFetchTool::with_private_hosts` set): the gate's checks, then the body of bridle's
/// `WebFetchTool::invoke`.
pub(crate) async fn fetch_inner(
    caveats: &Caveats,
    net_private: &Scope<String>,
    args: serde_json::Value,
) -> FetchResult<serde_json::Value> {
    admit(caveats)?;
    let parsed = FetchArgs::parse(&args)?;
    let original_url = parsed.url.clone();

    let resolver = build_resolver()?;
    let outcome = fetch_with_leashed_redirects(caveats, net_private, &resolver, parsed).await?;

    // Extract the main content as markdown. Fetched bytes are DATA: we run
    // them through readability + HTML->markdown and return them in a
    // structured envelope, never as instructions.
    let (title, markdown) = extract_markdown(&outcome.body, outcome.final_url.as_str());

    Ok(serde_json::json!({
        "url": original_url.as_str(),
        "final_url": outcome.final_url.as_str(),
        "status": outcome.status,
        "title": title,
        "markdown": markdown,
    }))
}

/// What the gate refuses before `web_fetch` runs, for a grant minted for this one call (as
/// `execute_tool` mints it): `Registry::dispatch` charges the grant's budget first, which a
/// fresh grant fails only at `AtMost(0)`; `Gate::authorize` then requires generation 0, the
/// one `agent_bridle::registry()` dispatches at. The gate's other step, the meet with
/// `Tool::required()`, changes nothing: `web_fetch` requires `Caveats::top()`.
fn admit(caveats: &Caveats) -> FetchResult<()> {
    if matches!(caveats.max_calls, CountBound::AtMost(0)) {
        return Err(WebFetchError::Budget);
    }
    let ok = match &caveats.valid_for_generation {
        Scope::All => true,
        Scope::Only(set) => set.contains(&0),
    };
    if ok {
        Ok(())
    } else {
        Err(WebFetchError::Generation)
    }
}

/// Leash check: may this fetch reach network `host`? `ToolContext::check_net`'s test and
/// wording: `execute_tool` journals this denial by parsing that text.
fn check_net(net: &Scope<String>, host: &str) -> FetchResult<()> {
    if host_is_permitted(net, host) {
        Ok(())
    } else {
        Err(WebFetchError::denied(format!(
            "network access to {host:?} is not within the granted authority"
        )))
    }
}

/// Parsed, validated arguments for one fetch.
#[derive(Debug)]
struct FetchArgs {
    url: Url,
    max_bytes: usize,
}

impl FetchArgs {
    fn parse(args: &serde_json::Value) -> FetchResult<Self> {
        let raw = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| WebFetchError::denied("missing required string field `url`"))?;

        let url = Url::parse(raw)
            .map_err(|e| WebFetchError::denied(format!("`url` is not a valid URL: {e}")))?;

        // Only http(s) — the leash governs the `net` axis, not arbitrary URL
        // schemes (no file://, ftp://, gopher://, …, all of which are SSRF
        // vectors that bypass the host/IP screen entirely).
        match url.scheme() {
            "http" | "https" => {}
            other => {
                return Err(WebFetchError::denied(format!(
                    "unsupported URL scheme {other:?}: only http and https are allowed"
                )))
            }
        }

        let max_bytes = args
            .get("max_bytes")
            .and_then(serde_json::Value::as_u64)
            .map(|n| usize::try_from(n).unwrap_or(HARD_MAX_BYTES))
            .unwrap_or(DEFAULT_MAX_BYTES)
            .min(HARD_MAX_BYTES);

        Ok(Self { url, max_bytes })
    }
}

/// What a completed (leash-passing) fetch produced.
struct FetchOutcome {
    final_url: Url,
    status: u16,
    body: String,
}

/// Build a Tokio DNS resolver from the system config, falling back to a public
/// default if the system config cannot be read (e.g. minimal CI containers).
fn build_resolver() -> FetchResult<TokioResolver> {
    use hickory_resolver::config::{ResolverConfig, GOOGLE};
    use hickory_resolver::net::runtime::TokioRuntimeProvider;
    let builder = TokioResolver::builder_tokio().unwrap_or_else(|_| {
        TokioResolver::builder_with_config(
            ResolverConfig::udp_and_tcp(&GOOGLE),
            TokioRuntimeProvider::default(),
        )
    });
    builder
        .build()
        .map_err(|e| WebFetchError::denied(format!("could not build DNS resolver: {e}")))
}

/// Resolve `host` to a set of IPs. A literal IP host is returned as-is (no DNS).
async fn resolve_host(resolver: &TokioResolver, host: &str) -> FetchResult<Vec<IpAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    match resolver.lookup_ip(host).await {
        Ok(lookup) => Ok(lookup.iter().collect()),
        Err(e) => Err(WebFetchError::denied(format!(
            "could not resolve host {host:?}: {e}"
        ))),
    }
}

/// Fetch `start`, following redirects **manually** so the leash re-screens every
/// hop. Returns the final response body (capped at `max_bytes`).
async fn fetch_with_leashed_redirects(
    caveats: &Caveats,
    net_private: &Scope<String>,
    resolver: &TokioResolver,
    start: FetchArgs,
) -> FetchResult<FetchOutcome> {
    let max_bytes = start.max_bytes;
    let mut current = start.url;

    for _hop in 0..=MAX_REDIRECTS {
        let host = current
            .host_str()
            .ok_or_else(|| WebFetchError::denied("URL has no host"))?
            .to_string();
        let port = current
            .port_or_known_default()
            .ok_or_else(|| WebFetchError::denied("URL has no known port"))?;

        // (1) Host allowlist — default-deny, via the leash itself. This is the
        // `check_net` above; it consults the *effective* net caveat.
        check_net(&caveats.net, &host)?;

        // (2) Resolve + SSRF-screen. Reachability is the effective `net` scope;
        // crossing into private/loopback space requires the host be named in the
        // separate `net_private` opt-in (AB-007, #270) — the `net` allowlist
        // alone never opens private space.
        let resolved = resolve_host(resolver, &host).await?;
        let safe =
            screen_host(&caveats.net, net_private, &host, &resolved).map_err(net_guard_to_tool)?;

        // (3) Pin the connection to a screened IP (anti-DNS-rebinding TOCTOU):
        // reqwest connects to exactly this address rather than re-resolving, so
        // a rebind between our check and the connect cannot redirect traffic.
        let pinned: Vec<SocketAddr> = safe.iter().map(|&ip| SocketAddr::new(ip, port)).collect();
        let client = build_pinned_client(&host, &pinned)?;

        // (4) Send with redirects DISABLED so we re-screen each Location.
        let resp = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| WebFetchError::denied(format!("request to {host:?} failed: {e}")))?;

        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    WebFetchError::denied(format!(
                        "{status} redirect with no usable Location header"
                    ))
                })?;
            // Resolve the (possibly relative) Location against the current URL,
            // then loop — the next iteration re-runs steps (1)-(4) on the new
            // host. Never blindly follow.
            current = current.join(location).map_err(|e| {
                WebFetchError::denied(format!(
                    "redirect Location {location:?} is not a valid URL: {e}"
                ))
            })?;
            match current.scheme() {
                "http" | "https" => {}
                other => {
                    return Err(WebFetchError::denied(format!(
                        "redirect to unsupported scheme {other:?} denied"
                    )))
                }
            }
            continue;
        }

        // Terminal response: read the body (capped) and return.
        let final_url = resp.url().clone();
        let status_code = status.as_u16();
        let body = read_capped(resp, max_bytes).await?;
        return Ok(FetchOutcome {
            final_url,
            status: status_code,
            body,
        });
    }

    Err(WebFetchError::denied(format!(
        "too many redirects (>{MAX_REDIRECTS})"
    )))
}

/// Map a [`NetGuardError`] to a leash denial (the agent-safe surface).
fn net_guard_to_tool(e: NetGuardError) -> WebFetchError {
    WebFetchError::denied(e.to_string())
}

/// Build a reqwest client pinned to `pinned` for `host`, with redirects off and
/// a request timeout. rustls (no openssl); we never auto-follow redirects.
fn build_pinned_client(host: &str, pinned: &[SocketAddr]) -> FetchResult<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        // Pin DNS for this host to the screened addresses (anti-rebinding).
        .resolve_to_addrs(host, pinned)
        // As bridle builds it, without `.no_proxy()`: `HTTP(S)_PROXY` still applies, and
        // a proxy resolves `host` itself, past this pin (a #3160 follow-up).
        // A modest UA so well-behaved servers respond; identifies the tool.
        .user_agent(concat!("newt-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| WebFetchError::denied(format!("could not build HTTP client: {e}")))
}

/// Read a response body to a UTF-8 string, capped at `max_bytes`.
///
/// Streams chunk-by-chunk with a hard byte budget rather than buffering the
/// whole body first: a hostile server that omits `Content-Length` cannot make
/// us allocate an unbounded body (a memory-DoS vector). The declared
/// `Content-Length`, when present, is an early-out so we never even start a body
/// that announces itself as too big.
async fn read_capped(mut resp: reqwest::Response, max_bytes: usize) -> FetchResult<String> {
    // Decline obviously-oversized bodies up front when the server declares one.
    if let Some(len) = resp.content_length() {
        if len > max_bytes as u64 {
            return Err(WebFetchError::denied(format!(
                "response body {len} bytes exceeds max_bytes {max_bytes}"
            )));
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| WebFetchError::denied(format!("reading response body failed: {e}")))?
    {
        let room = max_bytes.saturating_sub(buf.len());
        if chunk.len() > room {
            // Take what fits and stop — we never hold more than max_bytes.
            buf.extend_from_slice(&chunk[..room]);
            break;
        }
        buf.extend_from_slice(&chunk);
        if buf.len() >= max_bytes {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Run fetched HTML through the readability extractor, then convert the cleaned
/// main-article HTML to markdown. Returns `(title, markdown)`.
///
/// Best-effort: if readability cannot find a main article (e.g. a tiny test
/// page), we fall back to converting the whole document; if even that fails,
/// the markdown is the raw text. The caller always gets *some* data — the
/// security guarantees are upstream (the net leash), not in the extractor.
fn extract_markdown(html: &str, url: &str) -> (Option<String>, String) {
    use dom_smoothie::Readability;

    // Readability first: pull the main article + a clean title.
    if let Ok(mut readability) = Readability::new(html, Some(url), None) {
        if let Ok(article) = readability.parse() {
            let title = non_empty(article.title);
            let markdown = htmd::convert(&article.content)
                .unwrap_or_else(|_| article.text_content.to_string());
            if !markdown.trim().is_empty() {
                return (title, markdown);
            }
        }
    }

    // Fallback: convert the whole document.
    let markdown = htmd::convert(html).unwrap_or_else(|_| html.to_string());
    (None, markdown)
}

/// `Some(s)` unless `s` is empty/whitespace.
fn non_empty(s: String) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AB-007 authority model: the private-space ceiling is immutable and
    /// **defaults to deny-all**. Rewritten from bridle's
    /// `private_space_ceiling_defaults_to_deny_all_and_is_construction_time`, which read the
    /// ceiling off `WebFetchTool::new()`: `fetch` has no field to read, so this asks it. A
    /// loopback host that IS on the `net` allowlist is still refused private space. (Port 1:
    /// were the ceiling ever to open, the connection is refused, not made.)
    #[tokio::test]
    async fn private_space_ceiling_is_deny_all_through_the_public_entry() {
        let err = fetch(
            &loopback_grant(),
            serde_json::json!({ "url": "http://127.0.0.1:1/" }),
        )
        .await
        .unwrap_err();
        match err {
            WebFetchError::Denied { reason } => {
                assert!(reason.starts_with("SSRF block:"), "got {reason:?}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    fn loopback_grant() -> Caveats {
        // 127.0.0.1 explicitly allowlisted -> opted into loopback space.
        Caveats {
            net: Scope::only(["127.0.0.1".to_string()]),
            max_calls: CountBound::AtMost(5),
            ..Caveats::top()
        }
    }

    #[test]
    fn parse_rejects_non_http_schemes() {
        // file:// and friends are SSRF vectors that bypass the host/IP screen.
        for bad in ["file:///etc/passwd", "ftp://x/y", "gopher://h/0"] {
            let err = FetchArgs::parse(&serde_json::json!({ "url": bad })).unwrap_err();
            assert!(
                matches!(err, WebFetchError::Denied { .. }),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn parse_caps_max_bytes_at_hard_ceiling() {
        let p = FetchArgs::parse(&serde_json::json!({
            "url": "http://example.com",
            "max_bytes": (HARD_MAX_BYTES as u64) * 4
        }))
        .unwrap();
        assert_eq!(p.max_bytes, HARD_MAX_BYTES);
    }

    #[tokio::test]
    async fn host_not_in_scope_is_denied_before_any_request() {
        // net = Only{example.com}; a fetch to 127.0.0.1 must be denied by the
        // host allowlist (and never reach the network).
        let granted = Caveats {
            net: Scope::only(["example.com".to_string()]),
            ..Caveats::top()
        };
        let err = fetch(
            &granted,
            serde_json::json!({ "url": "http://127.0.0.1:1/" }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebFetchError::Denied { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn private_ip_denied_when_host_permitted_but_not_optedin() {
        // net = All permits the host string, but does not opt it into private
        // space, so a literal private IP host is SSRF-blocked. (No DNS, no
        // network — the literal IP is screened directly.)
        let granted = Caveats {
            net: Scope::All,
            ..Caveats::top()
        };
        let err = fetch(&granted, serde_json::json!({ "url": "http://10.0.0.1/" }))
            .await
            .unwrap_err();
        match err {
            WebFetchError::Denied { reason } => {
                assert!(
                    reason.contains("SSRF"),
                    "expected SSRF denial, got {reason:?}"
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn extract_markdown_converts_simple_html() {
        let html = "<html><head><title>Hi</title></head><body><h1>Heading</h1>\
            <p>Hello <b>world</b>.</p></body></html>";
        let (_title, md) = extract_markdown(html, "http://example.com/");
        assert!(md.contains("Heading"), "markdown was {md:?}");
        assert!(md.contains("world"), "markdown was {md:?}");
    }

    #[test]
    fn net_reachability_does_not_grant_private_space() {
        // AB-007 (#270): a host on the `net` reachability grant is NOT thereby
        // opted into private/loopback space. That is a SEPARATE decision on the
        // `net_private` axis (the tool's config).
        let g = loopback_grant();
        // The grant makes 127.0.0.1 reachable...
        assert!(matches!(&g.net, Scope::Only(s) if s.contains("127.0.0.1")));
        // ...but the default private-space opt-in (`none`) grants it nothing,
        // while an explicit `net_private` naming it does.
        assert!(!net_guard::host_may_reach_private_space(
            &Scope::none(),
            "127.0.0.1"
        ));
        assert!(net_guard::host_may_reach_private_space(
            &Scope::only(["127.0.0.1".to_string()]),
            "127.0.0.1"
        ));
    }

    // newt's tests (#3160): what bridle's gate, error type and constants did for this tool.

    /// The gate's order (`Registry::dispatch`, then `Gate::authorize`): a grant of
    /// `AtMost(0)` is a budget refusal even when its generation is wrong too, and only
    /// generation 0 is admitted.
    #[test]
    fn admit_checks_the_budget_before_the_generation() {
        let grant = |max_calls: CountBound, valid_for_generation: Scope<u64>| Caveats {
            max_calls,
            valid_for_generation,
            ..Caveats::top()
        };
        let refusal = |caveats: &Caveats| admit(caveats).err().map(|e| e.to_string());
        let budget = Some(WebFetchError::Budget.to_string());
        let generation = Some(WebFetchError::Generation.to_string());
        assert_eq!(
            refusal(&grant(CountBound::AtMost(0), Scope::only([42]))),
            budget
        );
        assert_eq!(refusal(&grant(CountBound::AtMost(0), Scope::All)), budget);
        assert_eq!(
            refusal(&grant(CountBound::AtMost(1), Scope::only([42]))),
            generation
        );
        assert_eq!(
            refusal(&grant(CountBound::Unlimited, Scope::none())),
            generation
        );
        assert_eq!(
            refusal(&grant(CountBound::AtMost(1), Scope::only([0, 42]))),
            None
        );
        assert_eq!(refusal(&grant(CountBound::Unlimited, Scope::All)), None);
    }

    /// agent-bridle-core's `display_is_stable_and_safe`, for the three variants: this text
    /// is what the model and the denial journal see.
    #[test]
    fn display_is_stable_and_safe() {
        assert_eq!(WebFetchError::denied("nope").to_string(), "denied: nope");
        assert_eq!(
            WebFetchError::Budget.to_string(),
            "denied: call budget (max_calls) exhausted"
        );
        assert_eq!(
            WebFetchError::Generation.to_string(),
            "denied: grant is not valid for the gate's generation"
        );
    }

    /// bridle's limits, pinned. `catalog.rs` tells the model "default 5 MiB, max 25 MiB".
    #[test]
    fn the_limits_are_pinned() {
        assert_eq!(MAX_REDIRECTS, 10);
        assert_eq!(DEFAULT_MAX_BYTES, 5 << 20);
        assert_eq!(HARD_MAX_BYTES, 25 << 20);
        assert_eq!(REQUEST_TIMEOUT_SECS, 30);
    }
}

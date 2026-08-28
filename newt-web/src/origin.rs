//! Same-origin checking for state-changing requests.
//!
//! The second, independent half of the CSRF defence (see [`crate::csrf`] for
//! the first). They are kept separate because they fail differently:
//! double-submit degrades if an attacker can write a cookie for the site — a
//! sibling subdomain, or a network position on a plain-HTTP LAN deployment —
//! and this check does not. Neither is a substitute for the other, which is
//! why a request must satisfy both.
//!
//! **Absent means refused.** Every browser has sent `Origin` on form posts and
//! `fetch` for years, and `Referer` covers the stragglers. A state-changing
//! request that claims neither is not a browser doing what this surface
//! expects, so it fails closed. That is also why the machine dock API
//! (`/api/sessions/:id/inject`, called by `ureq`, which sends neither) sits
//! outside this gate deliberately rather than by omission.

/// Where a request says it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginVerdict {
    /// Same origin as the resource — proceed.
    SameOrigin,
    /// A different origin, an opaque origin (`null`), or none at all.
    Refused,
}

/// The `scheme://authority` prefix of a URL, or the string itself when it is
/// already bare. `Referer` carries a full URL; `Origin` carries only this.
fn origin_of(url: &str) -> Option<&str> {
    let after_scheme = url.find("://")? + 3;
    let end = url[after_scheme..]
        .find('/')
        .map_or(url.len(), |i| after_scheme + i);
    Some(&url[..end])
}

/// The authority (`host:port`) of an origin.
fn authority_of(origin: &str) -> Option<&str> {
    origin.find("://").map(|i| &origin[i + 3..])
}

/// Decide whether a state-changing request is same-origin.
///
/// Pure over its inputs so the whole rule is testable without an HTTP server
/// or process env — the reason `NEWT_WEB_ORIGIN` arrives as a parameter rather
/// than being read here.
///
/// * `configured` (from `NEWT_WEB_ORIGIN`) is an EXACT match when set. Behind
///   the SSO ingress the browser's origin is the public HTTPS one and bears no
///   relation to the pod's `Host`, so the deployment states it.
/// * Otherwise the origin's authority must equal the request's `Host`. That is
///   the check that works for the loopback and LAN binds, where the scheme
///   cannot be known from inside the process (a terminating proxy may have
///   spoken HTTPS to the client and HTTP to us).
#[must_use]
pub fn check(
    origin: Option<&str>,
    referer: Option<&str>,
    host: Option<&str>,
    configured: Option<&str>,
) -> OriginVerdict {
    // `Origin` is the authoritative statement; `Referer` is the fallback for a
    // client that sends only that. A request offering neither is refused.
    let Some(claimed) = origin.or_else(|| referer.and_then(origin_of)) else {
        return OriginVerdict::Refused;
    };
    // "null" is what a sandboxed iframe or a redirected cross-origin post
    // sends. It is an origin that matches nothing, and must never match ours.
    if claimed.is_empty() || claimed.eq_ignore_ascii_case("null") {
        return OriginVerdict::Refused;
    }
    if let Some(expected) = configured.filter(|e| !e.trim().is_empty()) {
        return if claimed == expected.trim() {
            OriginVerdict::SameOrigin
        } else {
            OriginVerdict::Refused
        };
    }
    match (authority_of(claimed), host) {
        // Case-insensitive on the host, which is not case-sensitive; the port
        // is compared as written, because it is digits either way.
        (Some(a), Some(h)) if !h.is_empty() && a.eq_ignore_ascii_case(h) => {
            OriginVerdict::SameOrigin
        }
        _ => OriginVerdict::Refused,
    }
}

#[cfg(test)]
mod tests {
    use super::{check, OriginVerdict::*};

    #[test]
    fn a_matching_authority_is_same_origin() {
        assert_eq!(
            check(
                Some("http://127.0.0.1:8880"),
                None,
                Some("127.0.0.1:8880"),
                None
            ),
            SameOrigin
        );
        // Scheme is deliberately not compared without an explicit config: a
        // terminating proxy may speak HTTPS outward and HTTP to us.
        assert_eq!(
            check(
                Some("https://box.lan:8880"),
                None,
                Some("box.lan:8880"),
                None
            ),
            SameOrigin
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for (origin, host) in [
            (Some("https://evil.test"), Some("127.0.0.1:8880")),
            // A different PORT is a different origin.
            (Some("http://127.0.0.1:9999"), Some("127.0.0.1:8880")),
            // The host as a path segment must not satisfy a substring test.
            (
                Some("https://evil.test/127.0.0.1:8880"),
                Some("127.0.0.1:8880"),
            ),
            // An opaque origin matches nothing.
            (Some("null"), Some("127.0.0.1:8880")),
            (Some(""), Some("127.0.0.1:8880")),
            // No origin at all, and no Host to compare against.
            (None, Some("127.0.0.1:8880")),
            (Some("http://127.0.0.1:8880"), None),
        ] {
            assert_eq!(
                check(origin, None, host, None),
                Refused,
                "{origin:?} {host:?}"
            );
        }
    }

    #[test]
    fn referer_is_the_fallback_and_is_reduced_to_its_origin() {
        assert_eq!(
            check(
                None,
                Some("http://127.0.0.1:8880/some/page?q=1"),
                Some("127.0.0.1:8880"),
                None
            ),
            SameOrigin
        );
        assert_eq!(
            check(
                None,
                Some("https://evil.test/x"),
                Some("127.0.0.1:8880"),
                None
            ),
            Refused
        );
        // Origin wins when both are present — a Referer cannot rescue a
        // cross-site Origin.
        assert_eq!(
            check(
                Some("https://evil.test"),
                Some("http://127.0.0.1:8880/"),
                Some("127.0.0.1:8880"),
                None
            ),
            Refused
        );
    }

    #[test]
    fn a_configured_origin_is_matched_exactly() {
        let cfg = Some("https://newt.example");
        assert_eq!(
            check(Some("https://newt.example"), None, Some("pod:8880"), cfg),
            SameOrigin
        );
        // …and the Host no longer rescues a mismatch.
        assert_eq!(
            check(Some("http://pod:8880"), None, Some("pod:8880"), cfg),
            Refused
        );
        assert_eq!(
            check(Some("https://newt.example.evil"), None, None, cfg),
            Refused
        );
        // A blank configured value is not a configuration.
        assert_eq!(
            check(Some("http://h:1"), None, Some("h:1"), Some("   ")),
            SameOrigin
        );
    }
}

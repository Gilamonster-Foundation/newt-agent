//! Double-submit CSRF tokens for the browser form surface.
//!
//! The cockpit sits behind a forward-auth proxy, so a cross-site request from
//! a page the operator happens to have open arrives **already authenticated** —
//! the browser attaches the proxy's session cookie whether or not the operator
//! meant to send anything. Authentication therefore does not answer "did the
//! operator ask for this"; that is what this module is for.
//!
//! **Double-submit, not a server-side session table.** The token is issued in
//! a `SameSite=Strict` cookie and echoed in a hidden form field; a request is
//! accepted only when the two match. A cross-site POST cannot read the cookie
//! (same-origin policy) *and* cannot cause it to be sent (`SameSite=Strict`),
//! so it cannot make the pair agree. This needs no server state, which matters
//! for a surface that is restarted freely and has no session store.
//!
//! It is deliberately paired with an `Origin` check rather than trusted alone:
//! double-submit degrades if an attacker can write a cookie for the site (a
//! subdomain, a MITM on a plain-HTTP LAN deployment), and the Origin check does
//! not. Two independent layers, and [`crate::csrf`] owns only one of them.

/// The cookie the token is issued in.
///
/// Not `__Host-` prefixed: that prefix requires `Secure`, which requires
/// HTTPS, and the documented dev posture is a loopback HTTP bind. The
/// deployment behind the SSO ingress is HTTPS and SHOULD carry the prefix —
/// tracked with the origin configuration rather than hardcoded here.
pub const COOKIE: &str = "newt_csrf";

/// The form field carrying the echo.
pub const FIELD: &str = "csrf";

/// A CSRF token.
///
/// Like [`crate::csp::Nonce`], there is no constructor from a caller-supplied
/// string: a token an attacker can predict is not a token, and the surest way
/// to prevent a reused constant is to make reuse unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Mint a fresh token from the same unguessable source the interaction
    /// nonces and CSP nonces already trust, rather than adding a second
    /// randomness dependency with its own failure modes.
    #[must_use]
    pub fn fresh() -> Self {
        Self(newt_core::new_conversation_id())
    }

    /// Adopt a token this browser already holds.
    ///
    /// Distinct from a public `From<String>`: it is named for what it is, so
    /// the one call site that reuses a cookie reads as reuse, and a constant
    /// still cannot be smuggled in as if it were minted.
    #[must_use]
    pub fn adopt(existing: String) -> Self {
        Self(existing)
    }

    /// The token text, for the cookie and the hidden field.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The `Set-Cookie` value that issues `token`.
///
/// `SameSite=Strict` is the half that makes double-submit sound: it is what
/// stops the cookie riding along on a cross-site POST. `HttpOnly` because
/// nothing reads this from script — the token reaches every form as a rendered
/// hidden field, so the enhanced path needs no script access either.
#[must_use]
pub fn set_cookie(token: &Token) -> String {
    format!(
        "{COOKIE}={}; Path=/; SameSite=Strict; HttpOnly",
        token.as_str()
    )
}

/// The token carried by a `Cookie` header, if any.
///
/// Tolerates the `a=1; b=2` form and surrounding spaces, and matches the
/// cookie NAME exactly — a `not_newt_csrf=…` must not satisfy a prefix test.
#[must_use]
pub fn from_cookie_header(raw: &str) -> Option<String> {
    raw.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == COOKIE).then(|| value.trim().to_string())
    })
}

/// The `csrf` field of an `application/x-www-form-urlencoded` body.
///
/// Percent-decoding is deliberately limited to `+` and `%XX`, which is the
/// whole of the form encoding; a token is minted from an alphanumeric-and-
/// hyphen alphabet, so this only ever has to survive a client that encoded
/// more than it needed to.
#[must_use]
pub fn from_form_body(body: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == FIELD).then(|| percent_decode(value))
    })
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether the cookie and the submitted field agree.
///
/// Constant-time in the COMPARISON so a token cannot be recovered a byte at a
/// time by timing repeated submissions. The length check leaks only the
/// length, which the alphabet already fixes.
#[must_use]
pub fn matches(cookie: &str, submitted: &str) -> bool {
    if cookie.is_empty() || cookie.len() != submitted.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in cookie.bytes().zip(submitted.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// The hidden field every form carries, ready to interpolate.
#[must_use]
pub fn hidden_field(token: &str) -> String {
    format!(
        r#"<input type="hidden" name="{FIELD}" value="{}">"#,
        crate::escape_attr(token)
    )
}

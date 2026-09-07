use super::*;

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}
fn hexs(s: &str) -> String {
    s.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn session_filter_registers_a_real_provider_key() {
    // The live session filter catches the provider bearer token — raw and
    // re-encoded — so a tool result or summary echoing it is redacted.
    let f = session_disclosure_filter(Some("sk-live-9f3a2b7c1d"));
    assert!(f.leaks("Authorization: Bearer sk-live-9f3a2b7c1d"));
    assert!(f.leaks(&format!("k={}", b64("sk-live-9f3a2b7c1d"))));
    assert!(f.redact("token=sk-live-9f3a2b7c1d").contains("[REDACTED]"));
}

#[test]
fn session_filter_ignores_trivial_or_absent_key() {
    // No key → inert (bit-for-bit safe to wire everywhere).
    assert!(!session_disclosure_filter(None).leaks("anything at all"));
    // A short/placeholder value is NOT registered — registering it would
    // over-redact benign text.
    assert!(!session_disclosure_filter(Some("x")).leaks("x marks the spot"));
}

#[test]
fn session_tls_redacts_installed_secret_and_restores() {
    // No filter installed on this thread → identity.
    assert_eq!(
        redact_session_ingress("plain sk-live-abc12345"),
        "plain sk-live-abc12345"
    );
    {
        let mut f = DisclosureFilter::new();
        f.register("sk-live-abc12345");
        let _g = scoped_session_disclosure(f);
        // Installed → the registered secret (raw + re-encoded) is redacted on
        // ANY model-ingress path that consults the TLS.
        assert!(!redact_session_ingress("token=sk-live-abc12345").contains("sk-live-abc12345"));
        let enc = b64("sk-live-abc12345");
        assert!(!redact_session_ingress(&format!("b={enc}")).contains(&enc));
    }
    // Guard dropped → restored to identity (no leak of the guard across turns).
    assert_eq!(
        redact_session_ingress("token=sk-live-abc12345"),
        "token=sk-live-abc12345"
    );
}

#[test]
fn catches_raw_value() {
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    assert!(f.leaks("the token is CANARY-7f3a9c2b right here"));
    assert!(!f.leaks("nothing secret in this text"));
}

#[test]
fn catches_base64_reencoding() {
    // The key property: re-encoding defeats a SHAPE filter, not a VALUE filter.
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    let leaked = format!("here is {} encoded", b64("CANARY-7f3a9c2b"));
    assert!(f.leaks(&leaked), "base64 re-encoding must still be caught");
}

#[test]
fn catches_hex_reencoding() {
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    assert!(f.leaks(&format!("payload={}", hexs("CANARY-7f3a9c2b"))));
}

#[test]
fn redacts_all_forms() {
    let mut f = DisclosureFilter::new();
    f.register("SECRETVAL-abc123");
    let text = format!(
        "raw=SECRETVAL-abc123 b64={} hex={}",
        b64("SECRETVAL-abc123"),
        hexs("SECRETVAL-abc123")
    );
    let red = f.redact(&text);
    assert!(!f.leaks(&red), "redacted text must not leak");
    assert!(red.contains("[REDACTED]"));
}

#[test]
fn value_filter_not_shape_filter() {
    // An UNREGISTERED token-shaped string is NOT flagged — we filter by known
    // value, not "looks like a secret". (The deliberate threat-model choice.)
    let f = DisclosureFilter::new();
    assert!(!f.leaks("AKIAIOSFODNN7EXAMPLE looks like a key but isn't registered"));
}

#[test]
fn empty_registration_is_ignored() {
    let mut f = DisclosureFilter::new();
    f.register("");
    assert!(!f.leaks("anything at all"));
}

// ── Full re-encoding matrix (mandate: raw, base64/base64url, hex, escaped,
//    URL-encoded, chunk-split) ─────────────────────────────────────────────

use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};

/// A secret whose bytes force `+`/`/` in standard base64, so its base64url
/// form genuinely differs (the `-`/`_` alphabet). The non-ASCII tail
/// guarantees it.
const B64_DISTINCT: &str = "canary-\u{00ff}\u{00fe}\u{00fd}";

#[test]
fn catches_base64url_reencoding() {
    let mut f = DisclosureFilter::new();
    f.register(B64_DISTINCT);
    let url = URL_SAFE.encode(B64_DISTINCT.as_bytes());
    let std = base64::engine::general_purpose::STANDARD.encode(B64_DISTINCT.as_bytes());
    assert_ne!(
        url, std,
        "test secret must distinguish base64 from base64url"
    );
    assert!(
        f.leaks(&format!("payload={url}")),
        "base64url must be caught"
    );
    assert!(f.leaks(&format!(
        "payload={}",
        URL_SAFE_NO_PAD.encode(B64_DISTINCT.as_bytes())
    )));
}

#[test]
fn catches_base64_nopad_reencoding() {
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    assert!(f.leaks(&format!(
        "b={}",
        STANDARD_NO_PAD.encode("CANARY-7f3a9c2b".as_bytes())
    )));
}

#[test]
fn catches_uppercase_hex() {
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    let upper: String = "CANARY-7f3a9c2b"
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect();
    assert!(f.leaks(&format!("HX={upper}")));
}

#[test]
fn catches_percent_encoding() {
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    let pct: String = "CANARY-7f3a9c2b"
        .as_bytes()
        .iter()
        .map(|b| format!("%{b:02X}"))
        .collect();
    assert!(
        f.leaks(&format!("q={pct}")),
        "URL/percent-encoding must be caught"
    );
}

#[test]
fn catches_string_escapes() {
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    let esc_x: String = "CANARY-7f3a9c2b"
        .as_bytes()
        .iter()
        .map(|b| format!("\\x{b:02x}"))
        .collect();
    let esc_u: String = "CANARY-7f3a9c2b"
        .as_bytes()
        .iter()
        .map(|b| format!("\\u{b:04x}"))
        .collect();
    assert!(
        f.leaks(&format!("s=\"{esc_x}\"")),
        "\\xXX escape must be caught"
    );
    assert!(
        f.leaks(&format!("s=\"{esc_u}\"")),
        "\\uXXXX escape must be caught"
    );
}

#[test]
fn catches_chunk_split_raw() {
    // The secret broken across whitespace (newline/space) — a shape a model
    // uses to slip a value past a naive contiguous scan.
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    assert!(f.leaks("prefix CANARY-\n7f3a9c2b suffix"));
    assert!(f.leaks("C A N A R Y - 7 f 3 a 9 c 2 b"));
}

#[test]
fn catches_chunk_split_base64() {
    // A line-wrapped base64 blob (the classic MIME 76-col wrap) must still
    // be caught once whitespace is normalised.
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    let b64 = base64::engine::general_purpose::STANDARD.encode("CANARY-7f3a9c2b".as_bytes());
    let mid = b64.len() / 2;
    let wrapped = format!("{}\n{}", &b64[..mid], &b64[mid..]);
    assert!(f.leaks(&wrapped), "line-wrapped base64 must be caught");
}

#[test]
fn redact_withholds_chunk_split() {
    // Chunk-split can't be excised inline; redact must fail closed by
    // withholding the whole text, so the post-condition holds for split too.
    let mut f = DisclosureFilter::new();
    f.register("CANARY-7f3a9c2b");
    let split = "leak: CANARY-\n7f3a9c2b done";
    let red = f.redact(split);
    assert!(!f.leaks(&red), "redacted split text must not leak");
    assert!(
        red.contains("withheld"),
        "split redaction withholds wholesale"
    );
}

#[test]
fn redact_post_condition_holds_for_every_form() {
    // The invariant that makes redact safe to forward: its output never
    // leaks, across the whole encoding matrix + a chunk split.
    let mut f = DisclosureFilter::new();
    f.register("SECRETVAL-abc123");
    let s = "SECRETVAL-abc123";
    let b64 = base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    let hex: String = s.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let pct: String = s.as_bytes().iter().map(|b| format!("%{b:02X}")).collect();
    let text = format!("raw={s} b64={b64} hex={hex} pct={pct} split=SEC\nRETVAL-abc123");
    assert!(!f.leaks(&f.redact(&text)), "redact output must never leak");
}

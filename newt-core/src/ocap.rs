//! OCAP enforcement scaffold — the *runtime* side of the deviation ratchet.
//!
//! `docs/security/ocap-deviations.md` defines the rule:
//!
//! > effective authority = meet( the human's grant , what the currently-verified
//! > invariants can actually enforce ).
//!
//! A dangerous capability is available **iff** all its required OCAP invariants
//! *verify*; otherwise it is **fail-closed OFF**, with honest evidence. A
//! *deviation* is an invariant currently **absent** (unbuilt). This module is the
//! runtime checker plus the fail-closed capability gates the register names
//! (`verify_b1`, `seed_live_credential`, …). CI's `just ocap-check`
//! (`scripts/ocap_check.py`) statically asserts that every `OCAP-DANGER:<id>`
//! site carries its `OCAP-GATE:<id>` while the deviation is open — so these gates
//! cannot be removed without turning the build red.
//!
//! Everything here is **fail-closed**: the verifiers return [`Verification::Absent`]
//! until the real OS-isolation / disclosure-filter / broker code lands, so the
//! dangerous paths are structurally unreachable — bounded *by construction*, not by
//! discipline. See `docs/design/ocap-enforcement.md` for the architecture.

use std::fmt;

/// The result of checking one OCAP invariant at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// The invariant is enforced; `evidence` records how it was confirmed.
    Verified { evidence: String },
    /// The invariant is not yet enforced (an open deviation). Dependent
    /// capabilities stay fail-closed; `reason` is the honest "why".
    Absent {
        deviation: &'static str,
        reason: String,
    },
}

impl Verification {
    /// True only when the invariant is actually enforced.
    #[must_use]
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    /// The deviation id when absent (for honest banners / the ledger).
    #[must_use]
    pub fn deviation(&self) -> Option<&'static str> {
        match self {
            Self::Absent { deviation, .. } => Some(deviation),
            Self::Verified { .. } => None,
        }
    }
}

/// Refusal of a dangerous capability because a required invariant is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailClosed {
    pub deviation: &'static str,
    pub reason: String,
}

impl fmt::Display for FailClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refused (fail-closed): OCAP invariant '{}' is not enforced — {}",
            self.deviation, self.reason
        )
    }
}

impl std::error::Error for FailClosed {}

/// Require an invariant before proceeding; the fail-closed gate primitive.
fn require(v: Verification) -> Result<(), FailClosed> {
    match v {
        Verification::Verified { .. } => Ok(()),
        Verification::Absent { deviation, reason } => Err(FailClosed { deviation, reason }),
    }
}

/// Verify **b1-os-isolation**: uid-namespace + Landlock fs + seccomp +
/// default-deny netns + an egress proxy that is the *only* egress.
///
/// UNBUILT — always [`Verification::Absent`] (`sandbox_kind = none`; the
/// in-process monitor is the only barrier). When the per-OS stack lands (Linux
/// Landlock-net 6.7 / seccomp / netns, macOS Seatbelt, Windows AppContainer —
/// `docs/design/captured-shell-cross-platform.md`), this returns `Verified` with
/// the confirmed floor, re-run *per session* (no COW-cloned-pod skip).
#[must_use]
pub fn verify_b1() -> Verification {
    Verification::Absent {
        deviation: "b1-os-isolation",
        reason: "no OS sandbox or egress proxy; the in-process monitor is the only barrier".into(),
    }
}

/// Verify **disclosure-gate-live-path**: every tool result passes a single
/// disclosure filter before it is pushed into `messages` (one chokepoint).
///
/// Still [`Verification::Absent`] — but the mechanism now exists. step-6.1a wired
/// the by-value [`DisclosureFilter`] into the SINGLE live tool-result chokepoint
/// (`maybe_offload_tool_result`), with the canary ratchet guard proving it redacts
/// a registered value in any encoding. This stays `Absent` until (a) the caller
/// registers the session secret into `ChatCtx.disclosure` at session start, and
/// (b) the next-turn observation + summary paths converge on the same value filter
/// (today shape-only). Then a canary seeded at session start is provably absent
/// from the model-facing stream and this returns `Verified`.
#[must_use]
pub fn verify_disclosure_gate() -> Verification {
    Verification::Absent {
        deviation: "disclosure-gate-live-path",
        reason: "live chokepoint gated by-value (step-6.1a); session-start \
                 registration + observation/summary convergence pending"
            .into(),
    }
}

/// A live, scoped credential to seed (the `pa login` use case): a short-lived
/// token a broker would present to outbound requests. The token VALUE is
/// deliberately not modelled here — the design keeps it *out of the box* (the
/// worker/model never sees it); only a non-secret `label` is carried for the
/// ledger.
#[derive(Debug, Clone)]
pub struct ScopedCredential {
    pub label: String,
}

/// Seed a live scoped credential into the agent's environment (`pa login`).
///
/// DANGEROUS: a live token with no OS sandbox is a direct token→internet
/// exfiltration path the instant the in-process monitor is bypassed, and the
/// token could surface to the model on the un-gated disclosure path. Per the
/// register it is **disabled while `b1-os-isolation` / `disclosure-gate-live-path`
/// are open**. Fail-closed: refuses unless both verify.
pub fn seed_live_credential(cred: &ScopedCredential) -> Result<(), FailClosed> {
    // OCAP-DANGER: b1-os-isolation — a live token with no OS sandbox is exfil-ready.
    // OCAP-GATE: b1-os-isolation
    require(verify_b1())?;
    // OCAP-DANGER: disclosure-gate-live-path — the token could reach the model raw.
    // OCAP-GATE: disclosure-gate-live-path
    require(verify_disclosure_gate())?;
    // (unreachable today) Both invariants verified: a broker now holds `cred` out
    // of the box and presents it to outbound requests; the value never enters the
    // model-facing environment.
    let _ = cred;
    Ok(())
}

/// Admit a genuinely-untrusted / foreign remote voice that may hold anything
/// sensitive (a future remote swarm peer).
///
/// DANGEROUS without the OS sandbox: a hostile voice with no containment can
/// escalate. **Disabled while `b1-os-isolation` is open.** Fail-closed.
pub fn admit_untrusted_remote(voice_fingerprint: &str) -> Result<(), FailClosed> {
    // OCAP-DANGER: b1-os-isolation — an untrusted voice needs OS containment.
    // OCAP-GATE: b1-os-isolation
    require(verify_b1())?;
    let _ = voice_fingerprint;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifiers_are_absent_until_built() {
        assert!(!verify_b1().is_verified());
        assert_eq!(verify_b1().deviation(), Some("b1-os-isolation"));
        assert!(!verify_disclosure_gate().is_verified());
        assert_eq!(
            verify_disclosure_gate().deviation(),
            Some("disclosure-gate-live-path")
        );
    }

    #[test]
    fn verified_reports_no_deviation() {
        let v = Verification::Verified {
            evidence: "synthetic".into(),
        };
        assert!(v.is_verified());
        assert_eq!(v.deviation(), None);
    }

    #[test]
    fn seed_live_credential_fails_closed_on_b1() {
        let cred = ScopedCredential {
            label: "pa-token".into(),
        };
        let err = seed_live_credential(&cred).unwrap_err();
        assert_eq!(err.deviation, "b1-os-isolation");
        assert!(err.to_string().contains("fail-closed"));
    }

    #[test]
    fn admit_untrusted_remote_fails_closed() {
        let err = admit_untrusted_remote("SHA256:deadbeef").unwrap_err();
        assert_eq!(err.deviation, "b1-os-isolation");
    }

    #[test]
    fn require_passes_only_when_verified() {
        assert!(require(Verification::Verified {
            evidence: "ok".into()
        })
        .is_ok());
        assert!(require(verify_b1()).is_err());
    }
}

// ===========================================================================
// Disclosure filter — the by-VALUE redaction primitive for
// `disclosure-gate-live-path` (docs/design/ocap-enforcement.md §3).
//
// Threat-model finding: filter by KNOWN VALUE, not by shape. A shape filter
// ("looks like a token") both over-blocks and is **defeated by re-encoding**; a
// value filter catches the registered secret's actual bytes in any common
// encoding. This is the mechanism `verify_disclosure_gate` will assert on the
// live tool-result path — the canary: a value seeded at session start must never
// reach a model-facing message, in ANY encoding. (Wiring it into the live
// `messages` chokepoint lives in the agentic loop — a follow-up, kept out of
// this module to avoid colliding with concurrent work there.)
// ===========================================================================

use base64::Engine as _;

/// Redacts known secret VALUES — and their common re-encodings — from text
/// before it reaches the model. Register the live token / session canary;
/// [`leaks`](Self::leaks) detects and [`redact`](Self::redact) removes it.
#[derive(Debug, Default, Clone)]
pub struct DisclosureFilter {
    secrets: Vec<String>,
}

impl DisclosureFilter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a secret value to catch (raw + re-encoded). Empty values are
    /// ignored. Secrets should be high-entropy (tokens/canaries) — a value
    /// filter trusts the caller not to register common substrings.
    pub fn register(&mut self, secret: impl Into<String>) {
        let s = secret.into();
        if !s.is_empty() {
            self.secrets.push(s);
        }
    }

    /// The concrete forms of `secret` we match: the raw value plus its common
    /// re-encodings — base64 (standard + url-safe, padded + unpadded), hex
    /// (lower + upper), percent-encoding (upper + lower), and the `\xXX` /
    /// `\uXXXX` string escapes. A model bent on exfiltration re-encodes; a
    /// VALUE filter follows the value through each transform rather than
    /// guessing at a shape. Deduped — short secrets collide across some forms.
    ///
    /// The `\uXXXX` form escapes per byte (`\u00XX`), which matches a real JSON
    /// escape only for ASCII secrets; non-ASCII tokens are still caught by the
    /// raw / base64 / hex forms.
    fn encodings(secret: &str) -> Vec<String> {
        use base64::engine::general_purpose::{
            STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
        };
        let bytes = secret.as_bytes();
        let hex_lower: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let hex_upper: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
        let pct_upper: String = bytes.iter().map(|b| format!("%{b:02X}")).collect();
        let pct_lower: String = bytes.iter().map(|b| format!("%{b:02x}")).collect();
        let esc_x: String = bytes.iter().map(|b| format!("\\x{b:02x}")).collect();
        let esc_u: String = bytes.iter().map(|b| format!("\\u{b:04x}")).collect();
        let mut forms = vec![
            secret.to_string(),
            STANDARD.encode(bytes),
            STANDARD_NO_PAD.encode(bytes),
            URL_SAFE.encode(bytes),
            URL_SAFE_NO_PAD.encode(bytes),
            hex_lower,
            hex_upper,
            pct_upper,
            pct_lower,
            esc_x,
            esc_u,
        ];
        forms.sort();
        forms.dedup();
        forms
    }

    /// The text with all Unicode whitespace removed — the normalisation that
    /// defeats a **chunk-split** obfuscation, where a secret (or one of its
    /// encodings) is broken across whitespace: `CANARY\n-7f3a…`, a line-wrapped
    /// base64 blob, hex split every N chars. Scanning both the raw text and its
    /// whitespace-stripped form catches the split without a reflow pass.
    fn strip_ws(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// Does `text` disclose any registered secret, in any tracked encoding —
    /// including a **chunk-split** occurrence broken across whitespace? This is
    /// the authoritative gate decision; the live path withholds a result for
    /// which this returns `true`.
    #[must_use]
    pub fn leaks(&self, text: &str) -> bool {
        let normalized = Self::strip_ws(text);
        self.secrets.iter().any(|s| {
            Self::encodings(s)
                .iter()
                .any(|e| text.contains(e.as_str()) || normalized.contains(e.as_str()))
        })
    }

    /// Replace every occurrence of every registered secret (raw or re-encoded)
    /// with `[REDACTED]`. Contiguous forms are excised inline; a **chunk-split**
    /// occurrence can't be excised without reflowing the text, so if any secret
    /// still surfaces once whitespace is normalised away, the whole text is
    /// withheld (fail closed). The post-condition `!self.leaks(&self.redact(t))`
    /// therefore holds for *every* input, contiguous or split.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.secrets {
            for enc in Self::encodings(s) {
                out = out.replace(enc.as_str(), "[REDACTED]");
            }
        }
        if self.leaks(&out) {
            return "[REDACTED: withheld — disclosed a registered secret]".to_string();
        }
        out
    }
}

#[cfg(test)]
mod disclosure_tests {
    use super::*;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }
    fn hexs(s: &str) -> String {
        s.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
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
}

// ===========================================================================
// Separation of duties — `sod-proposer-not-worker`
// (docs/security/ocap-deviations.md §sod, docs/design/ocap-enforcement.md §5).
//
// The policy-proposing surface must be a cryptographically DISTINCT, more-trusted
// identity than the confined worker — otherwise observe-then-propose lets the
// worker author its own ceiling (privilege escalation by self-proposal). The
// distinctness half is checkable now (`proposer_distinct`); the taint-aware
// observe-then-propose half is UNBUILT, so `verify_sod` stays Absent
// (fail-closed) and `auto_apply_policy` refuses regardless.
// ===========================================================================

/// The distinctness primitive: a non-empty proposer fingerprint different from
/// the worker's. **Necessary, not sufficient**, for separation of duties.
#[must_use]
pub fn proposer_distinct(proposer_fp: &str, worker_fp: &str) -> bool {
    !proposer_fp.is_empty() && proposer_fp != worker_fp
}

/// Verify **sod-proposer-not-worker**: a distinct, more-trusted proposer key
/// (`proposer_fp != worker_fp`) AND taint-aware observe-then-propose. The
/// distinctness half is checked here; the taint-aware half is unbuilt, so this
/// stays [`Verification::Absent`] — but the `reason` reports the distinctness
/// state so the ledger is honest. Flips to `Verified` when taint-awareness lands.
#[must_use]
pub fn verify_sod(proposer_fp: &str, worker_fp: &str) -> Verification {
    if !proposer_distinct(proposer_fp, worker_fp) {
        return Verification::Absent {
            deviation: "sod-proposer-not-worker",
            reason: "proposer key is not distinct from the worker (self-proposal)".into(),
        };
    }
    Verification::Absent {
        deviation: "sod-proposer-not-worker",
        reason: "distinct proposer key confirmed, but taint-aware observe-then-propose is unbuilt"
            .into(),
    }
}

/// Auto-apply a proposed policy (lower/raise a worker's `Caveats` without a human).
///
/// DANGEROUS: with no separation of duties this is privilege escalation by
/// self-proposal. **Disabled while `sod-proposer-not-worker` is open** — every
/// promotion needs a human approval bound to the lowered-`Caveats` hash. Fail-closed.
pub fn auto_apply_policy(proposer_fp: &str, worker_fp: &str) -> Result<(), FailClosed> {
    // OCAP-DANGER: sod-proposer-not-worker — auto-apply enables self-proposal escalation.
    // OCAP-GATE: sod-proposer-not-worker
    require(verify_sod(proposer_fp, worker_fp))?;
    Ok(())
}

#[cfg(test)]
mod sod_tests {
    use super::*;

    #[test]
    fn distinctness_primitive() {
        assert!(proposer_distinct("SHA256:proposer", "SHA256:worker"));
        assert!(!proposer_distinct("SHA256:same", "SHA256:same"));
        assert!(
            !proposer_distinct("", "SHA256:worker"),
            "empty proposer is not distinct"
        );
    }

    #[test]
    fn verify_sod_is_absent_until_taint_aware() {
        // Even with distinct keys, sod stays open (taint-aware half unbuilt).
        let v = verify_sod("SHA256:proposer", "SHA256:worker");
        assert!(!v.is_verified());
        assert_eq!(v.deviation(), Some("sod-proposer-not-worker"));
        // Self-proposal reports the distinctness failure specifically.
        let self_prop = verify_sod("SHA256:same", "SHA256:same");
        assert!(
            matches!(self_prop, Verification::Absent { reason, .. } if reason.contains("self-proposal"))
        );
    }

    #[test]
    fn auto_apply_fails_closed_both_ways() {
        assert!(matches!(
            auto_apply_policy("SHA256:p", "SHA256:w"),
            Err(FailClosed {
                deviation: "sod-proposer-not-worker",
                ..
            })
        ));
        assert!(auto_apply_policy("SHA256:same", "SHA256:same").is_err());
    }
}

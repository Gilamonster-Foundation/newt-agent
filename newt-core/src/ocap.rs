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
/// UNBUILT — always [`Verification::Absent`] (today redaction runs only on the
/// next-turn observation and is shape-only). When the single chokepoint lands and
/// a canary seeded at session start never appears in the model-facing stream,
/// this returns `Verified`.
#[must_use]
pub fn verify_disclosure_gate() -> Verification {
    Verification::Absent {
        deviation: "disclosure-gate-live-path",
        reason: "no single disclosure chokepoint on the live tool-result path".into(),
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

    /// The forms of `secret` we match: raw, standard base64, and lowercase hex.
    fn encodings(secret: &str) -> [String; 3] {
        let bytes = secret.as_bytes();
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        [secret.to_string(), b64, hex]
    }

    /// Does `text` disclose any registered secret, raw or re-encoded?
    #[must_use]
    pub fn leaks(&self, text: &str) -> bool {
        self.secrets
            .iter()
            .any(|s| Self::encodings(s).iter().any(|e| text.contains(e.as_str())))
    }

    /// Replace every occurrence of every registered secret (raw or re-encoded)
    /// with `[REDACTED]`.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.secrets {
            for enc in Self::encodings(s) {
                out = out.replace(enc.as_str(), "[REDACTED]");
            }
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
}

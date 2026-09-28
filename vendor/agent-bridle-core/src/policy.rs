//! **OCAP durable-policy schema** — the shared contract for per-verdict
//! permission policy (agent-bridle #220; newt-agent #1126 track O).
//!
//! The accumulation loop: an agent harness prompts for a capability, the human
//! decides, and a *durable* decision is stored as **data** the whole fleet can
//! honor — the leash loosens with use instead of staying naggy or getting
//! bypassed. This module defines only the SCHEMA + pure evaluation: types,
//! parse/serialize, merge, and the precedence law. No enforcement changes here;
//! stores (e.g. newt's `~/.newt/ocap/`) implement against it.
//!
//! ## The file shape
//!
//! One TOML file per verdict — `approve.toml`, `deny.toml`, `ask.toml`,
//! `passkey.toml` — each holding entries per capability class:
//!
//! ```toml
//! # ~/.newt/ocap/approve.toml
//! [[exec]]
//! target = "cargo"
//! note = "build tooling"
//! granted = "2026-07-14"
//!
//! [[fs]]
//! path = "~/workspaces"
//! write = true
//!
//! [[net]]
//! host = "crates.io"
//! ```
//!
//! ## The precedence law (load-bearing)
//!
//! When a target matches entries in several verdict files, the MOST
//! RESTRICTIVE wins: **deny > passkey > ask > approve**. A durable deny can
//! never be shadowed by a durable approve; a passkey step-up outranks a plain
//! ask. Evaluation returns `None` when no durable policy matches — the harness
//! falls through to its interactive prompt (or its default-deny floor).
//!
//! ## Danger-tier invariant (part of the contract)
//!
//! High-danger targets (interpreter exec, broad filesystem roots — as judged by
//! the *consuming* harness's danger table) MUST NOT be durably approvable: a
//! store rejects writing such an entry into `approve.toml` (at most it may
//! offer `passkey.toml`). This module cannot see the harness's danger table,
//! so [`PolicySet::validate_approve`] takes the judgment as a predicate — the
//! contract is that stores CALL it before persisting an approve entry.
//!
//! ## Signed approve entries (tamper-resistance; newt-agent #1207, #226)
//!
//! The policy dir is plain TOML on disk — anything with write access could
//! forge an entry. The exposure is **asymmetric**: a forged `deny`/`ask` only
//! *narrows* (fail-safe, at worst a nuisance), but a forged `approve` *widens*
//! authority the human never granted. So the contract requires signatures on
//! the loosening verdict only:
//!
//! - Every `approve.toml` entry carries `sig` — a hex Ed25519 signature over
//!   that entry's canonical [`signing payload`](ExecEntry::signing_payload)
//!   (version tag + class + the authority-bearing fields; the human metadata
//!   `note`/`granted`/`by` is deliberately excluded so editing a note does not
//!   invalidate the grant).
//! - A store calls [`PolicyFile::verified_approves`] at load with its trusted
//!   verifier (the operator's root verifying key): entries with a missing or
//!   invalid `sig` are DROPPED, loudly — fail-closed to "no durable grant",
//!   i.e. the harness prompts. `deny`/`ask`/`passkey` files load unsigned.
//! - Verification is a pure seam ([`ApproveVerifier`]), mirroring the step-up
//!   [`crate::step_up::DischargeVerifier`] pattern: no ceremony, no IO. The
//!   production Ed25519 impl ([`Ed25519ApproveVerifier`]) is feature-gated
//!   behind `verifier-ed25519` (ADR 0007's dependency posture).
//!
//! **Known limit (documented, not solved):** deleting an entry does not revoke
//! its signature — an attacker who captured a previously-signed entry can
//! re-add it verbatim. Revocation today = rotating the signing key; a
//! monotonic counter / revocation list is future contract work.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The four durable verdicts — one policy file each. Ordered by restrictiveness
/// (most restrictive first); [`Verdict::precedence`] makes the law explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Never allowed; the prompt is not even offered. `deny.toml`.
    Deny,
    /// Allowed only after a WebAuthn/passkey step-up (presence proof).
    /// `passkey.toml` — schema now; enforcement lands with the presence anchor.
    Passkey,
    /// Always prompt, even if a broader approve would match. `ask.toml` pins
    /// a target to interactive judgment.
    Ask,
    /// Durably allowed without prompting. `approve.toml`.
    Approve,
}

impl Verdict {
    /// Lower = more restrictive = wins ties. deny(0) > passkey(1) > ask(2) >
    /// approve(3).
    pub fn precedence(self) -> u8 {
        match self {
            Self::Deny => 0,
            Self::Passkey => 1,
            Self::Ask => 2,
            Self::Approve => 3,
        }
    }

    /// The canonical policy filename for this verdict (`deny.toml`, …).
    pub fn filename(self) -> &'static str {
        match self {
            Self::Deny => "deny.toml",
            Self::Passkey => "passkey.toml",
            Self::Ask => "ask.toml",
            Self::Approve => "approve.toml",
        }
    }
}

/// An exec-capability entry: a command target (basename or absolute path,
/// matched by the consuming harness's own exec-matching rules).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecEntry {
    /// The command target — a basename (`cargo`) or absolute path.
    pub target: String,
    /// Free-form human note ("build tooling").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// ISO 8601 date/datetime the decision was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted: Option<String>,
    /// Who recorded it: `human` (interactive prompt), `seed` (a shipped
    /// common-settings profile), or a tool name. Free-form provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Hex Ed25519 signature over [`Self::signing_payload`] (#226). REQUIRED
    /// for `approve.toml` entries (unsigned approves are dropped at load by
    /// [`PolicyFile::verified_approves`]); ignored on the narrowing verdicts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl ExecEntry {
    /// The canonical bytes an approve signature covers: version tag, class,
    /// and the authority-bearing field. Human metadata (`note`/`granted`/`by`)
    /// is excluded on purpose — editing a note must not invalidate the grant.
    pub fn signing_payload(&self) -> Vec<u8> {
        format!("{SIGNING_DOMAIN}\nexec\n{}", self.target).into_bytes()
    }
}

/// A filesystem-capability entry: a path prefix (with `~` expansion left to
/// the consuming store) and whether writes are covered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsEntry {
    /// The covered path prefix (`~` expansion is the store's job).
    pub path: String,
    /// `false`/absent = read-only coverage; `true` covers writes too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub write: bool,
    /// Free-form human note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// ISO 8601 date/datetime the decision was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted: Option<String>,
    /// Who recorded it: `human`, `seed`, or a tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Hex Ed25519 signature over [`Self::signing_payload`] (#226) — see
    /// [`ExecEntry::sig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl FsEntry {
    /// Canonical signed bytes. `write` IS authority-bearing (a read grant must
    /// not be re-usable as a write grant), so it is in the payload.
    pub fn signing_payload(&self) -> Vec<u8> {
        format!("{SIGNING_DOMAIN}\nfs\n{}\nwrite={}", self.path, self.write).into_bytes()
    }
}

/// A network-capability entry: a host (exact or the consuming harness's
/// wildcard rules).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetEntry {
    /// The covered host (exact, or the harness's wildcard rules).
    pub host: String,
    /// Free-form human note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// ISO 8601 date/datetime the decision was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted: Option<String>,
    /// Who recorded it: `human`, `seed`, or a tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Hex Ed25519 signature over [`Self::signing_payload`] (#226) — see
    /// [`ExecEntry::sig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl NetEntry {
    /// Canonical signed bytes — see [`ExecEntry::signing_payload`].
    pub fn signing_payload(&self) -> Vec<u8> {
        format!("{SIGNING_DOMAIN}\nnet\n{}", self.host).into_bytes()
    }
}

/// Domain-separation tag for approve signatures (#226). Version-bumped if the
/// payload shape ever changes; the tag guarantees a policy signature can never
/// be confused with any other Ed25519 signature the same key makes (step-up
/// discharges, mesh envelopes, …).
pub const SIGNING_DOMAIN: &str = "agent-bridle:ocap-approve:v1";

/// Verifies one approve entry's signature over its canonical payload. **Pure**
/// — no ceremony, no IO (the [`crate::step_up::DischargeVerifier`] posture).
/// An `Err(reason)` drops the entry at load, fail-closed; the reason is safe
/// to surface to the operator.
pub trait ApproveVerifier {
    /// Accept iff `sig` (raw 64 bytes) is a valid signature over `payload` by
    /// the trusted signer.
    fn verify(&self, payload: &[u8], sig: &[u8]) -> Result<(), String>;
}

/// Production [`ApproveVerifier`] for Ed25519 (`verify_strict`) against ONE
/// trusted verifying key — the operator's root key, supplied by the consuming
/// store. Feature-gated behind `verifier-ed25519` like the step-up sibling
/// (ADR 0007): the contract itself stays dependency-free.
#[cfg(feature = "verifier-ed25519")]
#[derive(Debug, Clone, Copy)]
pub struct Ed25519ApproveVerifier {
    /// The trusted signer's 32-byte Ed25519 verifying key.
    pub verifying_key: [u8; 32],
}

#[cfg(feature = "verifier-ed25519")]
impl ApproveVerifier for Ed25519ApproveVerifier {
    fn verify(&self, payload: &[u8], sig: &[u8]) -> Result<(), String> {
        use ed25519_dalek::{Signature, VerifyingKey};
        let vk = VerifyingKey::from_bytes(&self.verifying_key).map_err(|e| e.to_string())?;
        let sig_bytes: [u8; 64] = sig
            .try_into()
            .map_err(|_| "signature is not 64 bytes".to_string())?;
        vk.verify_strict(payload, &Signature::from_bytes(&sig_bytes))
            .map_err(|e| e.to_string())
    }
}

/// Decode a lowercase/uppercase hex string (the `sig` wire encoding). A two-line
/// decoder beats a dependency for one field; `pub(crate)` so [`crate::operator`]
/// (the web-authz contract) shares it rather than forking a second copy.
pub(crate) fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("hex string has odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// Encode bytes as lowercase hex — the inverse of [`hex_decode`], for write
/// paths that mint signed entries.
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One verdict file's contents: entries per capability class. All classes
/// optional — an empty file is valid (and means "no durable policy of this
/// verdict"). `deny_unknown_fields` keeps typos loud, matching the house
/// model-card discipline.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    /// Exec-class entries (`[[exec]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec: Vec<ExecEntry>,
    /// Filesystem-class entries (`[[fs]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fs: Vec<FsEntry>,
    /// Network-class entries (`[[net]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net: Vec<NetEntry>,
}

impl PolicyFile {
    /// Parse one verdict file from TOML.
    pub fn parse(contents: &str) -> Result<Self, String> {
        toml::from_str(contents).map_err(|e| format!("ocap policy TOML: {e}"))
    }

    /// Serialize back to TOML (minimal — empty classes are skipped).
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string(self).map_err(|e| format!("ocap policy serialize: {e}"))
    }

    /// The fail-closed sanitizer for the LOOSENING verdict (#226): retain only
    /// entries whose `sig` verifies over their canonical payload; drop the rest
    /// with a loud reason each. Stores MUST run `approve.toml` through this at
    /// load — an unsigned or tampered approve degrades to "no durable grant"
    /// (the harness prompts), never to granted authority. The narrowing
    /// verdicts (`deny`/`ask`/`passkey`) are NOT sanitized: forging them can
    /// only narrow, and dropping a deny over a bad signature would *widen* —
    /// exactly backwards.
    pub fn verified_approves(&self, verifier: &dyn ApproveVerifier) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut check = |what: &str, payload: Vec<u8>, sig: &Option<String>| -> bool {
            let verdict = match sig {
                None => Err("missing `sig` (unsigned approve)".to_string()),
                Some(s) => hex_decode(s).and_then(|raw| verifier.verify(&payload, &raw)),
            };
            match verdict {
                Ok(()) => true,
                Err(reason) => {
                    warnings.push(format!("approve entry `{what}` dropped: {reason}"));
                    false
                }
            }
        };
        let kept = Self {
            exec: self
                .exec
                .iter()
                .filter(|e| check(&e.target, e.signing_payload(), &e.sig))
                .cloned()
                .collect(),
            fs: self
                .fs
                .iter()
                .filter(|e| check(&e.path, e.signing_payload(), &e.sig))
                .cloned()
                .collect(),
            net: self
                .net
                .iter()
                .filter(|e| check(&e.host, e.signing_payload(), &e.sig))
                .cloned()
                .collect(),
        };
        (kept, warnings)
    }
}

/// The capability classes durable policy can cover. Kept closed and small on
/// purpose — a new class is a contract rev, not a stringly-typed extension
/// (unlike knob maps, a *permission* surface must be enumerable to audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityClass {
    /// Command execution.
    Exec,
    /// Filesystem access.
    Fs,
    /// Network access.
    Net,
}

/// A full policy set: the four verdict files, loaded by a store. Evaluation
/// applies the precedence law across them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PolicySet {
    /// The loaded verdict files, keyed by verdict.
    pub files: BTreeMap<Verdict, PolicyFile>,
}

impl PolicySet {
    /// Look up the durable verdict for a target, applying the precedence law:
    /// the most restrictive matching verdict wins (deny > passkey > ask >
    /// approve). `None` = no durable policy — the harness falls through to its
    /// interactive prompt / default-deny floor.
    ///
    /// Matching here is EXACT (string equality on target/path/host); a
    /// consuming harness with richer matching (basenames, path prefixes,
    /// wildcards) normalizes the candidate before calling, or evaluates its
    /// own match and uses [`Verdict::precedence`] to combine.
    pub fn evaluate(&self, class: CapabilityClass, target: &str) -> Option<Verdict> {
        let mut best: Option<Verdict> = None;
        for (&verdict, file) in &self.files {
            let hit = match class {
                CapabilityClass::Exec => file.exec.iter().any(|e| e.target == target),
                CapabilityClass::Fs => file.fs.iter().any(|e| e.path == target),
                CapabilityClass::Net => file.net.iter().any(|e| e.host == target),
            };
            if hit && best.is_none_or(|b| verdict.precedence() < b.precedence()) {
                best = Some(verdict);
            }
        }
        best
    }

    /// The danger-tier invariant hook: a store MUST call this before
    /// persisting an entry into `approve.toml`, passing its own danger
    /// judgment. High-danger targets are never durably approvable — the
    /// contract offers `Passkey` as the strongest grant for them.
    pub fn validate_approve(
        class: CapabilityClass,
        target: &str,
        is_high_danger: impl Fn(CapabilityClass, &str) -> bool,
    ) -> Result<(), String> {
        if is_high_danger(class, target) {
            return Err(format!(
                "`{target}` is a high-danger target and cannot be durably approved \
                 (offer passkey step-up instead)"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_with(verdict: Verdict, file: PolicyFile) -> PolicySet {
        let mut s = PolicySet::default();
        s.files.insert(verdict, file);
        s
    }

    #[test]
    fn verdict_files_and_precedence_are_canonical() {
        assert_eq!(Verdict::Deny.filename(), "deny.toml");
        assert_eq!(Verdict::Approve.filename(), "approve.toml");
        // The law: deny > passkey > ask > approve.
        assert!(Verdict::Deny.precedence() < Verdict::Passkey.precedence());
        assert!(Verdict::Passkey.precedence() < Verdict::Ask.precedence());
        assert!(Verdict::Ask.precedence() < Verdict::Approve.precedence());
    }

    #[test]
    fn parses_the_documented_approve_file() {
        let f = PolicyFile::parse(
            "[[exec]]\ntarget = \"cargo\"\nnote = \"build tooling\"\ngranted = \"2026-07-14\"\n\n\
             [[fs]]\npath = \"~/workspaces\"\nwrite = true\n\n[[net]]\nhost = \"crates.io\"\n",
        )
        .unwrap();
        assert_eq!(f.exec[0].target, "cargo");
        assert!(f.fs[0].write);
        assert_eq!(f.net[0].host, "crates.io");
        // Round-trip stays minimal (no empty classes, no None provenance).
        let out = f.to_toml().unwrap();
        assert!(!out.contains("by ="), "unset provenance skipped: {out}");
    }

    #[test]
    fn empty_and_unknown_field_behavior() {
        // An empty file is valid — no durable policy of that verdict.
        assert_eq!(PolicyFile::parse("").unwrap(), PolicyFile::default());
        // A typo'd key is a loud error (deny_unknown_fields).
        assert!(PolicyFile::parse("[[exec]]\ntarget=\"x\"\nnotee=\"typo\"\n").is_err());
    }

    #[test]
    fn deny_shadows_approve_and_passkey_outranks_ask() {
        let mut s = PolicySet::default();
        s.files.insert(
            Verdict::Approve,
            PolicyFile::parse("[[exec]]\ntarget=\"rm\"\n").unwrap(),
        );
        s.files.insert(
            Verdict::Deny,
            PolicyFile::parse("[[exec]]\ntarget=\"rm\"\n").unwrap(),
        );
        // A durable deny can never be shadowed by a durable approve.
        assert_eq!(s.evaluate(CapabilityClass::Exec, "rm"), Some(Verdict::Deny));

        let mut s = PolicySet::default();
        s.files.insert(
            Verdict::Ask,
            PolicyFile::parse("[[net]]\nhost=\"api.example\"\n").unwrap(),
        );
        s.files.insert(
            Verdict::Passkey,
            PolicyFile::parse("[[net]]\nhost=\"api.example\"\n").unwrap(),
        );
        assert_eq!(
            s.evaluate(CapabilityClass::Net, "api.example"),
            Some(Verdict::Passkey)
        );
    }

    #[test]
    fn no_match_falls_through_to_the_interactive_floor() {
        let s = set_with(
            Verdict::Approve,
            PolicyFile::parse("[[exec]]\ntarget=\"cargo\"\n").unwrap(),
        );
        assert_eq!(s.evaluate(CapabilityClass::Exec, "python3"), None);
        assert_eq!(
            s.evaluate(CapabilityClass::Fs, "cargo"),
            None,
            "class-scoped"
        );
    }

    #[test]
    fn high_danger_targets_are_never_durably_approvable() {
        let judge = |class: CapabilityClass, target: &str| {
            class == CapabilityClass::Exec && (target == "bash" || target == "python3")
        };
        assert!(PolicySet::validate_approve(CapabilityClass::Exec, "bash", judge).is_err());
        assert!(PolicySet::validate_approve(CapabilityClass::Exec, "cargo", judge).is_ok());
        assert!(PolicySet::validate_approve(CapabilityClass::Net, "bash", judge).is_ok());
    }

    /// #226: the payload is version-tagged, class-scoped, includes the
    /// authority bits (fs `write`), and EXCLUDES the human metadata — editing a
    /// note must not invalidate a grant; a read grant must not re-sign as write.
    #[test]
    fn signing_payloads_are_canonical_and_authority_scoped() {
        let e = ExecEntry {
            target: "cargo".into(),
            note: Some("anything".into()),
            ..Default::default()
        };
        assert_eq!(
            e.signing_payload(),
            b"agent-bridle:ocap-approve:v1\nexec\ncargo"
        );
        let noteless = ExecEntry {
            target: "cargo".into(),
            ..Default::default()
        };
        assert_eq!(e.signing_payload(), noteless.signing_payload());

        let read = FsEntry {
            path: "/ws".into(),
            ..Default::default()
        };
        let write = FsEntry {
            path: "/ws".into(),
            write: true,
            ..Default::default()
        };
        assert_ne!(
            read.signing_payload(),
            write.signing_payload(),
            "write is authority-bearing"
        );
        // Class separation: same target string, different class, different bytes.
        let net = NetEntry {
            host: "cargo".into(),
            ..Default::default()
        };
        assert_ne!(noteless.signing_payload(), net.signing_payload());
    }

    /// A verifier scripted by signature value — the pure-seam test double.
    struct SigEquals(&'static [u8]);
    impl ApproveVerifier for SigEquals {
        fn verify(&self, _payload: &[u8], sig: &[u8]) -> Result<(), String> {
            (sig == self.0).then_some(()).ok_or("bad signature".into())
        }
    }

    /// #226 fail-closed law: unsigned and bad-sig approve entries are DROPPED
    /// with loud reasons; valid ones survive; `sig` round-trips through TOML.
    #[test]
    fn verified_approves_drops_unsigned_and_invalid_fail_closed() {
        let file = PolicyFile::parse(
            "[[exec]]\ntarget=\"cargo\"\nsig=\"0a0b\"\n\
             [[exec]]\ntarget=\"git\"\n\
             [[net]]\nhost=\"crates.io\"\nsig=\"ff\"\n",
        )
        .unwrap();
        let (kept, warnings) = file.verified_approves(&SigEquals(&[0x0a, 0x0b]));
        assert_eq!(kept.exec.len(), 1, "only the valid-sig entry survives");
        assert_eq!(kept.exec[0].target, "cargo");
        assert!(kept.net.is_empty(), "bad sig dropped");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings
            .iter()
            .any(|w| w.contains("git") && w.contains("unsigned")));
        assert!(warnings.iter().any(|w| w.contains("crates.io")));
        // The kept entry keeps its signature through a serialize round-trip.
        let toml = kept.to_toml().unwrap();
        assert!(toml.contains("sig = \"0a0b\""), "{toml}");
    }

    #[test]
    fn hex_codec_round_trips_and_rejects_odd_length() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x1a]), "00ff1a");
        assert_eq!(hex_decode("00ff1a").unwrap(), vec![0x00, 0xff, 0x1a]);
        assert_eq!(hex_decode("00FF1A").unwrap(), vec![0x00, 0xff, 0x1a]);
        assert!(hex_decode("abc").is_err());
        assert!(hex_decode("zz").is_err());
    }

    /// #226 end-to-end with REAL Ed25519 (feature-gated like the step-up
    /// verifier tests): sign the canonical payload, verify at load; a tampered
    /// target (signature replayed onto a different entry) is dropped.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn ed25519_sign_and_verify_roundtrip_rejects_tamper() {
        use ed25519_dalek::{Signer, SigningKey};
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let verifier = Ed25519ApproveVerifier {
            verifying_key: sk.verifying_key().to_bytes(),
        };

        let mut entry = ExecEntry {
            target: "cargo".into(),
            ..Default::default()
        };
        entry.sig = Some(hex_encode(&sk.sign(&entry.signing_payload()).to_bytes()));

        // The signed grant survives…
        let file = PolicyFile {
            exec: vec![entry.clone()],
            ..Default::default()
        };
        let (kept, warnings) = file.verified_approves(&verifier);
        assert_eq!(kept.exec.len(), 1);
        assert!(warnings.is_empty(), "{warnings:?}");

        // …a tampered target (old signature, new authority) is dropped…
        let mut forged = entry.clone();
        forged.target = "rm".into();
        let file = PolicyFile {
            exec: vec![forged],
            ..Default::default()
        };
        let (kept, warnings) = file.verified_approves(&verifier);
        assert!(
            kept.exec.is_empty(),
            "replayed sig must not carry to a new target"
        );
        assert_eq!(warnings.len(), 1);

        // …and a different signer's grant is dropped (single-root trust).
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let mut foreign = ExecEntry {
            target: "cargo".into(),
            ..Default::default()
        };
        foreign.sig = Some(hex_encode(
            &other.sign(&foreign.signing_payload()).to_bytes(),
        ));
        let file = PolicyFile {
            exec: vec![foreign],
            ..Default::default()
        };
        let (kept, _) = file.verified_approves(&verifier);
        assert!(kept.exec.is_empty());
    }
}

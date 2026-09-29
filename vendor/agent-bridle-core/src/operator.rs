//! Operator-authz contract — the shared types that bind an *authenticated
//! human operator* to a *permission decision* (newt-agent #1354, the "web-authz
//! triangle", from the PR #1353 security audit).
//!
//! The shape is: an identity provider identifies the human ([`HumanPrincipal`]),
//! that human is bound to a mesh agent ([`PrincipalBinding`]), the agent issues
//! a signed, expiring request for a decision ([`PermissionChallenge`]), and the
//! operator answers with an [`OperatorDecision`] that names a verdict the agent
//! then mints. bridle owns the contract; newt implements the first store behind
//! it (locked-decision #8).
//!
//! Two disciplines this module holds to:
//! * **Reuse, don't reinvent crypto.** Signatures verify through the existing
//!   [`crate::policy::ApproveVerifier`] seam (Ed25519 concrete impl feature-gated
//!   behind `verifier-ed25519`), and the agent fingerprint is
//!   `agent_mesh_protocol::Fingerprint`. No new signer/verifier trait.
//! * **Wall-clock-free.** A challenge carries an absolute deadline, but the
//!   expiry *predicate* takes `now` as a parameter ([`PermissionChallenge::is_expired`]),
//!   so this crate never reads a clock (the `step_up` causal-freshness posture).
//!
//! Multi-field `signing_payload`s are **length-delimited**, not newline-joined
//! like the single-field `policy` entries: with more than one variable field, a
//! newline join lets a value forge a field boundary (`sub = "x\nagent=…"`).
//! Length prefixes make that unrepresentable.

use crate::policy::{ApproveVerifier, CapabilityClass, Verdict};
use agent_mesh_protocol::Fingerprint;
use serde::{Deserialize, Serialize};

/// Domain tag for a [`PrincipalBinding`] signature — kept distinct from every
/// other Ed25519 signature the same key makes.
const BINDING_DOMAIN: &[u8] = b"agent-bridle:operator:principal-binding:v1";
/// Domain tag for a [`PermissionChallenge`] signature.
const CHALLENGE_DOMAIN: &[u8] = b"agent-bridle:operator:permission-challenge:v1";

/// Append `field` to `buf` length-delimited (`u64` LE length ‖ bytes), so no two
/// distinct field tuples ever share a payload.
fn push_field(buf: &mut Vec<u8>, field: &[u8]) {
    buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
    buf.extend_from_slice(field);
}

/// Decode a hex signature and verify it over `payload`, fail-closed. `None`
/// (unsigned) is an error, never a pass.
fn verify_hex_sig(
    payload: &[u8],
    sig: &Option<String>,
    verifier: &dyn ApproveVerifier,
) -> Result<(), String> {
    let hex = sig.as_ref().ok_or_else(|| "unsigned".to_string())?;
    let raw = crate::policy::hex_decode(hex)?;
    verifier.verify(payload, &raw)
}

/// An authenticated human identity, keyed by OIDC `(issuer, subject)` — **never**
/// email. `sub` is the stable per-issuer account id; email is mutable and
/// display-only. Groups carry coarse authorization asserted by the IdP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanPrincipal {
    /// OIDC issuer (`iss`) — the identity provider that vouched for this human.
    pub issuer: String,
    /// OIDC subject (`sub`) — the stable identity key within the issuer.
    pub subject: String,
    /// Display-only email. MUST NOT be used as the identity key (email changes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Coarse authorization groups asserted by the IdP.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}

impl HumanPrincipal {
    /// The identity key — `(issuer, subject)`, never email.
    pub fn key(&self) -> (&str, &str) {
        (self.issuer.as_str(), self.subject.as_str())
    }

    /// Whether the IdP asserted membership in `group`.
    pub fn is_member_of(&self, group: &str) -> bool {
        self.groups.iter().any(|g| g == group)
    }
}

/// A signed binding of a [`HumanPrincipal`] to the mesh agent it authorizes —
/// the "who ↔ which agent" edge of the triangle. The agent signs
/// [`Self::signing_payload`] with its mesh `AgentKey`; verification reuses the
/// [`ApproveVerifier`] seam.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalBinding {
    /// The authenticated human this binding is about.
    pub human: HumanPrincipal,
    /// The mesh agent this human is bound to control.
    pub mesh_agent_fingerprint: Fingerprint,
    /// Causal freshness — the mesh generation at issue (NOT wall-clock).
    pub issued_generation: u64,
    /// Agent's Ed25519 signature over [`Self::signing_payload`], hex-encoded.
    /// `None` until signed; an unsigned binding never verifies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl PrincipalBinding {
    /// A fresh, unsigned binding. Call [`Self::signed`] with the agent's
    /// signature over [`Self::signing_payload`] to complete it.
    pub fn new(
        human: HumanPrincipal,
        mesh_agent_fingerprint: Fingerprint,
        issued_generation: u64,
    ) -> Self {
        Self {
            human,
            mesh_agent_fingerprint,
            issued_generation,
            sig: None,
        }
    }

    /// The canonical, domain-separated, length-delimited bytes the agent signs.
    /// Groups/email are excluded — they are IdP assertions, not authority-bearing
    /// for the who↔agent edge (matching `policy`'s "sign the authority, not the
    /// metadata").
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(BINDING_DOMAIN);
        push_field(&mut p, self.human.issuer.as_bytes());
        push_field(&mut p, self.human.subject.as_bytes());
        push_field(&mut p, &self.mesh_agent_fingerprint.0);
        p.extend_from_slice(&self.issued_generation.to_le_bytes());
        p
    }

    /// Attach `sig` (the agent's raw 64-byte signature over
    /// [`Self::signing_payload`]).
    pub fn signed(mut self, sig: [u8; 64]) -> Self {
        self.sig = Some(crate::policy::hex_encode(&sig));
        self
    }

    /// Verify the binding's signature with `verifier` (the agent's key),
    /// fail-closed.
    pub fn verify(&self, verifier: &dyn ApproveVerifier) -> Result<(), String> {
        verify_hex_sig(&self.signing_payload(), &self.sig, verifier)
    }
}

/// The capability a challenge requests — the unified vocabulary reconciling
/// newt's 6-valued `DenialKind` with bridle's 3-valued durable
/// [`CapabilityClass`]. `RemoteTool`/`GitWrite` are name-based leashes with no
/// durable policy class ([`Self::to_capability_class`] returns `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedCapability {
    /// Command execution.
    Exec,
    /// Filesystem read.
    FsRead,
    /// Filesystem write.
    FsWrite,
    /// Network access.
    Net,
    /// A remote (MCP) tool invocation.
    RemoteTool,
    /// A local git write.
    GitWrite,
}

impl RequestedCapability {
    /// The stable wire tag (matches newt's `DenialKind::as_str`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::FsRead => "fs_read",
            Self::FsWrite => "fs_write",
            Self::Net => "net",
            Self::RemoteTool => "remote_tool",
            Self::GitWrite => "git_write",
        }
    }

    /// The durable [`CapabilityClass`] this maps to, or `None` for a name-based
    /// leash (`RemoteTool`/`GitWrite`) that has no `ocap/*.toml` class.
    pub fn to_capability_class(self) -> Option<CapabilityClass> {
        match self {
            Self::Exec => Some(CapabilityClass::Exec),
            Self::FsRead | Self::FsWrite => Some(CapabilityClass::Fs),
            Self::Net => Some(CapabilityClass::Net),
            Self::RemoteTool | Self::GitWrite => None,
        }
    }
}

/// The danger tier a challenge carries. **Gate-stamped** — the agent classifies
/// it; the operator surface renders it and never classifies. High-danger targets
/// are never durably always-allowable (at most passkey-gated).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DangerTier {
    /// Routine.
    Low,
    /// Notable.
    Medium,
    /// Interpreter exec, broad fs roots — never durably always-allowable.
    High,
}

/// A signed, expiring, digest-bearing request for an operator's permission
/// decision, issued by the agent. Signed with the agent's mesh key; verified via
/// [`ApproveVerifier`]. Wall-clock-free — [`Self::is_expired`] takes `now`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionChallenge {
    /// Unguessable per-request nonce — binds a decision to THIS request (a turn
    /// issues several).
    pub request_id: String,
    /// The capability class requested.
    pub capability: RequestedCapability,
    /// The concrete target (exec target / fs path / net host / tool name).
    pub target: String,
    /// Gate-stamped danger tier.
    pub danger: DangerTier,
    /// The mesh agent that issued (and signs) this challenge.
    pub mesh_agent_fingerprint: Fingerprint,
    /// Absolute expiry, unix seconds. The predicate takes `now`; this crate
    /// never reads a clock.
    pub expires_at_unix: i64,
    /// Agent's Ed25519 signature over [`Self::signing_payload`], hex-encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl PermissionChallenge {
    /// A fresh, unsigned challenge. Call [`Self::signed`] to complete it.
    pub fn new(
        request_id: impl Into<String>,
        capability: RequestedCapability,
        target: impl Into<String>,
        danger: DangerTier,
        mesh_agent_fingerprint: Fingerprint,
        expires_at_unix: i64,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            capability,
            target: target.into(),
            danger,
            mesh_agent_fingerprint,
            expires_at_unix,
            sig: None,
        }
    }

    /// The canonical, domain-separated, length-delimited bytes the agent signs —
    /// EVERY authority-bearing field, so a decision binds to exactly what was
    /// asked (a substituted target/danger/expiry breaks the signature).
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(CHALLENGE_DOMAIN);
        push_field(&mut p, self.request_id.as_bytes());
        push_field(&mut p, self.capability.as_str().as_bytes());
        push_field(&mut p, self.target.as_bytes());
        push_field(&mut p, danger_tag(self.danger).as_bytes());
        push_field(&mut p, &self.mesh_agent_fingerprint.0);
        p.extend_from_slice(&self.expires_at_unix.to_le_bytes());
        p
    }

    /// Attach `sig` (the agent's raw 64-byte signature over
    /// [`Self::signing_payload`]).
    pub fn signed(mut self, sig: [u8; 64]) -> Self {
        self.sig = Some(crate::policy::hex_encode(&sig));
        self
    }

    /// Verify the challenge signature with `verifier` (the agent's key),
    /// fail-closed.
    pub fn verify(&self, verifier: &dyn ApproveVerifier) -> Result<(), String> {
        verify_hex_sig(&self.signing_payload(), &self.sig, verifier)
    }

    /// The content address of this challenge (`BLAKE3` of [`Self::signing_payload`]) —
    /// what an [`OperatorDecision`] pins so it cannot be lifted onto another.
    pub fn digest(&self) -> Fingerprint {
        Fingerprint::of_bytes(&self.signing_payload())
    }

    /// Whether the challenge has expired at `now_unix`. Fail-closed at the
    /// boundary: at-or-past the deadline is expired.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        now_unix >= self.expires_at_unix
    }
}

/// The stable wire tag for a danger tier (used inside the signing payload).
fn danger_tag(d: DangerTier) -> &'static str {
    match d {
        DangerTier::Low => "low",
        DangerTier::Medium => "medium",
        DangerTier::High => "high",
    }
}

/// What an operator decided about a challenge — the **live decision**
/// vocabulary, distinct from the durable policy [`Verdict`] (what lands in
/// `ocap/*.toml`). Web decisions are STRICTLY EPHEMERAL (newt-agent #1354): the
/// web channel emits only `Deny`/`AllowOnce`/`AllowSession` and NEVER
/// `ApproveDurable` — promoting a grant to durable policy is a terminal-audit
/// action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionVerdict {
    /// Refuse.
    Deny,
    /// Allow this one request only (ephemeral).
    AllowOnce,
    /// Allow for the rest of this session (ephemeral).
    AllowSession,
    /// Promote to durable policy (`approve.toml`). Terminal-audit channel only —
    /// the web channel MUST NOT emit this (the web-ephemeral law).
    ApproveDurable,
}

impl DecisionVerdict {
    /// The durable policy [`Verdict`] this maps to, if it promotes to
    /// `ocap/*.toml`. Only `ApproveDurable` is durable; the ephemeral grants
    /// return `None`.
    pub fn durable_policy_verdict(self) -> Option<Verdict> {
        match self {
            Self::ApproveDurable => Some(Verdict::Approve),
            _ => None,
        }
    }

    /// Whether this decision writes durable policy (terminal-only; web = false).
    pub fn is_durable(self) -> bool {
        matches!(self, Self::ApproveDurable)
    }
}

/// An operator's answer to a [`PermissionChallenge`]: a live verdict bound to
/// exactly one challenge (by `request_id` and payload digest, so it cannot be
/// lifted onto another), carrying the authenticated human who decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorDecision {
    /// The challenge's nonce this answers.
    pub request_id: String,
    /// The content address of the challenge this answers (binds the decision to
    /// exactly what the operator saw).
    pub challenge_digest: Fingerprint,
    /// The live verdict.
    pub verdict: DecisionVerdict,
    /// Whether a passkey step-up is still owed before the grant is effective.
    #[serde(default)]
    pub step_up_owed: bool,
    /// The authenticated human who decided (never email-keyed).
    pub decided_by: HumanPrincipal,
}

impl OperatorDecision {
    /// A decision answering `challenge`, pinning its nonce + digest. `step_up_owed`
    /// defaults false; set it with [`Self::requiring_step_up`].
    pub fn for_challenge(
        challenge: &PermissionChallenge,
        verdict: DecisionVerdict,
        decided_by: HumanPrincipal,
    ) -> Self {
        Self {
            request_id: challenge.request_id.clone(),
            challenge_digest: challenge.digest(),
            verdict,
            step_up_owed: false,
            decided_by,
        }
    }

    /// Mark that a passkey step-up is owed before this grant takes effect.
    pub fn requiring_step_up(mut self) -> Self {
        self.step_up_owed = true;
        self
    }

    /// Whether this decision actually answers `challenge` — the nonce AND the
    /// content digest must match, so a decision for one challenge can never be
    /// replayed onto a different (even same-`request_id`, tampered) one.
    pub fn answers(&self, challenge: &PermissionChallenge) -> bool {
        self.request_id == challenge.request_id && self.challenge_digest == challenge.digest()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(seed: u8) -> Fingerprint {
        Fingerprint::of_bytes(&[seed; 8])
    }

    fn a_human() -> HumanPrincipal {
        HumanPrincipal {
            issuer: "https://idp.example".into(),
            subject: "sub-123".into(),
            email: Some("op@example.com".into()),
            groups: vec!["newt-operators".into()],
        }
    }

    #[test]
    fn human_principal_is_keyed_by_issuer_subject_never_email() {
        let h = a_human();
        assert_eq!(h.key(), ("https://idp.example", "sub-123"));
        assert!(h.is_member_of("newt-operators"));
        assert!(!h.is_member_of("admins"));
        // Same (iss,sub) with a changed email is the SAME identity key.
        let mut h2 = h.clone();
        h2.email = Some("changed@example.com".into());
        assert_eq!(h.key(), h2.key());
    }

    #[test]
    fn requested_capability_maps_onto_the_durable_vocab() {
        use RequestedCapability::*;
        // Exhaustive over the 6-valued vocab.
        assert_eq!(Exec.to_capability_class(), Some(CapabilityClass::Exec));
        assert_eq!(FsRead.to_capability_class(), Some(CapabilityClass::Fs));
        assert_eq!(FsWrite.to_capability_class(), Some(CapabilityClass::Fs));
        assert_eq!(Net.to_capability_class(), Some(CapabilityClass::Net));
        // Name-based leashes have NO durable class.
        assert_eq!(RemoteTool.to_capability_class(), None);
        assert_eq!(GitWrite.to_capability_class(), None);
        // Wire tags match newt's DenialKind::as_str.
        assert_eq!(FsWrite.as_str(), "fs_write");
        assert_eq!(RemoteTool.as_str(), "remote_tool");
    }

    #[test]
    fn signing_payloads_are_field_boundary_safe() {
        // Two humans whose (issuer, subject) concatenate the same way but split
        // differently MUST NOT share a binding payload — the length prefixes are
        // what prevent the forgery a newline join would allow.
        let a = PrincipalBinding::new(
            HumanPrincipal {
                issuer: "ab".into(),
                subject: "c".into(),
                email: None,
                groups: vec![],
            },
            fp(1),
            0,
        );
        let b = PrincipalBinding::new(
            HumanPrincipal {
                issuer: "a".into(),
                subject: "bc".into(),
                email: None,
                groups: vec![],
            },
            fp(1),
            0,
        );
        assert_ne!(a.signing_payload(), b.signing_payload());
        // Distinct generation ⇒ distinct payload.
        let mut c = a.clone();
        c.issued_generation = 1;
        assert_ne!(a.signing_payload(), c.signing_payload());
    }

    #[test]
    fn challenge_expiry_is_fail_closed_at_the_boundary() {
        let c = PermissionChallenge::new(
            "r1",
            RequestedCapability::Exec,
            "bash",
            DangerTier::High,
            fp(2),
            100,
        );
        assert!(!c.is_expired(99), "before the deadline: live");
        assert!(c.is_expired(100), "AT the deadline: expired (fail-closed)");
        assert!(c.is_expired(101), "after: expired");
    }

    #[test]
    fn decision_binds_to_exactly_its_challenge() {
        let c = PermissionChallenge::new(
            "r1",
            RequestedCapability::Exec,
            "bash",
            DangerTier::High,
            fp(3),
            100,
        );
        let d = OperatorDecision::for_challenge(&c, DecisionVerdict::AllowOnce, a_human());
        assert!(d.answers(&c), "answers its own challenge");
        // A challenge with the same request_id but a tampered field has a
        // different digest ⇒ the decision does NOT answer it.
        let mut tampered = c.clone();
        tampered.target = "rm".into();
        assert_eq!(tampered.request_id, c.request_id);
        assert!(
            !d.answers(&tampered),
            "digest mismatch ⇒ not answered (no lift)"
        );
        // A different request_id also fails.
        let other = PermissionChallenge::new(
            "r2",
            RequestedCapability::Exec,
            "bash",
            DangerTier::High,
            fp(3),
            100,
        );
        assert!(!d.answers(&other));
    }

    #[test]
    fn decision_verdict_only_approve_durable_is_durable() {
        use DecisionVerdict::*;
        assert_eq!(
            ApproveDurable.durable_policy_verdict(),
            Some(Verdict::Approve)
        );
        assert!(ApproveDurable.is_durable());
        for ephemeral in [Deny, AllowOnce, AllowSession] {
            assert_eq!(
                ephemeral.durable_policy_verdict(),
                None,
                "{ephemeral:?} is ephemeral"
            );
            assert!(!ephemeral.is_durable());
        }
    }

    #[test]
    fn step_up_owed_is_off_by_default_and_opt_in() {
        let c = PermissionChallenge::new(
            "r1",
            RequestedCapability::Net,
            "example.com",
            DangerTier::Medium,
            fp(4),
            100,
        );
        let d = OperatorDecision::for_challenge(&c, DecisionVerdict::AllowSession, a_human());
        assert!(!d.step_up_owed);
        assert!(d.requiring_step_up().step_up_owed);
    }
}

/// Ed25519 sign/verify round-trips — gated behind `verifier-ed25519` like the
/// `policy`/`step_up` siblings (the concrete verifier and `ed25519-dalek` only
/// exist under that feature). The real signer is the mesh `AgentKey`; a raw
/// `SigningKey` stands in faithfully (both are Ed25519 over the same payload).
#[cfg(all(test, feature = "verifier-ed25519"))]
mod signed_tests {
    use super::*;
    use crate::policy::Ed25519ApproveVerifier;
    use ed25519_dalek::{Signer, SigningKey};

    fn key() -> (SigningKey, Ed25519ApproveVerifier) {
        // Deterministic key (tests must not use randomness).
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let verifier = Ed25519ApproveVerifier {
            verifying_key: sk.verifying_key().to_bytes(),
        };
        (sk, verifier)
    }

    #[test]
    fn principal_binding_sign_verify_roundtrip_rejects_tamper() {
        let (sk, verifier) = key();
        let human = HumanPrincipal {
            issuer: "https://idp.example".into(),
            subject: "sub-1".into(),
            email: None,
            groups: vec![],
        };
        let binding = PrincipalBinding::new(human, Fingerprint::of_bytes(&[9u8; 8]), 3);
        let sig = sk.sign(&binding.signing_payload()).to_bytes();
        let signed = binding.clone().signed(sig);
        assert!(
            signed.verify(&verifier).is_ok(),
            "genuine signature verifies"
        );

        // Unsigned fails.
        assert!(
            binding.verify(&verifier).is_err(),
            "unsigned is fail-closed"
        );
        // Tampered generation fails (payload changed, sig no longer covers it).
        let mut tampered = signed.clone();
        tampered.issued_generation = 4;
        assert!(
            tampered.verify(&verifier).is_err(),
            "tampered generation rejected"
        );
        // Tampered fingerprint fails.
        let mut tampered2 = signed;
        tampered2.mesh_agent_fingerprint = Fingerprint::of_bytes(&[1u8; 8]);
        assert!(
            tampered2.verify(&verifier).is_err(),
            "tampered agent rejected"
        );
    }

    #[test]
    fn challenge_sign_verify_roundtrip_rejects_tamper() {
        let (sk, verifier) = key();
        let c = PermissionChallenge::new(
            "req-1",
            RequestedCapability::Exec,
            "bash",
            DangerTier::High,
            Fingerprint::of_bytes(&[9u8; 8]),
            100,
        );
        let sig = sk.sign(&c.signing_payload()).to_bytes();
        let signed = c.signed(sig);
        assert!(signed.verify(&verifier).is_ok());
        // Substituting the target (bash → rm) breaks the signature — the whole
        // point: a decision binds to exactly what the agent asked.
        let mut swapped = signed.clone();
        swapped.target = "rm".into();
        assert!(
            swapped.verify(&verifier).is_err(),
            "target substitution rejected"
        );
        // A wrong key rejects a genuine signature.
        let other = Ed25519ApproveVerifier {
            verifying_key: SigningKey::from_bytes(&[8u8; 32])
                .verifying_key()
                .to_bytes(),
        };
        assert!(signed.verify(&other).is_err(), "wrong signer rejected");
    }
}

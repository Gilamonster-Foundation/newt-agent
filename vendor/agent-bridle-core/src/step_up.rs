//! Human-presence step-up: a third leash outcome between *allow* and *deny*.
//!
//! The base leash decision is two-valued: a call is within the granted authority
//! or it is not (see [`crate::Gate::authorize`]). This module adds a third
//! outcome — **`attest`** — "authorized, but only with a fresh, non-repudiable
//! act of human presence" (a passkey / biometric gesture). It is **not a new
//! authority**: a discharge adds nothing to the grant (`effective` is still
//! `granted.meet(required)`); it sharpens the *liveness condition* under which
//! the same Writ is exercised. So it cannot break attenuation.
//!
//! The design (`newt-agent/docs/design/human-presence-capabilities.md`, paper
//! §7.5):
//!
//! - The [`Gate`](crate::Gate) stays pure and synchronous: it **verifies a
//!   proof** ([`Discharge`]) — it never performs the gesture. A host capability
//!   ([`DischargeProvider`], sibling of [`Sandbox`](crate::Sandbox)) runs the
//!   ceremony; [`Gate::authorize_step_up`](crate::Gate) orchestrates
//!   evaluate→obtain→authorize so the host needs a single call.
//! - The proof is bound to the *exact* action by a content-addressed
//!   [`Challenge`] — what-you-see-is-what-you-sign — so a gesture cannot be
//!   harvested and replayed for a different action.
//! - A verified, recorded gesture becomes a content-addressed [`Attestation`]:
//!   data that carries its own proof of integrity.
//!
//! Content-addressing reuses the mesh's BLAKE3 primitive
//! ([`agent_mesh_protocol::Fingerprint::of_bytes`]) so the whole stack speaks one
//! content-address.

use agent_mesh_protocol::Fingerprint;
use serde::{Deserialize, Serialize};

use crate::{ToolContext, ToolError};

/// Domain-separation tag mixed into every step-up [`Challenge`]. Bumping the
/// version invalidates every previously issued challenge.
// v2 (v0.8): the bound challenge now includes a per-session `SessionId`, so a
// discharge is domain-separated by the session that issued it. The version bump
// is deliberate — any v1 discharge (no session in the preimage) can never match
// a v2 challenge, which is exactly the cross-lifetime replay we are closing.
const CHALLENGE_DOMAIN: &[u8] = b"agent-bridle/step-up/v2";

// ── Presence ────────────────────────────────────────────────────────────────

/// The strength of human gesture an action demands, weakest to strongest.
///
/// The ordering is **load-bearing**: a discharge satisfies a requirement iff its
/// presence is `>=` the required presence (a `Passkey` over-satisfies a
/// `Prompt`; a `Prompt` never satisfies a `Passkey`). Because attenuation may
/// only *raise* a required presence, this keeps "you can get more restrictive,
/// never less" true for the presence axis too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// No human gesture required.
    #[default]
    None,
    /// A soft prompt: any UI affirmation (a typed "yes", a click). Advisory —
    /// it proves a human *chose*, not *who*. (Charter: Tether.)
    Prompt,
    /// A hardware human gesture: a WebAuthn/FIDO2 user-presence (and optional
    /// user-verification) assertion from an authenticator the human controls.
    Passkey,
}

// ── Content-addressed action identity + challenge ────────────────────────────

/// The content address (BLAKE3) of a canonicalized [`CallRequest`] — a stable,
/// collision-resistant identity for "this exact action."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentId([u8; 32]);

impl ContentId {
    /// Content-address arbitrary bytes with the mesh's BLAKE3 primitive.
    #[must_use]
    pub fn of_bytes(data: &[u8]) -> Self {
        Self(Fingerprint::of_bytes(data).0)
    }

    /// The raw 32-byte digest.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lower-hex rendering of the digest.
    #[must_use]
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// A per-session identity that domain-separates every step-up [`Challenge`] a
/// gate issues, so a human gesture proven for one session cannot authenticate a
/// different one.
///
/// **The problem it closes.** Without it, a challenge is `action ‖ generation ‖
/// nonce`, and the gate's single-use replay guard is an in-memory set. Two
/// distinct hazards follow: a second live gate never saw the first's discharge,
/// and a *recreated* gate (process/session restart) starts with an empty guard.
/// Either way an identical `(action, generation, nonce)` reproduces identical
/// challenge bytes, so a captured discharge re-verifies. Folding a `SessionId`
/// into the preimage makes the challenge bytes differ per session, so the old
/// discharge answers a challenge no live gate is asking — the replay is refused
/// structurally, not merely because a volatile set happens to remember it.
///
/// **Host contract — the identity MUST be lifetime-unique.** The core crate is
/// deliberately rng-less and clock-less, so the host supplies the 32 bytes. Mint
/// them **fresh per gate lifetime** from a CSPRNG (or a durable per-session
/// secret that is never reused across a trust boundary). Reusing the same
/// `SessionId` in a recreated gate re-opens exactly the replay window this type
/// exists to close — the domain separation is only as strong as the freshness
/// of the identity behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId([u8; 32]);

impl SessionId {
    /// Wrap 32 host-supplied bytes as a session identity. See the type contract:
    /// these MUST be lifetime-unique (freshly minted per gate), or the replay
    /// protection they provide is void.
    #[must_use]
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw 32-byte identity mixed into every challenge this session binds.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A what-you-see-is-what-you-sign challenge: the content address of
/// `DOMAIN ‖ session ‖ action_id ‖ generation ‖ nonce`. The authenticator signs
/// *this*, so a verified signature proves the human authorized that exact action,
/// in that session, in that causal generation, for that single-use nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge([u8; 32]);

impl Challenge {
    /// Bind a challenge to the issuing [`SessionId`], an action's [`ContentId`],
    /// a causal `generation` (never wall-clock), and a single-use `nonce`.
    ///
    /// The `session` is the first field after the domain tag, so a discharge is
    /// inseparable from the session that issued its challenge: replaying it into
    /// any other session recomputes different bytes and is refused.
    #[must_use]
    pub fn bind(
        session: &SessionId,
        action: &ContentId,
        generation: u64,
        nonce: &[u8; 32],
    ) -> Self {
        let mut buf = Vec::with_capacity(CHALLENGE_DOMAIN.len() + 32 + 32 + 8 + 32);
        buf.extend_from_slice(CHALLENGE_DOMAIN);
        buf.extend_from_slice(session.as_bytes());
        buf.extend_from_slice(action.as_bytes());
        buf.extend_from_slice(&generation.to_le_bytes());
        buf.extend_from_slice(nonce);
        Self(Fingerprint::of_bytes(&buf).0)
    }

    /// The raw 32-byte challenge the authenticator signs.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// The action a leash decision is about: a tool name, its arguments, and the
/// resolved resource the policy keys on.
///
/// **Resolve before constructing.** A `resource` (and any path-bearing `args`)
/// must already be canonicalized (realpath, normalized refspec) so the human
/// approves the *resolved* effect, not a `..`-bearing alias.
#[derive(Debug, Clone)]
pub struct CallRequest {
    /// The dispatch name of the tool (e.g. `git.push`, `email.send`).
    pub tool: String,
    /// The tool arguments (the MCP `arguments` object).
    pub args: serde_json::Value,
    /// The resolved, policy-relevant resource (e.g. `github.com/org/repo`,
    /// a realpath, a recipient set).
    pub resource: String,
}

impl CallRequest {
    /// Construct a request.
    #[must_use]
    pub fn new(
        tool: impl Into<String>,
        args: serde_json::Value,
        resource: impl Into<String>,
    ) -> Self {
        Self {
            tool: tool.into(),
            args,
            resource: resource.into(),
        }
    }

    /// A request with no arguments and no resource — used by the back-compat
    /// no-step-up path.
    #[must_use]
    pub fn unspecified(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            args: serde_json::Value::Null,
            resource: String::new(),
        }
    }

    /// The content address of this action, computed over a canonical
    /// serialization of `(tool, canonical(args), resource)`.
    ///
    /// Canonicalization sorts object keys recursively (so argument order cannot
    /// change the identity) and is robust to whether `serde_json`'s
    /// `preserve_order` feature is enabled. Full RFC 8785 number/string
    /// normalization is a follow-up; for now the byte form is deterministic and
    /// order-independent, which is what the binding requires.
    #[must_use]
    pub fn content_id(&self) -> ContentId {
        let canonical = (
            self.tool.as_str(),
            canonical_json(&self.args),
            self.resource.as_str(),
        );
        let bytes = serde_json::to_vec(&canonical)
            .expect("a (str, Value, str) tuple is always JSON-serializable");
        ContentId::of_bytes(&bytes)
    }
}

/// Recursively rebuild a JSON value with object keys sorted, so the byte form is
/// deterministic regardless of insertion order or the `preserve_order` feature.
fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for k in keys {
                sorted.insert(k.clone(), canonical_json(&map[k]));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical_json).collect())
        }
        other => other.clone(),
    }
}

// ── Requirement, discharge, attestation ──────────────────────────────────────

/// What step-up an action demands before the gate will mint a context for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestRequirement {
    /// Minimum gesture strength this action demands.
    pub presence: Presence,
    /// Whether a verified gesture must be recorded as a provenance
    /// [`Attestation`] (the `passkey+record` policy decision).
    pub record: bool,
    /// Maximum age, in causal generations (never wall-clock), a discharge may
    /// have: the gate accepts a discharge bound to any generation in
    /// `[current - freshness_generations, current]`. `0` ⇒ it must be bound to
    /// the current generation (fresh-per-act, HIGH-consequence); `N` ⇒ a gesture
    /// may be reused for up to `N` generations (LOW-consequence amortization).
    /// Enforced in [`Gate::authorize_with_discharge`](crate::Gate) by recomputing
    /// the bound [`Challenge`] across the window, combined with single-use
    /// consumption so one gesture authorizes exactly one act (ADR 0007 D4). The
    /// window is capped defensively (fail-closed) to bound the scan.
    pub freshness_generations: u64,
}

impl AttestRequirement {
    /// The empty requirement: no gesture, no record. The base-case for actions
    /// with no step-up policy.
    pub const NONE: Self = Self {
        presence: Presence::None,
        record: false,
        freshness_generations: 0,
    };

    /// A requirement for the given presence (no recording, current-generation
    /// freshness).
    #[must_use]
    pub fn presence(presence: Presence) -> Self {
        Self {
            presence,
            record: false,
            freshness_generations: 0,
        }
    }

    /// A requirement for a hardware gesture **and** a recorded attestation.
    #[must_use]
    pub fn passkey_recorded() -> Self {
        Self {
            presence: Presence::Passkey,
            record: true,
            freshness_generations: 0,
        }
    }

    /// Does this action demand any gesture at all?
    #[must_use]
    pub fn demands_gesture(&self) -> bool {
        self.presence > Presence::None
    }
}

/// A human-presence proof presented to the gate. Crypto-format-agnostic: the
/// `signature` and `credential_id` are opaque bytes a [`DischargeVerifier`]
/// interprets (e.g. the `Ed25519Verifier` reads them as a raw ed25519 verifying
/// key + assertion, under the `verifier-ed25519` feature).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discharge {
    /// The gesture strength actually achieved (e.g. user-presence vs. verified).
    pub presence: Presence,
    /// Which authenticator/credential produced the proof (a public-key id).
    pub credential_id: Vec<u8>,
    /// The challenge bytes the authenticator signed.
    pub challenge: [u8; 32],
    /// The assertion signature. For the raw-Ed25519 path this signs the
    /// `challenge` directly; for the WebAuthn path it signs
    /// `authenticator_data ‖ SHA-256(client_data_json)`.
    pub signature: Vec<u8>,
    /// WebAuthn `authenticatorData` (binary: `rpIdHash‖flags‖signCount‖…`), set
    /// only when the proof is a WebAuthn/CTAP2 assertion ([`WebAuthnVerifier`]).
    /// `None` for the raw-Ed25519 path. Backward-compatible on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authenticator_data: Option<Vec<u8>>,
    /// WebAuthn `clientDataJSON` (the raw UTF-8 bytes the authenticator hashed),
    /// set only for a WebAuthn assertion; carries the base64url-encoded
    /// challenge the verifier binds against. `None` for the raw-Ed25519 path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_data_json: Option<Vec<u8>>,
}

/// A content-addressed, non-repudiable record that a human authorized a specific
/// action — Provenance that becomes a Scar in the causal log. It carries its own
/// proof: the credential id + signature prove *which* authenticator, the
/// challenge proves *which* action, the generation proves *which* flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    /// Schema tag for forward-compatible parsing.
    pub schema: String,
    /// The tool whose invocation was authorized.
    pub tool: String,
    /// The resolved resource that was authorized.
    pub resource: String,
    /// The bound challenge the human signed.
    pub challenge: [u8; 32],
    /// The causal generation this authorization is valid for.
    pub generation: u64,
    /// The authenticator/credential id.
    pub credential_id: Vec<u8>,
    /// The assertion signature.
    pub signature: Vec<u8>,
    /// The gesture strength achieved.
    pub presence: Presence,
}

impl Attestation {
    /// Current attestation schema tag.
    pub const SCHEMA: &'static str = "agent-bridle/attestation/v1";

    /// Build a provenance record from a verified discharge for `(tool,
    /// resource)` in `generation`. Called by the gate only *after* a
    /// [`DischargeVerifier`] has accepted the discharge.
    #[must_use]
    pub fn from_verified(
        tool: &str,
        resource: &str,
        discharge: &Discharge,
        generation: u64,
    ) -> Self {
        Self {
            schema: Self::SCHEMA.to_string(),
            tool: tool.to_string(),
            resource: resource.to_string(),
            challenge: discharge.challenge,
            generation,
            credential_id: discharge.credential_id.clone(),
            signature: discharge.signature.clone(),
            presence: discharge.presence,
        }
    }

    /// The content address of this attestation record.
    #[must_use]
    pub fn content_id(&self) -> ContentId {
        let bytes = serde_json::to_vec(self).expect("Attestation is always JSON-serializable");
        ContentId::of_bytes(&bytes)
    }
}

/// Verifies a [`Discharge`] against the requirement and the gate-recomputed
/// [`Challenge`]. **Pure**: it performs no ceremony and no IO — that is a host
/// capability outside the gate. An `Err(reason)` is turned into a leash denial.
pub trait DischargeVerifier {
    /// Accept iff the discharge is a valid proof for `expected`, at or above
    /// `required.presence`. The reason string is safe to surface to the agent.
    fn verify(
        &self,
        discharge: &Discharge,
        required: &AttestRequirement,
        expected: &Challenge,
    ) -> Result<(), String>;
}

/// A production [`DischargeVerifier`] for **raw Ed25519** assertions — the format
/// the OpenSSH `ed25519-sk` / software-passkey path produces. It interprets
/// [`Discharge::credential_id`] as a 32-byte verifying key and
/// [`Discharge::signature`] as a 64-byte assertion over the gate-recomputed
/// [`Challenge`], and is **presence-agnostic about *how*** the gesture was
/// achieved (it trusts the host-reported [`Presence`] only up to the floor the
/// gate re-checks below).
///
/// It checks, in order: the presence floor (`discharge.presence >= required.presence`),
/// the challenge binding (anti-theater — the signed bytes must equal the bytes
/// the gate recomputed), then `verify_strict` over the challenge.
///
/// **Off by default.** Enable the `verifier-ed25519` cargo feature. This is the
/// raw-Ed25519 path only; the WebAuthn/CTAP2 assertion path (clientDataJSON +
/// authenticatorData + UP/UV flag bits) is the sibling [`WebAuthnVerifier`]
/// (`verifier-webauthn` feature). Attestation-certificate chains / FIDO MDS and
/// live USB/HID transport remain out of scope for both (ADR 0007).
#[cfg(feature = "verifier-ed25519")]
#[derive(Debug, Default, Clone, Copy)]
pub struct Ed25519Verifier;

#[cfg(feature = "verifier-ed25519")]
impl DischargeVerifier for Ed25519Verifier {
    fn verify(
        &self,
        discharge: &Discharge,
        required: &AttestRequirement,
        expected: &Challenge,
    ) -> Result<(), String> {
        use ed25519_dalek::{Signature, VerifyingKey};
        // Presence floor first — a too-weak gesture is rejected before any crypto
        // (fail-closed; ADR 0007 D2).
        if discharge.presence < required.presence {
            return Err(format!(
                "presence {:?} is below required {:?}",
                discharge.presence, required.presence
            ));
        }
        // Anti-theater: the discharge must answer THIS action's challenge.
        if &discharge.challenge != expected.as_bytes() {
            return Err("discharge does not answer this action's challenge".into());
        }
        let vk_bytes: [u8; 32] = discharge
            .credential_id
            .as_slice()
            .try_into()
            .map_err(|_| "credential id is not a 32-byte ed25519 key".to_string())?;
        let vk = VerifyingKey::from_bytes(&vk_bytes).map_err(|e| e.to_string())?;
        let sig_bytes: [u8; 64] = discharge
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| "signature is not 64 bytes".to_string())?;
        let sig = Signature::from_bytes(&sig_bytes);
        vk.verify_strict(expected.as_bytes(), &sig)
            .map_err(|e| e.to_string())
    }
}

/// base64url **without padding** (RFC 4648 §5), the encoding WebAuthn uses for
/// `clientDataJSON.challenge`. Encoding-only: we encode the gate-recomputed
/// challenge and string-compare it to the assertion, so no decoder (and no
/// malleable-input parsing) is needed.
#[cfg(any(feature = "verifier-webauthn", feature = "verifier-webauthn-es256"))]
fn base64url_nopad(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(T[((n >> 18) & 0x3f) as usize] as char);
        out.push(T[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(T[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(T[(n & 0x3f) as usize] as char);
        }
    }
    out
}

/// A production [`DischargeVerifier`] for **WebAuthn/CTAP2 assertions** — the
/// format a hardware passkey (or a `navigator.credentials.get()` ceremony)
/// produces. The opaque [`Discharge`] fields are read as a WebAuthn assertion:
/// [`Discharge::credential_id`] is the 32-byte Ed25519 (COSE `EdDSA`/`-8`)
/// verifying key, [`Discharge::authenticator_data`] and
/// [`Discharge::client_data_json`] are the assertion's two signed structures, and
/// [`Discharge::signature`] is the EdDSA signature over
/// `authenticatorData ‖ SHA-256(clientDataJSON)` (the WebAuthn signing input).
///
/// It checks, in order (fail-closed; ADR 0007 D2):
/// 1. the **presence floor** — `discharge.presence >= required.presence` —
///    *before any crypto or parsing*;
/// 2. the **flag bits** in `authenticatorData`: User-Presence (UP) must be set,
///    and User-Verification (UV) must be set whenever the requirement is
///    `Presence::Passkey` (the hardware-verified tier);
/// 3. the **challenge binding** (anti-theater): `clientDataJSON.type` is
///    `"webauthn.get"` and its `challenge` equals the base64url of the
///    gate-recomputed [`Challenge`];
/// 4. the **signature** over `authenticatorData ‖ SHA-256(clientDataJSON)`.
///
/// **Off by default.** Enable the `verifier-webauthn` cargo feature. Out of
/// scope (ADR 0007): attestation-certificate chain validation / FIDO MDS, live
/// USB/HID transport, and credential registration — this verifies a *presented*
/// assertion, it does not run the ceremony (that is a [`DischargeProvider`]).
#[cfg(feature = "verifier-webauthn")]
#[derive(Debug, Default, Clone, Copy)]
pub struct WebAuthnVerifier;

/// The shared WebAuthn assertion checks — presence floor, `authenticatorData`
/// flags (UP required; UV required for Passkey), the `webauthn.get` type, and
/// the challenge binding — returning the signed message
/// `authenticatorData ‖ SHA-256(clientDataJSON)`. **Single-sourced** so the two
/// signature-suite verifiers ([`WebAuthnVerifier`], EdDSA/-8, and
/// [`WebAuthnEs256Verifier`], ES256/-7) cannot drift on the security-critical
/// gating — the broken call is made unrepresentable rather than fixed twice.
#[cfg(any(feature = "verifier-webauthn", feature = "verifier-webauthn-es256"))]
fn webauthn_verify_common(
    discharge: &Discharge,
    required: &AttestRequirement,
    expected: &Challenge,
) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};

    // 1. Presence floor first — a too-weak gesture is rejected before any
    // parsing or crypto (fail-closed; ADR 0007 D2).
    if discharge.presence < required.presence {
        return Err(format!(
            "presence {:?} is below required {:?}",
            discharge.presence, required.presence
        ));
    }

    // The WebAuthn proof parts must be present.
    let auth_data = discharge
        .authenticator_data
        .as_deref()
        .ok_or("WebAuthn assertion is missing authenticatorData")?;
    let client_data = discharge
        .client_data_json
        .as_deref()
        .ok_or("WebAuthn assertion is missing clientDataJSON")?;

    // 2. authenticatorData = rpIdHash[32] ‖ flags[1] ‖ signCount[4] ‖ …
    if auth_data.len() < 37 {
        return Err("authenticatorData is too short (need ≥ 37 bytes)".into());
    }
    let flags = auth_data[32];
    let up = flags & 0x01 != 0; // bit 0: User Present
    let uv = flags & 0x04 != 0; // bit 2: User Verified
    if !up {
        return Err("authenticatorData User-Presence (UP) flag is not set".into());
    }
    if required.presence >= Presence::Passkey && !uv {
        return Err(
            "authenticatorData User-Verification (UV) flag is required for Passkey but not set"
                .into(),
        );
    }

    // 3. clientDataJSON: must be a `webauthn.get` answering THIS challenge.
    #[derive(serde::Deserialize)]
    struct ClientData {
        #[serde(rename = "type")]
        typ: String,
        challenge: String,
    }
    let cd: ClientData = serde_json::from_slice(client_data)
        .map_err(|e| format!("clientDataJSON does not parse: {e}"))?;
    if cd.typ != "webauthn.get" {
        return Err(format!(
            "clientDataJSON.type is {:?}, expected \"webauthn.get\"",
            cd.typ
        ));
    }
    // Anti-theater: the signed challenge must equal the bytes the gate
    // recomputed (base64url form).
    if cd.challenge != base64url_nopad(expected.as_bytes()) {
        return Err("assertion does not answer this action's challenge".into());
    }
    // Defense-in-depth: the discharge's own challenge field must agree too, so a
    // recorded Attestation (built from these fields) stays consistent.
    if &discharge.challenge != expected.as_bytes() {
        return Err("discharge challenge does not match the gate-recomputed challenge".into());
    }

    // The signed message: authenticatorData ‖ SHA-256(clientDataJSON).
    let mut signed = Vec::with_capacity(auth_data.len() + 32);
    signed.extend_from_slice(auth_data);
    signed.extend_from_slice(&Sha256::digest(client_data));
    Ok(signed)
}

#[cfg(feature = "verifier-webauthn")]
impl DischargeVerifier for WebAuthnVerifier {
    fn verify(
        &self,
        discharge: &Discharge,
        required: &AttestRequirement,
        expected: &Challenge,
    ) -> Result<(), String> {
        use ed25519_dalek::{Signature, VerifyingKey};
        // Shared gating + the signed message (single-sourced).
        let signed = webauthn_verify_common(discharge, required, expected)?;
        // Verify the EdDSA (-8) signature over that message.
        let vk_bytes: [u8; 32] = discharge
            .credential_id
            .as_slice()
            .try_into()
            .map_err(|_| "credential id is not a 32-byte ed25519 key".to_string())?;
        let vk = VerifyingKey::from_bytes(&vk_bytes).map_err(|e| e.to_string())?;
        let sig_bytes: [u8; 64] = discharge
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| "signature is not 64 bytes".to_string())?;
        let sig = Signature::from_bytes(&sig_bytes);
        vk.verify_strict(&signed, &sig).map_err(|e| e.to_string())
    }
}

/// The **ES256** (COSE alg −7) sibling of [`WebAuthnVerifier`]. Most platform
/// passkeys (phone / laptop) sign ECDSA-P256, not EdDSA, so this is what lets
/// real hardware work. Shares [`webauthn_verify_common`] for the presence/flag/
/// challenge gating and differs only in the signature suite: an ASN.1-DER ECDSA
/// signature over `authenticatorData ‖ SHA-256(clientDataJSON)`, verified with a
/// P-256 (SEC1) public key (`p256::ecdsa` hashes with SHA-256 internally).
/// **Off by default** — enable the `verifier-webauthn-es256` cargo feature
/// (pulls `p256`). Same out-of-scope as the sibling (ADR 0007): no attestation
/// chain / MDS / registration.
#[cfg(feature = "verifier-webauthn-es256")]
#[derive(Debug, Default, Clone, Copy)]
pub struct WebAuthnEs256Verifier;

#[cfg(feature = "verifier-webauthn-es256")]
impl DischargeVerifier for WebAuthnEs256Verifier {
    fn verify(
        &self,
        discharge: &Discharge,
        required: &AttestRequirement,
        expected: &Challenge,
    ) -> Result<(), String> {
        use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
        // Shared gating + the signed message (single-sourced).
        let signed = webauthn_verify_common(discharge, required, expected)?;
        // The credential is a P-256 public key (SEC1 point); the assertion is an
        // ASN.1-DER ECDSA signature.
        let vk = VerifyingKey::from_sec1_bytes(&discharge.credential_id)
            .map_err(|_| "credential id is not a valid P-256 SEC1 public key".to_string())?;
        let sig = Signature::from_der(&discharge.signature)
            .map_err(|_| "signature is not a valid DER ECDSA signature".to_string())?;
        vk.verify(&signed, &sig).map_err(|e| e.to_string())
    }
}

/// Runs the human-presence **ceremony** and returns a [`Discharge`] — the dual
/// of [`DischargeVerifier`] (a provider *produces* a proof; a verifier *checks*
/// one).
///
/// This is a **host capability**, a sibling of [`Sandbox`](crate::Sandbox): it
/// performs IO/UI (prompts a passkey, drives an authenticator) and lives in the
/// host, not the gate. The [`Gate`](crate::Gate) never calls it during
/// verification — only [`Gate::authorize_step_up`](crate::Gate) calls it, to
/// orchestrate the evaluate→obtain→authorize sequence on the host's behalf.
///
/// **It is not trusted to self-attest presence.** A provider returns whatever
/// [`Presence`] it claims to have achieved, but the gate (via the
/// [`DischargeVerifier`]) still re-checks `discharge.presence >= required.presence`
/// and that the discharge answers the gate-recomputed [`Challenge`]. A lying or
/// buggy provider can only get its discharge *rejected*, never over-admitted
/// (ADR 0007 D5).
pub trait DischargeProvider {
    /// Run the ceremony for `request` at `required` strength and return a
    /// [`Discharge`] whose `challenge` answers
    /// [`Challenge::bind`]`(session, &request.content_id(), generation, nonce)`.
    ///
    /// `session`, `generation`, and the single-use `nonce` are supplied by the
    /// caller (the gate) so the produced proof binds to this exact session,
    /// action, generation, and nonce — what-you-see-is-what-you-sign. The
    /// `session` in particular makes the proof inseparable from the gate that
    /// asked for it, so it cannot be replayed into another. An `Err(reason)` (the
    /// human declined, no authenticator present, a transport failure) becomes a
    /// fail-closed leash denial; the reason is safe to surface to the agent.
    fn obtain(
        &self,
        session: &SessionId,
        request: &CallRequest,
        required: &AttestRequirement,
        generation: u64,
        nonce: &[u8; 32],
    ) -> Result<Discharge, String>;
}

/// A presented step-up proof, bundled: the single-use `nonce` the challenge was
/// bound with, the [`Discharge`] itself, and the [`DischargeVerifier`] that
/// checks it. Grouping the "proof" inputs keeps
/// [`Gate::authorize_with_discharge`](crate::Gate) to a small argument list and
/// distinct from the "what" (tool, grant, request, policy).
pub struct DischargeAttempt<'a> {
    /// The single-use nonce the challenge was bound with.
    pub nonce: [u8; 32],
    /// The proof produced by the host after running the ceremony.
    pub discharge: &'a Discharge,
    /// The verifier that checks the proof (pure; performs no ceremony).
    pub verifier: &'a dyn DischargeVerifier,
}

// ── Decision ─────────────────────────────────────────────────────────────────

/// The gate's verdict for one call under a step-up policy.
#[derive(Debug)]
pub enum Decision {
    /// Authorized with no step-up owed — the minted context, exactly as today.
    Allow(ToolContext),
    /// Refused. Fail-closed; this always wins.
    Deny(ToolError),
    /// Conditionally authorized: obtain a discharge satisfying the requirement
    /// and re-present it to [`Gate::authorize_with_discharge`](crate::Gate). No
    /// context is minted and no budget is charged here.
    NeedsDischarge(AttestRequirement),
}

// ── Policy ───────────────────────────────────────────────────────────────────

/// One policy rule mapping an action selector to a required step-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// `"<tool>"` or `"<tool>:<resource-glob>"`. The glob supports a single
    /// trailing `*` (or `**`) meaning prefix-match.
    pub selector: String,
    /// The step-up this rule demands when it matches.
    pub requirement: AttestRequirement,
}

impl Rule {
    fn matches(&self, request: &CallRequest) -> bool {
        let (tool, resource_glob) = match self.selector.split_once(':') {
            Some((tool, glob)) => (tool, Some(glob)),
            None => (self.selector.as_str(), None),
        };
        if tool != request.tool {
            return false;
        }
        match resource_glob {
            None => true,
            Some(glob) => glob_prefix_match(glob, &request.resource),
        }
    }
}

/// Exact match, or prefix-match when `pattern` ends in `*` / `**`.
fn glob_prefix_match(pattern: &str, text: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        let prefix = prefix.strip_suffix('*').unwrap_or(prefix);
        text.starts_with(prefix)
    } else {
        pattern == text
    }
}

/// The per-action step-up policy: a set of selector rules plus a fall-through
/// default. Most-specific (longest matching selector) wins. This is the
/// authoring surface behind the operator menu (*yes once / yes always / yes on
/// passkey / no*); it composes *on top of* the `Caveats` grant — `Caveats`
/// decides whether the authority exists, this decides what gesture admits its
/// use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepUpPolicy {
    /// Selector rules, evaluated most-specific-wins.
    pub rules: Vec<Rule>,
    /// The requirement for any action no rule matches.
    pub default: AttestRequirement,
}

impl StepUpPolicy {
    /// The empty policy: nothing ever needs a gesture. Used by the back-compat
    /// no-step-up path so existing behavior is unchanged.
    pub const EMPTY: Self = Self {
        rules: Vec::new(),
        default: AttestRequirement::NONE,
    };

    /// A policy with the given rules and default.
    #[must_use]
    pub fn new(rules: Vec<Rule>, default: AttestRequirement) -> Self {
        Self { rules, default }
    }

    /// The strongest requirement matching `request` (longest selector wins), or
    /// the policy default when none match.
    #[must_use]
    pub fn required_for(&self, request: &CallRequest) -> AttestRequirement {
        let mut best: Option<&Rule> = None;
        for rule in &self.rules {
            if rule.matches(request) && best.is_none_or(|b| rule.selector.len() > b.selector.len())
            {
                best = Some(rule);
            }
        }
        best.map_or_else(|| self.default.clone(), |r| r.requirement.clone())
    }
}

impl Default for StepUpPolicy {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Caveats, CountBound, Gate, Tool, ToolResult};
    // `ToolError` is only referenced by the crypto tests below (gated), so gate
    // its import too — otherwise `--no-default-features` flags it unused.
    #[cfg(feature = "verifier-ed25519")]
    use crate::ToolError;
    #[cfg(any(feature = "verifier-ed25519", feature = "verifier-webauthn"))]
    use ed25519_dalek::{Signer, SigningKey};

    /// A trivial tool, named so policy selectors can match it.
    struct NamedTool(&'static str);
    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn invoke(
            &self,
            _args: serde_json::Value,
            _cx: &ToolContext,
        ) -> ToolResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
    }

    /// Deterministic test key (a fixed seed — never a real secret). The step-up
    /// crypto tests sign with this and verify with the production
    /// [`Ed25519Verifier`], so they require the `verifier-ed25519` feature; the
    /// lean `--no-default-features` build compiles them out (CI's `--all-features`
    /// matrix runs them).
    #[cfg(feature = "verifier-ed25519")]
    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    /// A fixed session identity for the step-up tests. Every gate under test is
    /// built `.with_session(test_session())` and every discharge is signed for it,
    /// so the ceremony round-trips; the cross-session *rejection* is proven
    /// explicitly with a second, distinct id in
    /// [`a_discharge_bound_to_one_session_is_rejected_by_another`]. Ungated: it
    /// mints only a `SessionId` (no crypto), and non-verifier gate tests call it
    /// too, so it must exist under `--no-default-features` as well.
    fn test_session() -> SessionId {
        SessionId::new([9u8; 32])
    }

    /// Build a discharge by signing the challenge for `request` at `generation`,
    /// bound to `session` (what the gate will recompute against).
    #[cfg(feature = "verifier-ed25519")]
    fn sign_discharge(
        session: &SessionId,
        key: &SigningKey,
        request: &CallRequest,
        generation: u64,
        nonce: &[u8; 32],
        presence: Presence,
    ) -> Discharge {
        let challenge = Challenge::bind(session, &request.content_id(), generation, nonce);
        let sig = key.sign(challenge.as_bytes());
        Discharge {
            presence,
            credential_id: key.verifying_key().to_bytes().to_vec(),
            challenge: *challenge.as_bytes(),
            signature: sig.to_bytes().to_vec(),
            authenticator_data: None,
            client_data_json: None,
        }
    }

    fn push_policy() -> StepUpPolicy {
        StepUpPolicy::new(
            vec![Rule {
                selector: "git.push:github.com/org/*".to_string(),
                requirement: AttestRequirement::passkey_recorded(),
            }],
            AttestRequirement::NONE,
        )
    }

    fn push_request() -> CallRequest {
        CallRequest::new(
            "git.push",
            serde_json::json!({"ref": "refs/heads/main"}),
            "github.com/org/repo",
        )
    }

    #[test]
    fn presence_is_totally_ordered_none_prompt_passkey() {
        assert!(Presence::None < Presence::Prompt);
        assert!(Presence::Prompt < Presence::Passkey);
    }

    #[test]
    fn content_id_is_deterministic_and_argument_order_independent() {
        let a = CallRequest::new(
            "email.send",
            serde_json::json!({"to": "x", "subj": "y"}),
            "r",
        );
        let b = CallRequest::new(
            "email.send",
            serde_json::json!({"subj": "y", "to": "x"}),
            "r",
        );
        assert_eq!(
            a.content_id(),
            b.content_id(),
            "key order must not change identity"
        );
        let c = CallRequest::new(
            "email.send",
            serde_json::json!({"to": "z", "subj": "y"}),
            "r",
        );
        assert_ne!(a.content_id(), c.content_id(), "different args must differ");
    }

    /// Regression: a step-up requirement must NOT mint a context or charge a
    /// call — the gate withholds the leash until a proof is presented.
    #[test]
    fn needs_discharge_does_not_mint_or_charge() {
        let gate = Gate::with_budget(0, CountBound::AtMost(1)).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        // First, an action the policy gates: must return NeedsDischarge.
        match gate.evaluate(&tool, &granted, &push_request(), &push_policy()) {
            Decision::NeedsDischarge(req) => assert_eq!(req.presence, Presence::Passkey),
            other => panic!("expected NeedsDischarge, got {other:?}"),
        }
        // The single budgeted call must still be available — proving the
        // NeedsDischarge above charged nothing.
        let free = NamedTool("free.tool");
        match gate.evaluate(
            &free,
            &granted,
            &CallRequest::unspecified("free.tool"),
            &StepUpPolicy::EMPTY,
        ) {
            Decision::Allow(cx) => assert!(cx.caveats().leq(&granted)),
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    /// A valid discharge over the bound challenge mints the context and records
    /// the attestation; the context still carries least authority.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn valid_discharge_mints_and_records() {
        let gate = Gate::new(0).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        let nonce = [9u8; 32];
        let discharge = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &nonce,
            Presence::Passkey,
        );
        let attempt = DischargeAttempt {
            nonce,
            discharge: &discharge,
            verifier: &Ed25519Verifier,
        };

        let (cx, attestation) = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt)
            .expect("valid discharge authorizes");
        assert!(cx.caveats().leq(&granted));
        let attestation = attestation.expect("passkey+record must produce an attestation");
        assert_eq!(attestation.tool, "git.push");
        assert_eq!(attestation.resource, "github.com/org/repo");
        // The record is content-addressed and stable.
        assert_eq!(attestation.content_id(), attestation.content_id());
    }

    /// A test [`DischargeProvider`] standing in for the host ceremony: it signs
    /// the bound challenge with a fixed key at a chosen presence (the gesture's
    /// effect, stubbed — no real authenticator).
    #[cfg(feature = "verifier-ed25519")]
    struct MockProvider {
        key: SigningKey,
        presence: Presence,
    }
    #[cfg(feature = "verifier-ed25519")]
    impl DischargeProvider for MockProvider {
        fn obtain(
            &self,
            session: &SessionId,
            request: &CallRequest,
            _required: &AttestRequirement,
            generation: u64,
            nonce: &[u8; 32],
        ) -> Result<Discharge, String> {
            Ok(sign_discharge(
                session,
                &self.key,
                request,
                generation,
                nonce,
                self.presence,
            ))
        }
    }

    /// A provider whose ceremony fails — the human declined, or no authenticator
    /// is present.
    #[cfg(feature = "verifier-ed25519")]
    struct FailingProvider;
    #[cfg(feature = "verifier-ed25519")]
    impl DischargeProvider for FailingProvider {
        fn obtain(
            &self,
            _session: &SessionId,
            _request: &CallRequest,
            _required: &AttestRequirement,
            _generation: u64,
            _nonce: &[u8; 32],
        ) -> Result<Discharge, String> {
            Err("ceremony failed: human declined".into())
        }
    }

    /// The #61 orchestration helper: a provider that produces a valid passkey
    /// discharge drives `evaluate→obtain→authorize` to a minted context and a
    /// recorded attestation in a single call.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn provider_obtain_then_authorize_mints_and_records() {
        let gate = Gate::new(0).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let provider = MockProvider {
            key: test_key(),
            presence: Presence::Passkey,
        };
        let (cx, attestation) = gate
            .authorize_step_up(
                &tool,
                &granted,
                &push_request(),
                &push_policy(),
                &provider,
                &Ed25519Verifier,
                [11u8; 32],
            )
            .expect("a valid provider discharge authorizes");
        assert!(cx.caveats().leq(&granted));
        let attestation = attestation.expect("passkey+record must produce an attestation");
        assert_eq!(attestation.tool, "git.push");
    }

    /// Fail-closed: a provider whose ceremony errors makes the helper deny and
    /// mint/charge nothing — the single budgeted call survives (mirrors
    /// `needs_discharge_does_not_mint_or_charge`).
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn provider_error_fails_closed() {
        let gate = Gate::with_budget(0, CountBound::AtMost(1)).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let err = gate
            .authorize_step_up(
                &tool,
                &granted,
                &push_request(),
                &push_policy(),
                &FailingProvider,
                &Ed25519Verifier,
                [12u8; 32],
            )
            .expect_err("a failed ceremony is fail-closed");
        assert!(matches!(err, ToolError::Denied { .. }));
        // The single budgeted call is untouched — proving nothing was charged.
        let free = NamedTool("free.tool");
        match gate.evaluate(
            &free,
            &granted,
            &CallRequest::unspecified("free.tool"),
            &StepUpPolicy::EMPTY,
        ) {
            Decision::Allow(cx) => assert!(cx.caveats().leq(&granted)),
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    /// The verifier — not the provider — decides: a provider that only achieves
    /// `Prompt` cannot satisfy the policy's `Passkey` requirement, even though it
    /// returned `Ok`. The gate re-checks presence regardless of what the
    /// provider claimed (ADR 0007 D5).
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn provider_below_required_presence_is_denied() {
        let gate = Gate::new(0).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let provider = MockProvider {
            key: test_key(),
            presence: Presence::Prompt,
        };
        let err = gate
            .authorize_step_up(
                &tool,
                &granted,
                &push_request(),
                &push_policy(),
                &provider,
                &Ed25519Verifier,
                [13u8; 32],
            )
            .expect_err("a Prompt provider cannot satisfy a Passkey policy");
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    /// When no step-up is owed, the helper degenerates to an ordinary authorize:
    /// it never runs the ceremony and returns no attestation.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn authorize_step_up_without_gesture_degenerates_to_authorize() {
        let gate = Gate::new(0).with_session(test_session());
        let granted = Caveats::top();
        let free = NamedTool("free.tool");
        let provider = MockProvider {
            key: test_key(),
            presence: Presence::Passkey,
        };
        let (cx, attestation) = gate
            .authorize_step_up(
                &free,
                &granted,
                &CallRequest::unspecified("free.tool"),
                &StepUpPolicy::EMPTY,
                &provider,
                &Ed25519Verifier,
                [14u8; 32],
            )
            .expect("no gesture owed → ordinary authorize");
        assert!(cx.caveats().leq(&granted));
        assert!(attestation.is_none(), "no step-up → no attestation");
    }

    /// Anti-theater: a discharge signed for a DIFFERENT action (different nonce)
    /// is rejected. This fails only because the challenge is bound to the exact
    /// action — a generic gesture would pass.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn wrong_challenge_is_denied() {
        let gate = Gate::new(0).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        // Sign over a different nonce than the one the gate will recompute with.
        let discharge = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &[1u8; 32],
            Presence::Passkey,
        );
        let attempt = DischargeAttempt {
            nonce: [2u8; 32], // gate's nonce differs → expected challenge differs
            discharge: &discharge,
            verifier: &Ed25519Verifier,
        };
        let err = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt)
            .expect_err("mismatched challenge must be denied");
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    /// Fail-closed: a too-weak gesture (Prompt) cannot satisfy a Passkey
    /// requirement.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn presence_too_weak_fails_closed() {
        let gate = Gate::new(0).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        let nonce = [4u8; 32];
        // Correct challenge, but only Prompt strength.
        let discharge = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &nonce,
            Presence::Prompt,
        );
        let attempt = DischargeAttempt {
            nonce,
            discharge: &discharge,
            verifier: &Ed25519Verifier,
        };
        let err = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt)
            .expect_err("Prompt cannot satisfy Passkey");
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    /// ADR 0007 D2/D3 (design §10 Q3): the **no-authenticator** case. A
    /// `Presence::None` discharge — "no hardware gesture was achievable" — over
    /// the *correctly bound* challenge still cannot satisfy a `Passkey`
    /// requirement. This isolates the presence floor (the challenge matches, so
    /// the only reason to deny is presence), and is distinct from
    /// `presence_too_weak_fails_closed` (which covers `Prompt`): it proves the
    /// gate fails closed and never silently downgrades when *no* presence is
    /// achievable, rather than only when a weaker-but-nonzero one is.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn no_authenticator_presence_none_cannot_satisfy_passkey() {
        let gate = Gate::new(0).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        let nonce = [5u8; 32];
        // Correctly bound challenge (same nonce the gate recomputes with), but
        // the achieved presence is None — the "no authenticator available" case.
        let discharge = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &nonce,
            Presence::None,
        );
        let attempt = DischargeAttempt {
            nonce,
            discharge: &discharge,
            verifier: &Ed25519Verifier,
        };
        let err = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt)
            .expect_err("Presence::None cannot satisfy Passkey");
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    /// #62 acceptance: the **production** [`Ed25519Verifier`] accepts a discharge
    /// signed over the bound challenge (and mints), rejects one signed over a
    /// different nonce (anti-theater), and rejects a `Prompt` gesture against a
    /// `Passkey` requirement (fail-closed). This exercises the public, exported
    /// type — no test-only verifier exists on this path.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn ed25519_verifier_accepts_valid_and_rejects_wrong_challenge() {
        let gate = Gate::new(0).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();

        // Accept: a passkey discharge over the gate-recomputed challenge mints.
        let nonce = [21u8; 32];
        let ok = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &nonce,
            Presence::Passkey,
        );
        let attempt = DischargeAttempt {
            nonce,
            discharge: &ok,
            verifier: &Ed25519Verifier,
        };
        let (cx, attestation) = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt)
            .expect("a valid ed25519 discharge authorizes");
        assert!(cx.caveats().leq(&granted));
        assert!(
            attestation.is_some(),
            "passkey+record produces an attestation"
        );

        // Reject: signed over a different nonce than the gate recomputes.
        let bad = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &[99u8; 32],
            Presence::Passkey,
        );
        let bad_attempt = DischargeAttempt {
            nonce: [22u8; 32], // gate's nonce differs → expected challenge differs
            discharge: &bad,
            verifier: &Ed25519Verifier,
        };
        let err = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &bad_attempt)
            .expect_err("wrong challenge is denied");
        assert!(matches!(err, ToolError::Denied { .. }));

        // Reject: a Prompt gesture cannot satisfy a Passkey requirement.
        let weak_nonce = [23u8; 32];
        let weak = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &weak_nonce,
            Presence::Prompt,
        );
        let weak_attempt = DischargeAttempt {
            nonce: weak_nonce,
            discharge: &weak,
            verifier: &Ed25519Verifier,
        };
        let err = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &weak_attempt)
            .expect_err("Prompt cannot satisfy Passkey");
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    /// #63 regression: a verified discharge is **single-use** — re-presenting the
    /// same valid `DischargeAttempt` is denied as a replay, and the replay charges
    /// no budget. This FAILS on the pre-ledger code (which minted twice).
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn discharge_is_single_use_replay_is_denied() {
        let gate = Gate::with_budget(0, CountBound::AtMost(2)).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        let nonce = [31u8; 32];
        let discharge = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &nonce,
            Presence::Passkey,
        );
        let attempt = DischargeAttempt {
            nonce,
            discharge: &discharge,
            verifier: &Ed25519Verifier,
        };
        // First presentation authorizes (spends 1 of 2 budget).
        gate.authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt)
            .expect("first discharge authorizes");
        // Re-presenting the SAME discharge is a replay → denied.
        let err = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt)
            .expect_err("replay of the same discharge is denied");
        assert!(matches!(err, ToolError::Denied { .. }));
        // The replay charged no budget: a budgeted call still remains.
        let free = NamedTool("free.tool");
        match gate.evaluate(
            &free,
            &granted,
            &CallRequest::unspecified("free.tool"),
            &StepUpPolicy::EMPTY,
        ) {
            Decision::Allow(_) => {}
            other => panic!("expected Allow (budget survived the replay), got {other:?}"),
        }
    }

    /// v0.8 cross-lifetime replay defense: a discharge obtained under one
    /// [`SessionId`] must NOT authenticate a *different* session — even with the
    /// identical action, generation, and nonce, and even against a gate whose
    /// single-use `consumed` set has never seen it (as a recreated gate's would
    /// be empty). The session is folded into the challenge preimage, so the old
    /// discharge answers a challenge the second gate never asks.
    ///
    /// This FAILS on the pre-session code: without a `SessionId` in `bind`, the
    /// two gates recompute byte-identical challenges and the captured discharge
    /// verifies — exactly the replay across Registry/process lifetime this closes.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn a_discharge_bound_to_one_session_is_rejected_by_another() {
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        let nonce = [42u8; 32];

        // Session A issues + accepts a valid discharge.
        let session_a = SessionId::new([0xAA; 32]);
        let gate_a = Gate::new(0).with_session(session_a);
        let discharge = sign_discharge(&session_a, &test_key(), &req, 0, &nonce, Presence::Passkey);
        let attempt_a = DischargeAttempt {
            nonce,
            discharge: &discharge,
            verifier: &Ed25519Verifier,
        };
        gate_a
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt_a)
            .expect("gate A accepts a discharge for its own session");

        // A distinct session B (a second/recreated gate — fresh, empty consumed
        // set) is presented the SAME discharge, same action/generation/nonce.
        let gate_b = Gate::new(0).with_session(SessionId::new([0xBB; 32]));
        let attempt_b = DischargeAttempt {
            nonce,
            discharge: &discharge,
            verifier: &Ed25519Verifier,
        };
        let err = gate_b
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt_b)
            .expect_err("a discharge from session A must not authenticate session B");
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    /// v0.8 fail-closed default: a gate with no [`SessionId`] bound refuses every
    /// discharge — it cannot domain-separate its challenges, so it must not
    /// pretend to verify one. The refusal precedes verification and charging.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn an_unsessioned_gate_refuses_every_discharge() {
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        let nonce = [7u8; 32];
        // A perfectly valid discharge for `test_session()` — the point is that the
        // gate never bound a session, so it cannot accept ANY discharge.
        let discharge = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &nonce,
            Presence::Passkey,
        );
        let attempt = DischargeAttempt {
            nonce,
            discharge: &discharge,
            verifier: &Ed25519Verifier,
        };
        let gate = Gate::new(0); // no .with_session(..)
        let err = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt)
            .expect_err("an unsessioned gate must refuse a discharge");
        match err {
            ToolError::Denied { reason } => {
                assert!(reason.contains("session"), "reason was: {reason}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    /// #63: `freshness_generations: 0` requires the *current* generation — a
    /// discharge bound to an earlier generation is denied (the default
    /// `passkey_recorded()` requirement is freshness 0).
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn freshness_generations_zero_requires_current_generation() {
        let gate = Gate::new(1).with_session(test_session()); // current generation is 1
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        let nonce = [32u8; 32];
        // Signed for generation 0 — one behind the gate, outside a zero window.
        let stale = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &nonce,
            Presence::Passkey,
        );
        let attempt = DischargeAttempt {
            nonce,
            discharge: &stale,
            verifier: &Ed25519Verifier,
        };
        let err = gate
            .authorize_with_discharge(&tool, &granted, &req, &push_policy(), &attempt)
            .expect_err("a stale discharge fails freshness_generations: 0");
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    /// #63: with `freshness_generations: 1`, a discharge bound to the previous
    /// generation IS accepted — proving the field demonstrably affects behavior
    /// (the window includes generation `g-1`).
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn freshness_generations_window_accepts_recent() {
        let gate = Gate::new(1).with_session(test_session());
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        let policy = StepUpPolicy::new(
            vec![Rule {
                selector: "git.push:github.com/org/*".to_string(),
                requirement: AttestRequirement {
                    presence: Presence::Passkey,
                    record: true,
                    freshness_generations: 1,
                },
            }],
            AttestRequirement::NONE,
        );
        let nonce = [33u8; 32];
        // Signed for generation 0; gate at 1, window = 1 → in range.
        let recent = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &nonce,
            Presence::Passkey,
        );
        let attempt = DischargeAttempt {
            nonce,
            discharge: &recent,
            verifier: &Ed25519Verifier,
        };
        let (cx, attestation) = gate
            .authorize_with_discharge(&tool, &granted, &req, &policy, &attempt)
            .expect("a one-generation-old discharge is within the window");
        assert!(cx.caveats().leq(&granted));
        assert!(attestation.is_some());
    }

    /// #63: single-use is concurrency-safe — two threads presenting the SAME
    /// discharge against one shared gate yield exactly one success.
    #[cfg(feature = "verifier-ed25519")]
    #[test]
    fn discharge_single_use_is_concurrency_safe() {
        use std::sync::Arc;
        use std::thread;

        let gate =
            Arc::new(Gate::with_budget(0, CountBound::AtMost(2)).with_session(test_session()));
        let granted = Caveats::top();
        let tool = NamedTool("git.push");
        let req = push_request();
        let policy = push_policy();
        let nonce = [34u8; 32];
        let discharge = sign_discharge(
            &test_session(),
            &test_key(),
            &req,
            0,
            &nonce,
            Presence::Passkey,
        );

        let (r1, r2) = thread::scope(|s| {
            let h1 = s.spawn(|| {
                let attempt = DischargeAttempt {
                    nonce,
                    discharge: &discharge,
                    verifier: &Ed25519Verifier,
                };
                gate.authorize_with_discharge(&tool, &granted, &req, &policy, &attempt)
                    .is_ok()
            });
            let h2 = s.spawn(|| {
                let attempt = DischargeAttempt {
                    nonce,
                    discharge: &discharge,
                    verifier: &Ed25519Verifier,
                };
                gate.authorize_with_discharge(&tool, &granted, &req, &policy, &attempt)
                    .is_ok()
            });
            (h1.join().unwrap(), h2.join().unwrap())
        });
        assert_eq!(
            [r1, r2].iter().filter(|ok| **ok).count(),
            1,
            "exactly one of two concurrent identical discharges succeeds"
        );
    }

    #[test]
    fn policy_most_specific_wins_and_default_applies() {
        let policy = StepUpPolicy::new(
            vec![
                Rule {
                    selector: "fs.delete:/tmp/*".to_string(),
                    requirement: AttestRequirement::presence(Presence::Prompt),
                },
                Rule {
                    selector: "fs.delete:/tmp/important/*".to_string(),
                    requirement: AttestRequirement::presence(Presence::Passkey),
                },
            ],
            AttestRequirement::NONE,
        );
        // Longer selector wins for the nested path.
        let nested = CallRequest::new("fs.delete", serde_json::Value::Null, "/tmp/important/x");
        assert_eq!(policy.required_for(&nested).presence, Presence::Passkey);
        // Broad path matches only the short rule.
        let broad = CallRequest::new("fs.delete", serde_json::Value::Null, "/tmp/scratch");
        assert_eq!(policy.required_for(&broad).presence, Presence::Prompt);
        // Unmatched tool falls through to the default.
        let other = CallRequest::new("email.read", serde_json::Value::Null, "inbox");
        assert_eq!(policy.required_for(&other).presence, Presence::None);
    }

    // ── WebAuthn verifier (#72, verifier-webauthn) ───────────────────────────

    /// authenticatorData flag bits.
    #[cfg(feature = "verifier-webauthn")]
    const UP: u8 = 0x01; // User Present
    #[cfg(feature = "verifier-webauthn")]
    const UV: u8 = 0x04; // User Verified

    /// Build a WebAuthn (EdDSA) assertion discharge over `challenge`: a
    /// `webauthn.get` clientDataJSON carrying the base64url challenge, a 37-byte
    /// authenticatorData whose flag byte is `flags`, and an Ed25519 signature
    /// over `authData ‖ SHA-256(clientDataJSON)`. A fixture builder — no live
    /// authenticator (acceptance criteria: canned vectors, no hardware).
    #[cfg(feature = "verifier-webauthn")]
    fn webauthn_discharge(
        key: &SigningKey,
        challenge: &Challenge,
        presence: Presence,
        flags: u8,
    ) -> Discharge {
        use sha2::{Digest, Sha256};
        let client_data = format!(
            r#"{{"type":"webauthn.get","challenge":"{}","origin":"https://example.org"}}"#,
            base64url_nopad(challenge.as_bytes())
        )
        .into_bytes();
        // rpIdHash[32] ‖ flags[1] ‖ signCount[4]
        let mut auth_data = vec![0u8; 37];
        auth_data[32] = flags;
        let mut signed = auth_data.clone();
        signed.extend_from_slice(&Sha256::digest(&client_data));
        let sig = key.sign(&signed);
        Discharge {
            presence,
            credential_id: key.verifying_key().to_bytes().to_vec(),
            challenge: *challenge.as_bytes(),
            signature: sig.to_bytes().to_vec(),
            authenticator_data: Some(auth_data),
            client_data_json: Some(client_data),
        }
    }

    #[cfg(feature = "verifier-webauthn")]
    fn webauthn_challenge(nonce: u8) -> Challenge {
        Challenge::bind(
            &test_session(),
            &push_request().content_id(),
            0,
            &[nonce; 32],
        )
    }

    #[cfg(feature = "verifier-webauthn")]
    #[test]
    fn webauthn_accepts_valid_passkey_assertion() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let challenge = webauthn_challenge(3);
        let d = webauthn_discharge(&key, &challenge, Presence::Passkey, UP | UV);
        assert!(WebAuthnVerifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &challenge
            )
            .is_ok());
    }

    #[cfg(feature = "verifier-webauthn")]
    #[test]
    fn webauthn_rejects_tampered_challenge() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        // Signed over one challenge; the gate recomputed a different one.
        let d = webauthn_discharge(&key, &webauthn_challenge(3), Presence::Passkey, UP | UV);
        let err = WebAuthnVerifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &webauthn_challenge(4),
            )
            .unwrap_err();
        assert!(err.contains("challenge"), "{err}");
    }

    #[cfg(feature = "verifier-webauthn")]
    #[test]
    fn webauthn_rejects_cleared_up_flag() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let challenge = webauthn_challenge(3);
        // UV set but UP cleared — UP is mandatory, so reject.
        let d = webauthn_discharge(&key, &challenge, Presence::Passkey, UV);
        let err = WebAuthnVerifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &challenge,
            )
            .unwrap_err();
        assert!(err.contains("User-Presence"), "{err}");
    }

    #[cfg(feature = "verifier-webauthn")]
    #[test]
    fn webauthn_passkey_requires_uv() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let challenge = webauthn_challenge(3);
        // UP set, UV clear: fine for a Prompt requirement, but a Passkey
        // requirement demands the verified gesture.
        let d = webauthn_discharge(&key, &challenge, Presence::Passkey, UP);
        let err = WebAuthnVerifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &challenge,
            )
            .unwrap_err();
        assert!(err.contains("User-Verification"), "{err}");
    }

    /// A `Prompt`-strength discharge can never satisfy a `Passkey` requirement —
    /// rejected before any crypto (mirrors `presence_too_weak_fails_closed`).
    #[cfg(feature = "verifier-webauthn")]
    #[test]
    fn webauthn_presence_too_weak_fails_closed() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let challenge = webauthn_challenge(3);
        let d = webauthn_discharge(&key, &challenge, Presence::Prompt, UP | UV);
        let err = WebAuthnVerifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &challenge,
            )
            .unwrap_err();
        assert!(err.contains("presence"), "{err}");
    }

    #[cfg(feature = "verifier-webauthn")]
    #[test]
    fn webauthn_rejects_forged_signature() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let challenge = webauthn_challenge(3);
        let mut d = webauthn_discharge(&key, &challenge, Presence::Passkey, UP | UV);
        d.signature[0] ^= 0xff; // a one-bit-flipped (forged) signature
        assert!(WebAuthnVerifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &challenge
            )
            .is_err());
    }

    // ── ES256 (COSE alg -7) — the real-platform-passkey sibling ──────────────
    // Mirrors the EdDSA WebAuthn vectors with a deterministic P-256 signer, so
    // the shared gating is proven identical and the ES256 signature suite works.

    #[cfg(feature = "verifier-webauthn-es256")]
    fn es256_key() -> p256::ecdsa::SigningKey {
        // Fixed scalar → deterministic (RFC6979) signatures; no hardware/rng.
        p256::ecdsa::SigningKey::from_slice(&[0x42u8; 32]).unwrap()
    }

    #[cfg(feature = "verifier-webauthn-es256")]
    fn es256_challenge(nonce: u8) -> Challenge {
        Challenge::bind(
            &test_session(),
            &CallRequest::unspecified("es256.test").content_id(),
            0,
            &[nonce; 32],
        )
    }

    /// Build a WebAuthn (ES256) assertion over `challenge`: `webauthn.get`
    /// clientDataJSON, a 37-byte authenticatorData with flag byte `flags`, and an
    /// ASN.1-DER ECDSA-P256 signature over `authData ‖ SHA-256(clientDataJSON)`.
    #[cfg(feature = "verifier-webauthn-es256")]
    fn webauthn_es256_discharge(
        key: &p256::ecdsa::SigningKey,
        challenge: &Challenge,
        presence: Presence,
        flags: u8,
    ) -> Discharge {
        use p256::ecdsa::{signature::Signer, Signature};
        use sha2::{Digest, Sha256};
        let client_data = format!(
            r#"{{"type":"webauthn.get","challenge":"{}","origin":"https://example.org"}}"#,
            base64url_nopad(challenge.as_bytes())
        )
        .into_bytes();
        let mut auth_data = vec![0u8; 37];
        auth_data[32] = flags;
        let mut signed = auth_data.clone();
        signed.extend_from_slice(&Sha256::digest(&client_data));
        let sig: Signature = key.sign(&signed);
        Discharge {
            presence,
            credential_id: key
                .verifying_key()
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            challenge: *challenge.as_bytes(),
            signature: sig.to_der().as_bytes().to_vec(),
            authenticator_data: Some(auth_data),
            client_data_json: Some(client_data),
        }
    }

    #[cfg(feature = "verifier-webauthn-es256")]
    #[test]
    fn webauthn_es256_accepts_valid_passkey_assertion() {
        let key = es256_key();
        let challenge = es256_challenge(3);
        let d = webauthn_es256_discharge(&key, &challenge, Presence::Passkey, 0x01 | 0x04);
        assert!(WebAuthnEs256Verifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &challenge
            )
            .is_ok());
    }

    #[cfg(feature = "verifier-webauthn-es256")]
    #[test]
    fn webauthn_es256_rejects_tampered_challenge() {
        let key = es256_key();
        let d = webauthn_es256_discharge(&key, &es256_challenge(3), Presence::Passkey, 0x05);
        let err = WebAuthnEs256Verifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &es256_challenge(4),
            )
            .unwrap_err();
        assert!(err.contains("challenge"), "{err}");
    }

    #[cfg(feature = "verifier-webauthn-es256")]
    #[test]
    fn webauthn_es256_passkey_requires_uv() {
        let key = es256_key();
        let challenge = es256_challenge(3);
        // UP set, UV clear — a Passkey requirement demands the verified gesture.
        let d = webauthn_es256_discharge(&key, &challenge, Presence::Passkey, 0x01);
        let err = WebAuthnEs256Verifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &challenge,
            )
            .unwrap_err();
        assert!(err.contains("User-Verification"), "{err}");
    }

    #[cfg(feature = "verifier-webauthn-es256")]
    #[test]
    fn webauthn_es256_rejects_forged_signature() {
        let key = es256_key();
        let challenge = es256_challenge(3);
        let mut d = webauthn_es256_discharge(&key, &challenge, Presence::Passkey, 0x05);
        let n = d.signature.len();
        d.signature[n - 1] ^= 0xff; // corrupt the DER signature
        assert!(WebAuthnEs256Verifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &challenge
            )
            .is_err());
    }

    #[cfg(feature = "verifier-webauthn-es256")]
    #[test]
    fn webauthn_es256_rejects_wrong_key() {
        // An assertion by one key must not verify against another's public key.
        let challenge = es256_challenge(3);
        let mut d = webauthn_es256_discharge(&es256_key(), &challenge, Presence::Passkey, 0x05);
        let other = p256::ecdsa::SigningKey::from_slice(&[0x43u8; 32]).unwrap();
        d.credential_id = other
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        assert!(WebAuthnEs256Verifier
            .verify(
                &d,
                &AttestRequirement::presence(Presence::Passkey),
                &challenge
            )
            .is_err());
    }
}

# Human-Presence Capabilities

### Passkey step-up as an OCAP decision verb, and the human-rooted swarm bootstrap

**Status:** Design note (no implementation). 2026-06-18.
**Builds on:**
- `docs/design/ssh-ca-trust-root.md` (#455/#465) — the human's ed25519 SSH key as
  the swarm CA, attenuated OpenSSH member certs, `SshCaveats`/`GitCaveats`.
- `docs/decisions/agentic_object_capability_security.md` — the `Caveats` lattice
  and the mint-token enforcement invariant.
- `docs/design/config-scaling-deployment-and-trust.md` — multi-substrate config /
  trust scaling.
- The "Age of the Confused Deputy" paper, **§7.5** (Gilamonster knowledge#40) — the
  prose statement of this design; this note is its engineering plan of record.
- `agent-mesh-protocol/src/caveats.rs` (`meet`, `meet_never_amplifies`),
  `agent-bridle/agent-bridle-core/src/gate.rs` (`Gate::authorize`),
  `newt-core/src/{git_caveats,ssh_caveats,agent_identity}.rs`.

---

## TL;DR

- **One new decision verb, `attest`.** The leash decision becomes a polarity
  **`{allow, attest, deny}`** under a single **ephemeral⇄persistent** mutation.
  `attest` = "allow *iff* accompanied by a fresh human-presence proof" (a
  WebAuthn/FIDO2 assertion: YubiKey touch, Touch ID, Windows Hello). The operator
  menu — *yes once / yes always / yes on passkey / no once / no always* — is
  exactly these three verbs under one mutation, not five choices.
- **`attest` is not a new authority.** A discharge adds nothing to the grant
  (`effective = granted.meet(required)` is unchanged); it sharpens the *liveness
  condition* under which the **same** Writ is exercised. At the menu it reads as a
  third option; in the algebra it is a **constraint** — an extension of Refusal.
  So it cannot break attenuation.
- **The keystone — mutating policy is itself a capability.** Attenuating (a
  *deny*, a tighter rule) needs only ordinary authority; **amplifying (a standing
  *allow* that widens the down-set) requires the human root, via an `attest`.** An
  agent cannot loosen its own leash. This scales to the enterprise: an admin
  ships a **signed policy artifact**; a worker's permitted mutation range is
  `{none, attenuate-only, ephemeral-only, full}`.
- **The gate verifies a proof; it never performs the gesture.** `Gate::authorize`
  stays pure/sync. A new host capability (`DischargeProvider`, sibling of
  `Sandbox`) runs the ceremony; the gate calls a pure `DischargeVerifier`.
- **Passkeys cannot be the CA.** A passkey signs only the fixed
  `authenticatorData ‖ H(clientData)`, only ES256, only with a live gesture, and
  is non-exportable. It is the **unseal gate and presence proof, never the
  signer.** The swarm root stays a software ed25519 `UserKey`; Path B seals its
  private half under a passkey **PRF/`hmac-secret`** KEK. The GitHub key is an
  **identity anchor only** (Path A), never a `CertChain` signer.
- **Touch-to-push** is just `attest` wired to `git push`: a FIDO2 `ed25519-sk`
  key whose CA cert omits `no-touch-required`. **YubiKey is the only "just
  works"**; synced iCloud/Google passkeys and a built-in Touch ID **cannot** be
  SSH `sk-keys`. The guarantee is hardware-can't-sign **+** a server-side
  `pre-receive` hook — the bridle gate alone is advisory.
- **Four prerequisite fixes** (red-team findings) gate any genesis CLI: revocation
  is presently a no-op; there is no proof-of-possession path for an externally
  held agent key; the bridle MCP grant is unsigned and fails *open*; `permits_key`
  is unenforced on the push path.

---

## 1. The gap

`docs/decisions/agentic_object_capability_security.md` and the `Caveats` lattice
give a **two-valued** local decision: a tool call is within the grant's down-set
or it is not (`Gate::authorize` → `Ok(ToolContext)` / `Err(ToolError)`,
`gate.rs:88`). Operating the deputy across substrates — a homelab, AWS, Azure,
GCP, all under one human — surfaces a third outcome the binary cannot express:
*authorized, but only with a fresh, non-repudiable act of human presence.*

Two motivating user stories:

1. **Step-up on a dangerous effect.** An agent may read mail freely, but *sending*
   mail as the human, *deleting* a protected file, or *pushing* to a protected
   repo should demand a live human gesture — configurable per action.
2. **Bootstrapping the swarm root with no pre-existing key.** `ssh-ca-trust-root.md`
   assumes the operator already has an ed25519 key published at
   `github.com/<user>.keys`. What roots the swarm when they do not? A passkey can
   *mint and seal* the root, binding it to *something the human has*.

Both are the same primitive: **human presence as a first-class element of the
authority algebra.**

---

## 2. The decision model

### 2.1 Three decisions, one mutation

```
            ephemeral (once)         persistent (always)
 allow      yes once                 yes always           ← writes a standing rule
 attest     yes (w/ passkey) once    always require passkey
 deny       no once                  no always
```

`Presence` is a totally-ordered strength (the ordering is load-bearing, §2.4):

```rust
// agent-bridle-core/src/decision.rs (new)
pub enum Presence { None, Prompt, Passkey }   // None < Prompt < Passkey

pub struct AttestRequirement {
    pub presence: Presence,           // minimum gesture strength
    pub record: bool,                 // must a provenance attestation be recorded?
    pub freshness_generations: u64,   // max age in GENERATIONS, never wall-clock
}

pub enum Decision {
    Allow(ToolContext),               // mint — exactly as today
    Deny(ToolError),                  // fail-closed; always wins
    NeedsDischarge(AttestRequirement),// withhold the mint until a proof is presented
}
```

### 2.2 The gate split — evaluate vs. admit, one mint site

`Gate::authorize` is the only place a `ToolContext` is minted (`gate.rs:110`); it
is `&self`, synchronous, IO-free. We must keep it that way — an interactive
authenticator call inside the choke point would make it async, IO-bearing, and
TOCTOU-prone. So we **split evaluation from minting** and keep the gate a *proof
verifier*, never a *gesture performer*:

```rust
impl Gate {
    /// Pure, sync, no IO. Returns Allow when no step-up is owed,
    /// NeedsDischarge(req) when one is, Deny on any hard failure.
    pub fn evaluate(&self, tool, granted, request: &CallRequest,
                    policy: &StepUpPolicy) -> Decision;

    /// The step-up admission path. Verifies a discharge against the requirement
    /// and the EXACT request, then performs the ordinary authorize. Still the
    /// only mint site.
    pub fn authorize_with_discharge(&self, tool, granted, request, policy,
        discharge: &Discharge, verifier: &dyn DischargeVerifier)
        -> ToolResult<(ToolContext, Option<Attestation>)>;
}
```

The existing `authorize(tool, granted)` is preserved as a back-compat wrapper for
the no-step-up case (empty policy ⇒ `evaluate` only returns `Allow`/`Deny`), so
**every current caller and test compiles unchanged** — the existing gate suite is
the regression guard that least-authority/budget/generation are untouched.

### 2.3 Fail-closed composition

Internally, admission slots into the existing order *without disturbing it*
("deny before charging", `gate.rs:88-111`):

1. `effective = granted.meet(&tool.required())` — **unchanged.** The discharge is
   bound to the *effective* authority.
2. `check_generation(granted)?` — **unchanged**, before any charge.
3. **NEW** — step-up admission, *before* charging the budget so an invalid/missing
   discharge does not consume a call (mirrors `denied_request_does_not_charge_budget`,
   `gate.rs:300`). Any verify failure ⇒ `Err` ⇒ `Deny`. The new band can only
   *withhold* the mint, never grant it.
4. `charge_one(granted)?` then `ToolContext::mint(effective, sandbox_kind)`.

Three invariants the skeptic should check, all preserved: **Deny always wins**;
**least authority is untouched** (a passkey unlocks *use of already-granted*
authority, never *more*); **budget integrity** (a declined gesture burns no call).

### 2.4 Why `Presence` is `Ord`

`policy.required_for(request)` returns the **strongest** matching requirement; a
discharge satisfies it iff `discharge.presence >= required.presence`. A `Passkey`
over-satisfies a `Prompt`; a `Prompt` never satisfies a `Passkey`. This
monotonicity is what makes attenuation sound (§3): a sub-delegated grant can only
*raise* the floor.

---

## 3. `PresenceCaveats` — the third-party caveat, as a sibling lattice

"This action needs a fresh human gesture" is precisely a **macaroon third-party
caveat**: satisfied only if a *different* authority — the human's authenticator,
which the compromised agent cannot impersonate — issues a discharge. We model it
the way `GitCaveats`/`SshCaveats` already model their surfaces: a *separate* small
lattice composed *alongside* the signed `Caveats` by `meet`, never merged into the
wire type (so no protocol bump).

```rust
// newt-core/src/presence_caveats.rs (sibling of git_caveats.rs / ssh_caveats.rs)
pub struct PresenceCaveats {
    pub floors: BTreeMap<ActionClass, Presence>, // per-class floor
    pub default: Presence,                        // floor for unlisted classes
}
```

**Orientation (the crux):** a *higher* presence floor means *more* restriction, so
this lattice is oriented opposite to the others — `top()` is the least presence
(`None`), and **`meet` takes the per-class MAX of the two floors**. Max can only
hold or rise, so:

> `child = parent.meet(delegation)` always has `child.floor(c) >= parent.floor(c)`
> — **a child can only ADD presence requirements, never remove them.**

This is the dual of `Scope::meet` (intersection) and `CountBound::meet` (min): meet
is the greatest lower bound *in the authority order*, and "more gesture required" is
*lower authority*. Same teeth as `meet_never_amplifies` (`caveats.rs`), with the same
property test: `presence_meet_never_lowers_a_floor`. **Refusal composes downward;
permission never does.** A worker the root gave `git.push` to, re-delegating to a
phone, cannot strip the `passkey` floor — the phone's effective floor is
`max(root, worker, phone)`.

---

## 4. WYSIWYS challenge binding (anti-theater)

A generic "Approve?" gesture an agent can harvest and replay is theater. The
discharge challenge is bound to the *specific* act:

```
challenge = BLAKE3( DOMAIN_SEP            // b"agent-bridle/step-up/v1"
                  ‖ tool_name             // "email.send"
                  ‖ canonical(args)       // RFC 8785 JCS, RESOLVED first
                  ‖ resource              // the matched selector (recipients, refspec, realpath)
                  ‖ generation.to_le()    // causal coordinate — binds to THIS flight
                  ‖ nonce )               // 32 random bytes, single-use
```

- **BLAKE3** because it is already the workspace content-address primitive (kyln,
  agent-mesh `fingerprint.rs`) — the attestation drops into existing provenance
  machinery with no new hash.
- **Resolve before hashing.** `fs.delete` paths are canonicalized exactly as
  `check_path_write` does (`context.rs` `canonicalize_for_check` — symlink-resolved,
  `..`-rejected) *before* entering the challenge, and the human-readable prompt is a
  deterministic function of the same canonical form. Otherwise the human approves
  `/work/../etc/passwd` thinking it is a `/work` file — *what you see is what you sign*
  fails.

The verified assertion becomes a content-addressed **provenance attestation**
(Charter: Provenance / Scar), appended to the kyln / agent-store causal log:

```rust
pub struct Attestation {
    pub schema: &'static str,          // "agent-bridle/attestation/v1"
    pub tool: String, pub resource: String,
    pub challenge: [u8; 32],           // binds to the exact action
    pub generation: u64,               // causal, not wall-clock
    pub credential_id: Vec<u8>,        // WHICH authenticator
    pub authenticator_data: Vec<u8>,   // UP/UV flags, sign count
    pub signature: Vec<u8>,
    pub presence: Presence,            // strength actually achieved (UP vs UV)
}
```

---

## 5. Enforcement planes

Four distinct planes; conflating them is the confused-deputy error this design
exists to avoid:

| Plane | Where | What | Trust |
|---|---|---|---|
| **Capability carriage** | newt-core `PresenceCaveats` (+ `Caveats`/`Git`/`Ssh`) | carries the floor along the delegation chain; `meet` non-amplifying | attenuation-only |
| **Per-call step-up gate** | agent-bridle `Gate::evaluate`/`authorize_with_discharge` | computes the requirement, *verifies a proof* | **client-advisory** (real for an honest binary; a patched agent can skip it) |
| **The ceremony** | host `DischargeProvider` (sibling of `Sandbox`) | invokes the platform authenticator (CTAP2 / Apple / Windows). Async, IO-bearing | trusted to *perform*, cannot *forge* (no key material) |
| **Effect-bound verification** | a verifier the client doesn't control (git `pre-receive`, send-relay, mesh peer) | re-derives the challenge from the *actual* effect; rejects unless a valid attestation rides along | **the real teeth** — holds against a fully patched client |

```rust
// HOST capability (async, IO) — sibling of Sandbox; lives OUTSIDE the pure core.
pub trait DischargeProvider {
    fn best_available_presence(&self) -> Presence;            // honest degradation
    async fn discharge(&self, challenge: &[u8;32], req: &AttestRequirement)
        -> Result<Discharge>;
}
// PURE verifier the gate uses — no ceremony, just checks a proof.
pub trait DischargeVerifier {
    fn verify(&self, d: &Discharge, req: &AttestRequirement,
              request: &CallRequest, generation: u64)
        -> Result<Option<Attestation>, String>;
}
```

`best_available_presence()` is the honest-degradation analogue of
`best_available_sandbox()` (`sandbox.rs`): a headless CI host that *cannot* gesture
reports `None`, and a `passkey`-required action then **fails closed** (Charter:
Novice) — you cannot autonomously discharge a human-presence caveat; that is the
point.

**Which action needs which plane.** Reversible/low-stakes (read mail,
`fs.delete:/tmp/**`): client gate only; `prompt` is advisory UX. Irreversible
(send mail, `git push` to a protected repo, delete under `/work/important`):
client gate **plus** effect-bound verification — the `passkey+record` decision
exists precisely so the recorded attestation can ride to the independent verifier.
**`passkey` on an irreversible action without the effect plane is theater against a
patched client.**

---

## 6. The policy config surface

```toml
# ~/.newt/step-up.toml — resolved like caveats_source.rs: env > file > default
[defaults]
read  = "allow"      # read-class: no gesture
write = "prompt"     # any unmatched write-class: at least a soft prompt (fail-closed)

# MOST-SPECIFIC-WINS. decision ∈ allow | prompt | passkey | passkey+record
[[rule]] selector = "email.read"                   decision = "allow"
[[rule]] selector = "email.send"                   decision = "passkey"
[[rule]] selector = "fs.delete:/work/important/**" decision = "passkey"
[[rule]] selector = "fs.delete:/tmp/**"            decision = "prompt"
[[rule]] selector = "git.push:github.com/org/*"    decision = "passkey+record"
```

| `decision` | `Presence` | `record` |
|---|---|---|
| `allow` | None | false |
| `prompt` | Prompt | false |
| `passkey` | Passkey | false |
| `passkey+record` | Passkey | true |

The resolved floor is **clamped upward** by any `PresenceCaveats` riding the grant:
`effective_floor = max(policy_floor, presence_caveats.floor(class))`. A delegated
grant can demand *more* gesture than local policy, never less. This is where the
config surface and the lattice meet — and it is the same **declare-then-allow**
shape as agent-bridle's `leash()`, one level up: `leash()` answers *is the
authority granted at all?*; step-up answers *what gesture admits its use?*.

### 6.1 The self-governing keystone, and the enterprise

The menu's *always* answers **write rules** — i.e. they *mutate the policy object*.
That is not neutral: a standing *allow* **amplifies** future authority, the one move
the lattice forbids. So **policy mutation is itself capability-governed**:

- **Attenuate** (write a *deny*, raise a floor): ordinary authority — always allowed
  (the dual of "a deputy may shrink its own writ").
- **Amplify** (write a standing *allow* that widens the down-set, or *lower* a
  presence floor): requires the **human root**, surfaced as an `attest` gated by a
  passkey. An agent cannot loosen its own leash.

For the enterprise, an administrator ships a **signed policy artifact** — the exact
grant workers receive — and withholds the mutation capability. A worker's permitted
mutation range is one of `{none, attenuate-only, ephemeral-only, full}`:
- *"only the admin may alter persistent state"* → worker = `ephemeral-only`.
- *"only the admin may alter the policy at all"* → worker = `none` (a frozen grant).

This is MDM / group-policy / IT-shipped-sudoers, rebuilt as an attenuation-only
lattice with a human-presence root — and it is the **structural fix** for a real
gap: today the bridle MCP boundary loads its grant from unsigned
`$AGENT_BRIDLE_CAVEATS` (→ config → `Caveats::top()` **UNCONFINED**), a
*fail-open* default. The shipped artifact must be **signed, admin-rooted, and
fail-closed**.

---

## 7. The root-of-trust bootstrap

Identity is ed25519 everywhere; the root is a per-user `UserKey` (PKCS#8 PEM,
`0600`); agents are ephemeral `AgentKey`s certified into a `CertChain`; trust is
flat-per-user (peers trust iff both chains root to the same user pubkey).
`AgentIdentity.signing_key` (`agent_identity.rs:210`) is a *path* to that PEM,
"never inline key material."

### 7.1 Path A — GitHub key as the **anchor** (not the CA)

The human's published key (`github.com/<user>.keys` over HTTPS — SSH:22 is
sandbox-blocked) **anchors identity**: pin **one** designated key id immutably in
the genesis record and cross-sign the root pubkey's fingerprint with it, so any peer
can verify "this root is the human's." It is **not** the CA — an SSH key cannot sign
a `CertChain`, and `.keys` returns *all* of a user's device keys with no role tag.
Verify genesis only against the **pinned** key, never by rebuilding an allow-list
from live `.keys` (else compromise of *any* device key mints a rogue genesis).

### 7.2 Path B — passkey **seals** the root (the no-key bootstrap)

Generate the software ed25519 `UserKey` normally; **seal its private half at rest**
under a KEK derived from the authenticator's **PRF / `hmac-secret`** extension:

```
KEK = HMAC-SHA256( CredRandom, SHA-256("WebAuthn PRF" ‖ 0x00 ‖ developerSalt) )
```

stable for a given (credential, salt). Unseal requires a live gesture; the unsealed
software key then does all unattended cert-issuance. The passkey is **structurally
disqualified as the CA** — it signs only `authenticatorData ‖ H(clientData)`, only
ES256, only with a gesture, and is non-exportable. **It is the unseal gate and a
presence proof, never the signer.**

> Hardware-binding caveat: the `passkey-rs` crate is a *software/virtual*
> authenticator — its `CredRandom` lives in *your* `CredentialStore`, i.e. on the
> same disk as the sealed key (**zero hardware binding**). For a real hardware seal,
> drive **libfido2/CTAP2** against a physical YubiKey for the PRF eval.

### 7.3 Cross-cloud enrollment

An agent booting in AWS/Azure/GCP/homelab generates its own `AgentKey` and requests
enrollment; the enrollment is authorized by a signature chaining to the human root
(Path A anchor + Path B-sealed `UserKey`). **Blocked by prerequisites in §9** —
notably there is no path today to certify an *externally held* agent pubkey with
proof-of-possession, and discovery is mDNS/LAN-only (no WAN reachability yet).

### 7.4 Touch-to-push (the canonical first `attest`)

`git push` to a protected repo as the most common irreversible effect:

- **Mechanism:** a FIDO2 `ed25519-sk` key (default touch, or `-O verify-required`
  for PIN/biometric UV); the CA signs the member cert **without** `no-touch-required`,
  so `sshd` (default `touch-required`, optionally `PubkeyAuthOptions verify-required`)
  forces a fresh gesture at **SSH-auth time, per push** (disable `ControlMaster`
  multiplexing for the host, or a 2nd push reuses the channel without re-touch).
- **Hardware reality (firm boundaries):** **YubiKey 5 is the only "just works."**
  **Synced iCloud/Google passkeys cannot be SSH `sk-keys`** (not exposed over
  CTAP2/libfido2). **Built-in Touch ID is not an `sk-key`** — Secretive gives a
  Secure-Enclave *ecdsa P-256* key with a *local* biometric prompt (no server-attested
  presence; no `verify-required` cert interplay). Windows Hello works partially.
- **Software gate is theater.** `passkey-rs` does **not** drive Touch ID / Windows
  Hello; `webauthn-rs` is the right *verifier*; only `windows-rs`
  (`WebAuthNAuthenticatorGetAssertion`) gives a CLI a real platform assertion — macOS
  needs an entitled helper app. So an Apple/Windows-only operator uses the
  **push-bound ceremony verified server-side**, not a pure CLI gesture.
- **The teeth are off-box:** a `pre-receive` hook recomputes
  `BLAKE3(remote ‖ ref ‖ new_OID ‖ old_OID ‖ nonce)` and rejects the ref update
  unless a UV-attested assertion against the enrolled credential rides along. A
  patched client can forge the *prompt* but not the *signature*.

**Enforcement plane for push lives in newt-core's SSH transport + the signed cert,
NOT the bridle leash** (same-trust-domain software the agent can patch). agent-bridle's
role is secondary: deny-by-default so `git push` is *only* reachable through the gated
transport (no raw push via an unleashed shell), and forbid copying any non-hardware
signing key. Attach a `presence` axis to `SshCaveats`, thread it into
`git_over_ssh_permitted` (`ssh_caveats.rs:121`), and propagate it into the cert
extensions per `ssh-ca-trust-root.md` §2.

---

## 8. Crate ownership

| Piece | Home |
|---|---|
| `Presence`, `AttestRequirement`, `Decision`, `Attestation`, `DischargeVerifier` | `agent-bridle-core` (pure, sync) |
| `Gate::evaluate` / `authorize_with_discharge` | `agent-bridle-core` |
| `DischargeProvider` (the ceremony; CTAP2/Apple/Windows impls) | host crate (newt / agent-bridle-mcp), async, outside core |
| `PresenceCaveats` lattice | `newt-core` (sibling of git/ssh caveats) |
| `StepUpPolicy` loader (env > file > default) | mirrors `agent-bridle-mcp/src/caveats_source.rs` |
| Presence axis on `SshCaveats` + cert extension propagation | `newt-core` + agent-mesh (per ssh-ca doc §10) |
| Effect verifier (`pre-receive`) | deploy artifact on the protected remote |

---

## 9. Prerequisite fixes (red-team) — blocking the genesis CLI

1. **Revocation is a no-op.** `CertChain::verify()` checks only the issuer
   signature + `caveats.leq(parent)`; it **ignores `valid_for_generation` and
   `expires_at`** and consults no KRL. Wire fail-closed, pull-based
   generation/KRL enforcement into `verify()` (causal generation is authoritative;
   wall-clock `expires_at` cannot be, per the no-`SystemTime::now()` rule).
2. **No proof-of-possession path.** `AgentKey::issue`/`delegate` *self-generate*
   the keypair — there is no way to certify an externally held agent pubkey. Add an
   issue-over-supplied-pubkey path with a PoP challenge before any cross-cloud join.
3. **Unsigned, fail-open grant.** The bridle MCP boundary loads `Caveats` from
   unsigned `$AGENT_BRIDLE_CAVEATS` and `#[serde(default)]` deserializes a missing
   field to `top()`. Require a **signed** grant; default **fail-closed**.
4. **`permits_key` unenforced** on the git-over-SSH path (only `permits_host` is
   checked); and `git_over_ssh_permitted` is **dead code with no production caller**
   (network ops "deferred (PR5)", `newt-git/src/lib.rs:12`). Wire the real push
   execution site through the gate before adding the presence axis.
5. **Online-root SPOF.** An operator-online root is a hot compromise target —
   prefer an **online intermediate-issuer split** and multi-authenticator-at-genesis.

---

## 10. Charter alignment (two registers)

| Systems name | Gnostic gloss | Invariant |
|---|---|---|
| `Decision::Allow` | exercise of the Writ | Writ |
| `Decision::Deny` | a Refusal — the keystone | Refusal |
| `Presence::Prompt` discharge | a Tether — the human steadying the hand | Tether |
| `Presence::Passkey` discharge | the Writ exercised only while the human's hand is on it | (a *sharpening* of Writ, not an 8th term) |
| `Attestation` + causal-log append | Provenance becoming Scar | Provenance / Scar |
| `best_available_presence()==None` ⇒ fail-closed | the Novice — when unsure, refuse | Novice |

`attest` is a **sharpening of Writ-exercise, not a distinct verb**: it adds no
power, it constrains the *liveness* of an existing one, and an unmet requirement
becomes a Refusal. The frozen vocabulary stays frozen — four *outcomes* on the
surface; the six *invariants* unchanged.

---

## 11. Phasing (branch-sized per CLAUDE.md)

1. **MVP — `feat/step-up-decision-mvp` (agent-bridle).** `decision.rs` types +
   `Gate::evaluate`/`authorize_with_discharge` (with the back-compat `authorize`
   wrapper) + a `SoftwareVerifier` stub (in-test ed25519 assertion over the §4
   challenge) + wire `git.push` as the first action. Regression tests:
   `needs_discharge_does_not_mint_without_proof`, `wrong_challenge_is_denied`,
   `passkey_required_but_no_provider_fails_closed`,
   `presence_meet_never_lowers_a_floor`, all existing gate tests green.
2. **`PresenceCaveats`** in newt-core (sibling-of-GitCaveats PR) + carriage across
   the mesh envelope.
3. **Real ceremony** — `DischargeProvider` impls (libfido2/CTAP2 for YubiKey;
   `windows-rs` for Hello; an entitled helper for macOS).
4. **Effect-bound `pre-receive`** verifier (separate deploy artifact) + the
   touch-to-push `SshCaveats` presence axis and cert-extension propagation.
5. **Prerequisite fixes §9** — interleaved; §9.1/§9.3 land before any genesis CLI.
6. **Genesis ceremony / `newt swarm init`** (Path A anchor + Path B seal) — last,
   gated on §9.

---

## 12. Decided / open / out of scope

**Decided:** `attest` is a sharpening, not a new verb; passkey is the unseal gate,
never the CA; software ed25519 `UserKey` sealed via PRF is the Path-B root; GitHub
key is an identity anchor only; the push enforcement plane is newt-core's SSH
transport + signed cert (bridle is secondary deny-by-default).

**Open:** where `Caveats`/floors live in an OpenSSH cert (extension vs. fingerprint
side-channel) — inherits `ssh-ca-trust-root.md` §10; KRL distribution (a signed
mesh topic); the macOS CLI-WebAuthn gap (entitled helper vs. Secretive fallback);
whether the genesis intermediate-issuer split is mandatory for v1.

**Out of scope:** implementation (this is design only); replacing iroh/QUIC (LAN
unchanged); the mobile app (`mesh-remote-control-mobile-app.md` owns it); side
channels and toolchain supply-chain (separate tracks).

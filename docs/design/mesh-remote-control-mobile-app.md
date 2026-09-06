# Design: Mobile Remote-Control App for newt-agent over agent-mesh

**Status:** Draft (design only — no implementation in this document)
**Date:** 2026-06-07
**Scope of this phase:** Local Wi-Fi only. WAN is handled out-of-band by an
existing WireGuard VPN and is explicitly deferred (§11).
**Builds on:** `docs/decisions/mesh_integration.md` (the `UserKey` → `AgentKey`
trust root and the `agent-mesh-bus` substrate) and
`docs/decisions/agentic_object_capability_security.md` (attenuated `Caveats`).

---

## TL;DR

Build an **Android + iOS app that is itself a first-class agent-mesh peer**.
The phone speaks `agent-mesh` end-to-end to a remote `newt-agent`, preserving
the peer-to-peer property — there is **no broker, no SSH-tunnelled shell, and
no server-side terminal** carrying session data. The app gives the human a
**terminal-like chat UI**: type a message/goal, the remote newt runs a turn
**within the authority its phone-issued `AgentKey` was minted with**, and the
output streams back token-by-token. It "mimics local terminal access" the way
`ssh host` does — but the wire is the signed, replay-defended mesh, not SSH.

Three things make this tractable today:

1. **WireGuard already solves reachability.** The phone gets L3 connectivity to
   the agent's network; this phase only has to work on a shared local Wi-Fi
   segment, and the WAN case "comes for free" once the phone is on the VPN.
2. **The trust root already exists.** Operator authority roots at
   `~/.newt/identity.pem` (`newt-identity`) — the same root the worker, coder,
   plugins, and ACP path already enforce against. The running worker holds an
   `AgentKey` under it and **delegates an attenuated child key to the phone** at
   enrollment (§4). The phone's authority is a *down-set* of the worker's *by
   construction* (worker-child delegation), enforced structurally (§5).
3. **The bus already gives us crypto for free.** Per-envelope ed25519
   signatures, monotonic-sequence + nonce replay defense, and capability-tagged
   discovery all come from `agent-mesh-bus`; we add only a thin **session
   protocol** (§6) on top of the existing request/reply + pub/sub primitives.

The only genuinely new server-side code is a **`NewtSessionService`** responder
(§7). The only new client-side code is a **shared Rust core** (the mesh client)
exposed to Kotlin/Swift via UniFFI, plus two thin native UIs (§8).

---

## 1. Goals and non-goals

### Goals

- A human on a phone can **discover, connect to, and drive a remote
  newt-agent** as if sitting at its terminal.
- The phone is a **real mesh peer** (P2P preserved). No central relay or
  bastion proxies the conversation.
- Every byte the phone sends is **cryptographically attributable** to a
  specific `AgentKey`, and every action the remote performs on the phone's
  behalf is **bounded by that key's `Caveats`** — a stolen phone cannot exceed
  the authority it was lent.
- **Streaming, terminal-like UX**: incremental output, interrupt/cancel, scroll
  history, reconnect to an in-flight session.
- Reuse existing Rust crates rather than reimplementing the bus or crypto in
  Kotlin/Swift.

### Non-goals (this phase)

- **WAN / NAT traversal / relay.** Handled by WireGuard at L3 (§11). No iroh
  relay work here.
- **A real PTY / VT220 emulator.** We render a structured chat/terminal UI, not
  a raw ANSI terminal running `vim` remotely (§6.5, §11).
- **Multi-tenant (multiple distinct operator roots).** One operator trust root
  (`~/.newt/identity.pem`), one human. *Shared / observed sessions among that
  one operator's own attachments are **in** scope — see §7.1 and Phase 1.*
- **App-store distribution, push notifications, MDM.** Out of scope until the
  protocol is proven.

---

## 2. The actors

```
┌─────────────────────────┐         agent-mesh (QUIC/iroh)          ┌──────────────────────────┐
│  Phone (Android / iOS)  │  signed envelopes, replay-defended      │   Host running newt      │
│                         │ ◄─────────────────────────────────────► │                          │
│  ┌───────────────────┐  │   newt/session/v1  (interactive)        │  ┌────────────────────┐  │
│  │  Native UI        │  │   newt/inference/v1 (existing ask)      │  │ NewtSessionService │  │
│  │  (chat/terminal)  │  │                                         │  │  (new responder)   │  │
│  └─────────┬─────────┘  │                                         │  └─────────┬──────────┘  │
│            │ UniFFI     │                                         │            │             │
│  ┌─────────▼─────────┐  │                                         │  ┌─────────▼──────────┐  │
│  │ Rust mesh core    │  │                                         │  │  newt-coder turn   │  │
│  │ (agent-mesh + new │  │                                         │  │  (Caveat-gated)    │  │
│  │  session client)  │  │                                         │  └────────────────────┘  │
│  └───────────────────┘  │                                         │                          │
│  AgentKey (phone), del- │                                         │  UserKey (trust root)    │
│  egated, attenuated     │                                         │  AgentKey (worker)       │
└─────────────────────────┘                                         └──────────────────────────┘
                ▲                                                                 ▲
                │           one-time enrollment over SSH (local Wi-Fi, §4)        │
                └─────────────────────────────────────────────────────────────────┘
```

- **Operator root (`UserKey`)** — the ed25519 trust root. **Unified on
  `~/.newt/identity.pem`** (`newt-identity`), the same root the worker, coder,
  plugins, and ACP path already enforce against; `newt-mesh`'s legacy
  `~/.agent-mesh/user.key` is migrated onto it (§4, finding #2). Issues the
  worker `AgentKey`. **Never leaves that host.**
- **Worker AgentKey** — what `newt-agent` runs as, chained under the operator
  root; advertises capability tags (`newt-inference`, and new `newt-session`).
  **It is the enroller**: it delegates the phone's child key (§4).
- **Phone AgentKey** — a *worker-delegated, attenuated* child key (chain:
  root → worker → phone), so the phone is structurally `⊑` the worker. Minted at
  enrollment (§4). The phone holds the private half in platform key storage
  (§8.3 — pending the hardware-backing spike, finding #6).

---

## 3. Why "over SSH" became "over agent-mesh + WireGuard"

The original framing was "remote control over SSH, mimicking local terminal
access." After establishing the constraints, the roles separated cleanly:

| Concern | Original assumption | Final design |
|---|---|---|
| WAN reachability | SSH tunnel into LAN | **WireGuard VPN** (pre-existing) |
| Secure data channel | SSH transport | **agent-mesh** (QUIC + ed25519 envelopes) |
| Authorization | SSH keys / Unix perms | **`AgentKey` + `Caveats`** (object-capability) |
| Terminal feel | SSH PTY | **app-native chat/terminal UI** over a session protocol |
| Identity bootstrap | — | **one-time SSH enrollment on local Wi-Fi** (§4) |

So **SSH survives only as the enrollment bootstrap** (the one moment the phone
reaches the worker host to obtain a worker-delegated child cert), plus as the
*mental model* for the UX. The interactive session never travels over SSH.
This keeps the system peer-to-peer: phone ↔ agent, no shell host in the middle.

> **Why not just SSH the whole thing?** An SSH shell would put the phone's
> authority at "whatever the Unix account can do," re-creating the confused-
> deputy problem the ocap design exists to kill. Driving the agent over the
> mesh means the phone's authority is *structurally* a down-set of the user's
> (§5), and every message is independently attributable.

---

## 4. Identity and enrollment (worker-delegated; the one place SSH is used)

The phone obtains a **worker-delegated child `AgentKey`** — chained
`root (~/.newt/identity.pem) → worker → phone` — without any private signing
material leaving the worker host. One-time, local-Wi-Fi flow:

1. **Phone generates a keypair.** Target: a hardware-backed, non-exportable key
   (Android Keystore / iOS Secure Enclave). **This is gated on a compatibility
   spike (finding #6 / §8.3):** if the platforms can't expose the ed25519
   signing op agent-mesh needs in hardware, the fallback is a software ed25519
   key sealed at rest by the platform keystore + biometric gate.
2. **Phone produces an enrollment request**: its public key + requested
   `AgentMetadata` (role `"newt-phone"`, host = device name, capability tags) +
   a proposed `Caveats` attenuation (§5).
3. **The worker delegates a child cert over the phone's public key**, capping the
   request at its policy-max via caveat *intersection*, and returns the signed
   `CertChain` over the enrollment channel:
   - **SSH (default this phase):** the request is piped to the worker host on
     local Wi-Fi; a `newt mesh enroll` subcommand performs the delegation and
     returns the chain.
   - **QR / out-of-band:** request/response exchanged as QR codes (no SSH
     dependency). Listed for completeness; SSH is the chosen path this phase.

   The §4.5 numeric-comparison pairing runs *during* this step, with the worker
   displaying the SAS on its **local-console attachment** (§7.1).
4. **Phone stores the returned `CertChain`** (public material only) beside its
   key. It is now a worker-delegated mesh peer.

> **[finding #1] This needs new upstream API.** Today
> `agent-mesh-protocol::AgentKey::{issue,delegate}` **generate their own
> in-memory keypair and certify that** — there is no path to certify an
> *externally supplied* phone public key, and `agent-mesh-transport` assumes it
> holds the agent signing *seed* when it builds the iroh endpoint identity. So
> this design has a hard dependency on **two upstream additions in agent-mesh**:
> (a) "delegate/issue a cert over an external public key," and (b) an
> **external / platform signer abstraction** the transport can use instead of a
> local seed. Without (b), the phone cannot both be a transport endpoint *and*
> keep a non-exportable key — the §8.3 software-key fallback is the
> degraded-but-shippable path until (b) lands.

Key properties:

- **No private signing material transits the network** — only a signed,
  attenuated child cert comes back; the phone's own private key is generated
  on-device and the worker never sees it (subject to finding #1).
- Enrollment is **revocable by the worker** (it issued the child) — see §5.4.
- Because delegation is **attenuation-only**, the worker's `enroll` caps what a
  phone may ever request, regardless of what the request asks for.

### 4.5 Numeric-comparison pairing (MITM + human-presence defense)

Enrollment over a network — even local Wi-Fi — is exposed to a
**machine-in-the-middle** who relays the key exchange and substitutes its own
key. Signatures alone don't catch this: the operator would happily sign *the
attacker's* public key if it arrives looking like the phone's. We close this
with an **out-of-band human comparison step**, in the spirit of Bluetooth
Secure Simple Pairing's *numeric comparison* and the SAS (Short Authentication
String) used in ZRTP.

**The value being compared is a commitment to the whole transcript, not a
random number.** Both sides independently compute:

```
sas = KDF( phone_pubkey ‖ host_pubkey ‖ session_nonce )   // truncated to a small range
```

Because the SAS is derived from *both* public keys, a MITM that swapped either
key produces a **different** SAS on the two ends — the human comparison then
fails. (Random numbers would only prove "a human pressed a button," not "the
keys match"; deriving from the transcript is what makes this MITM-resistant.)

**The challenge-response game (your three-numbers idea):**

1. The host maps its SAS into a **set of three candidate numbers**, one of
   which is the "true" SAS digit; it displays the **true** number on its
   **local terminal** (the out-of-band channel — see §7.1, why concurrent local
   access matters).
2. The phone, from *its* independently-derived SAS, presents **three choices**;
   the human — who is physically at the machine and can read the local
   terminal — taps the matching one.
3. **Repeat `k` rounds** with fresh per-round derivation. A blind attacker who
   cannot see the local terminal passes a single round with probability `1/3`,
   so `k` rounds reduce that to **`(1/3)^k`** (e.g. `k = 5` → ~0.4%). The
   operator picks `k` to taste; the rounds cost the human a few taps.

Properties:

- **MITM-resistant**: a swapped key yields mismatched SAS → the human's correct
  choices stop matching → pairing aborts.
- **Proves co-presence**: only someone who can see the host's local terminal
  during pairing can answer, binding enrollment to a human at the machine.
- **No shared secret to phish**: the human never types a code the attacker could
  capture and replay; they make a *selection*, and the selection is only
  meaningful against the live, transcript-bound SAS.

On success, the worker delegates the attenuated child cert (§4 step 3); on any
mismatch it refuses and discards the request.

---

## 5. Security model — the phone holds attenuated authority

This is the heart of the design and the reason to prefer mesh over SSH.

### 5.1 The phone's `AgentKey` is a down-set, not the user's authority

The phone is a **worker-delegated child** (chain `root → worker → phone`), so its
`Caveats` are `⊑` the worker's **by construction** — attenuation-only delegation
makes amplification impossible, not merely policy. Example policy a worker would
stamp on a remote-control phone:

```rust
Caveats {
    // The phone may ask the agent to run a coding turn, but the agent's
    // *exec* authority on the host is itself capped by the worker key — the
    // phone never widens it.
    exec:  Scope::only(["git", "cargo", "rg", "ls", "cat"]),
    net:   Scope::none(),               // no outbound net on the phone's behalf
    fs_read:  Scope::only(["<repo-root>"]),
    fs_write: Scope::only(["<repo-root>"]),
    max_calls: CountBound::AtMost(64),  // per-session tool-call budget
    valid_for_generation: Scope::only([current_gen]),
    ..Caveats::top()
}
```

Enforcement is the **existing** path: the worker calls `caveats_for_peer(cert)`
(`newt-mesh/src/caveats.rs`) which verifies the chain end-to-end and rejects any
amplification, then the dispatch sites consult `permits_exec` / `permits_fs_*`
/ `permits_one_more` (the `CaveatsExt` adaptors in `newt-core`). **A fully
compromised phone still cannot exceed the down-set it was minted with** —
safety is structural, not "the model behaved." Because the phone is already
`⊑` the worker, the `meet(worker, phone)` framing used in §6.4/§7 reduces to
*the phone's own caveats*; the explicit meet still matters for **shared
sessions** (§7.1), where the *active driver's* caveats govern a given turn.

### 5.2 Per-message attribution and replay defense — free from the bus

Every session message rides inside a `BusMessage`, so it inherits:

- **ed25519 signature** over the envelope → the remote knows exactly which
  `AgentKey` typed each line.
- **monotonic sequence + nonce** → a captured "delete the repo" message can't
  be replayed later.
- **user-fingerprint namespacing** on the topic → peers only auto-team within
  the same `UserKey`. A phone enrolled under a different user simply isn't on
  the same topic.

### 5.3 Stolen-phone threat model

- Private key is **sealed by the platform keystore and biometric-gated** —
  ideally hardware-backed and non-exportable (pending the §8.3 spike, finding
  #6); a software key sealed at rest + biometric gate is the fallback. Either
  way an attacker gets signing *use* only while the unlock/biometric gate is
  satisfied; the app gates session start behind biometric re-auth.
- The cert is **short-lived** and the budget (`max_calls`, `expires_at`) bounds
  damage between revocation and expiry.
- `net: Scope::none()` and a repo-scoped `fs_*` mean even a live, unlocked
  stolen phone can't exfiltrate beyond the repo or reach the internet via the
  agent.

### 5.4 Revocation (open item)

agent-mesh today is enrollment-based with no CRL. Because the phone is a
**worker-delegated child** (finding #5), the **worker is the natural revoker**:
it drops the phone's fingerprint from its allowed-children set, and
`NewtSessionService` checks that set before opening a session. Minimum viable:
short `expires_at` on the child cert + the worker's denied-fingerprint list;
root-level (operator) revocation cascades to all of a worker's children. A
signed revocation topic is the richer follow-up. Flagged in §12.

---

## 6. The session protocol (`newt/session/v1`)

The existing `newt/inference/v1` is single-shot request/reply with no streaming
(`mesh_integration.md` §"No streaming"). A terminal needs **streaming,
multi-turn, and cancel**. We add a new capability tag `newt-session` and a new
topic family.

### 6.1 Wire types (sketch)

```rust
/// Client → responder: open an interactive session.
struct SessionOpen {
    session_id: SessionId,          // UUID newtype (roadmap Step 1.2)
    workspace_hint: Option<String>, // repo / cwd the human wants
    cols: u16, rows: u16,           // for output reflow, not a real PTY
    protocol: u16,                  // = 1
}

/// Responder → client: session accepted (or refused with reason).
struct SessionOpened { session_id: SessionId, agent_model: String, cwd: String }

/// Client → responder: a line/goal the human typed.
struct SessionInput {
    session_id: SessionId,
    seq: u64,                       // client-side ordering within the session
    kind: InputKind,                // Prompt | ControlC (cancel) | Eof
    text: String,
}

/// Responder → client: one streamed chunk of output.
struct OutputChunk {
    session_id: SessionId,
    turn: u64,
    stream: OutputStream,           // Stdout | Stderr | AgentThought | ToolCall | Diff
    seq: u64,                       // ordering within the turn
    data: String,
    last: bool,                     // final chunk of this turn
}

/// Either side: graceful close.
struct SessionClose { session_id: SessionId, reason: String }
```

`SessionId` reuses the planned `newt-core` `SessionId(Uuid)` (ROADMAP Step 1.2);
the original plan mapped streaming `OutputChunk` onto `ChatChunk` / `ChatStream`
(ROADMAP Step 2.3). That unused type island was retired in Stage D batch 1;
it was never a live dependency. Streaming/cancel remains new work (§6.4).

### 6.2 Transport mapping onto the bus

The bus has **request/reply** and **publish/subscribe** (`publish_to`). We use
both:

- **`SessionOpen` / `SessionOpened`** → request/reply (one round-trip,
  authenticated, gets a session handle).
- **`SessionInput`** → request to the responder on a per-session topic
  `"<user_fp>:newt/session/v1/<session_id>/in"`. Cheap ack reply.
- **`OutputChunk`** → responder **publishes** to
  `"<user_fp>:newt/session/v1/<session_id>/out"`, which the phone is subscribed
  to. This is the streaming half — exactly the `publish_to` + "stream chunk
  topic" shape `mesh_integration.md` already anticipated for streaming.
- **`ControlC`** → a `SessionInput{kind: ControlC}` request; the responder
  cancels the in-flight `newt-coder` turn (cooperative cancellation token).

> **Why not one bidirectional stream?** The bus's stable surface is
> request/reply + pub/sub. Layering session semantics on those keeps us on the
> supported API and inherits its crypto. If agent-mesh later exposes a raw
> bidirectional QUIC stream, the session protocol can adopt it without changing
> the message shapes.

### 6.3 Ordering, backpressure, reconnect

- Each direction carries a monotonic `seq`; the phone reorders/ dedupes
  `OutputChunk`s and detects gaps.
- **Reconnect to an in-flight session:** the responder retains a bounded
  ring-buffer of recent `OutputChunk`s per session; on re-subscribe the phone
  sends a `resume_from: seq` and the responder replays the tail. This makes the
  app survive a Wi-Fi blip / app backgrounding without losing the turn.
- **Backpressure:** the phone acks output windows; the responder pauses
  publishing past a window to avoid flooding a slow mobile link.

### 6.4 What a "turn" does on the remote

A `SessionInput{Prompt}` drives one `newt-coder` turn (or a plain inference if
the workspace is bare). The turn runs **under the driving attachment's caveats**
— for a phone that is its worker-delegated child caveats, already `⊑` the worker
(§5.1), so `meet(worker, phone)` reduces to the phone's set. Streamed
`AgentThought` / `ToolCall` / `Diff` chunks give the terminal its "watch the
agent work" feel.

> **[finding #3] Streaming/cancel is real new work, not reuse.** Today the ACP
> path returns *one* `TaskReply` after `backend.complete(...)`. The former
> `ChatChunk` / `ChatStream` island was **never** wired into `InferenceBackend`,
> the ACP session, the coder loop, or any cancellation path, and was retired in
> Stage D batch 1. Delivering token-by-token `OutputChunk`s + `ControlC` requires
> *building* that streaming + cooperative-cancel surface (see §7, Phase 1b).

### 6.5 Terminal fidelity

The UI is a **structured terminal**, not a VT emulator: monospace output pane,
distinct styling for stdout/stderr/thoughts/tool-calls/diffs, an input line
with history, and a cancel button mapped to `ControlC`. This covers the
"mimic local terminal access" goal for an *agent* terminal without shipping a
PTY. A true PTY pass-through is a possible later mode (§11) but is a different,
heavier protocol and a much larger attack surface.

---

## 7. Server side: `NewtSessionService`

A new responder alongside `NewtMeshService`, living in the (out-of-workspace)
`newt-mesh` crate so it keeps the agent-mesh path-dep contained.

- **Bind**: like `NewtMeshService::bind`, but registers handlers on the
  `newt/session/v1/*` topics and advertises the `newt-session` capability tag
  (so phones can pre-filter in discovery).
- **Session table**: `SessionId → SessionState { caveats, turn_handle,
  output_ring, last_activity }`. Idle sessions time out.
- **Caveat enforcement**: on `SessionOpen`, call `caveats_for_peer(peer_cert)`,
  reject if verification fails, store the verified caveats, and gate every
  subsequent tool call in the driven turn through `meet(worker, phone)`.
- **Turn driver**: bridges `SessionInput` → a `newt-coder` turn (or a
  `newt-acp-worker` `Session`) and fans the turn's events out as `OutputChunk`s.
  **Note (finding #3):** the ACP/coder path is single-shot today, so the driver
  depends on the new streaming surface (§6.4) — it reuses the `Session` /
  `TaskReply` *shape*, but the streaming itself is new.
- **Cancellation**: holds a `tokio_util::sync::CancellationToken` per active
  turn; `ControlC` trips it. The cooperative-cancel plumbing through the backend
  trait and coder loop is part of that new work.

So the responder reuses the `Session` / `TaskReply` *shape* and the caveat-
extraction path, but **adds** the streaming + cancel surface rather than
inheriting a ready-made one.

### 7.1 Concurrent local + remote attach (multi-session model)

The pairing step (§4.5) and the product itself both require the agent to serve a
**local console and one or more remote phone sessions at the same time**. During
pairing the local terminal is the out-of-band display while the phone talks over
the mesh; in normal use a developer may be working at the keyboard while also
driving the agent from a phone.

So the agent is **not** a single-attach, one-stdin/stdout process. The model:

- The agent core owns a **session registry** (§7's `SessionId → SessionState`),
  and a *terminal attach* — local console **or** a remote `newt/session/v1`
  peer — is just an **attachment** to a session. The same `OutputChunk` stream
  fans out to every attachment subscribed to that session.
- **Local console** becomes one attachment alongside mesh sessions, rather than
  a privileged owner of fd 0/1/2. (This is a clean extension of the existing
  `stdio_guard` discipline in `newt-cli`, which already keeps the agent's real
  stdout separate from protocol I/O.)
- **Two attach modes (both in Phase 1 — finding #4):**
  - *Separate sessions* (default): the local user and the phone each drive their
    **own** `SessionId` with their **own** worker-delegated caveats. They don't
    step on each other; neither sees the other's session unless explicitly
    shared.
  - *Shared / observed session*: a phone may **attach as an observer** to a
    running local session (read-only `OutputChunk` stream) or, with **explicit
    local confirmation**, as a **co-driver** ("watch / drive what my laptop is
    doing from my phone"). Per finding #4 this is promoted out of non-goals and
    into Phase 1 acceptance, *not* deferred — it shares plumbing with separate
    sessions and is a primary use case.
- **Input arbitration** for a co-driven session: inputs are serialized by the
  session's `seq`; a turn in flight must complete or be cancelled before the
  next input is accepted, so two attachments can't interleave half-turns.
- **Caveat composition is per-attachment**: authority for any action is the
  *active driver attachment's* caveats (each already a worker-delegated child,
  so `⊑` worker). A read-only observer attachment carries `Caveats` with empty
  `exec`/`fs_write`, so observing can never mutate.

This multi-attach model makes the §4.5 pairing possible (local display + remote
choice, concurrently), supports the shared-session use case, and is the one
piece of newt-agent's *own* architecture this design pushes on (Phase 1) beyond
adding a responder.

---

## 8. Mobile app architecture

### 8.1 Shared Rust core (the load-bearing reuse)

The phone must speak the *real* agent-mesh protocol (QUIC, ed25519, the exact
envelope format). Reimplementing that in Kotlin/Swift would fork the crypto and
guarantee drift. Instead:

- A new crate **`newt-mesh-mobile`** (client-only) wraps `agent-mesh-bus` +
  `agent-mesh-discovery` + the new session client into a small, FFI-friendly
  API:

  ```rust
  fn enroll(request) -> CertChainBytes;          // §4
  fn list_peers(timeout) -> Vec<PeerInfo>;       // discovery
  fn open_session(peer_fp, opts) -> SessionHandle;
  fn send_input(handle, text);
  fn cancel(handle);
  // callback / async stream of OutputChunk → UI
  ```

- **Bindings via [UniFFI]**: generates Kotlin and Swift bindings from the Rust
  API, so both platforms share one implementation of the protocol and the key
  handling.
- **Build**: `cargo-ndk` → `.so` per Android ABI; `cargo` + `lipo`/xcframework
  for iOS (arm64 device + simulator). The QUIC stack (iroh/quinn) and ed25519
  (`ed25519-dalek`) are pure-Rust and cross-compile cleanly to both targets.
- **Async**: the core runs a `tokio` runtime on a background thread; the FFI
  surface is callback- or `Flow`/`AsyncSequence`-shaped so the UI thread never
  blocks.

### 8.2 Native UI layers (thin)

- **Android**: Kotlin + Jetpack Compose. A `SessionViewModel` consumes a Kotlin
  `Flow<OutputChunk>` from the core. Foreground service keeps the mesh socket
  alive during an active session; respects Doze when idle.
- **iOS**: SwiftUI. An `@Observable` session model consumes an `AsyncStream`.
  Background mode is constrained (iOS suspends sockets aggressively) — the
  reconnect/resume design (§6.3) is what makes this acceptable.

### 8.3 Key storage (gated on a crypto spike — finding #6)

- Private key: **target** is Android Keystore / iOS Secure Enclave
  (non-exportable, biometric-gated). **This is not a safe assumption yet** — it
  depends on the platforms exposing the exact ed25519 signing op agent-mesh
  needs in hardware, *and* on the agent-mesh transport accepting an external
  signer (finding #1b). A **pre-implementation spike** must confirm both.
- **Fallback** if hardware ed25519 isn't available for the deployment floor: a
  software ed25519 key sealed at rest by the platform keystore + biometric gate.
  Weaker (key material is decryptable in-process while unlocked) but shippable.
- Cert chain + known-peer records: app-private encrypted storage
  (EncryptedSharedPreferences / Keychain).

---

## 9. Discovery on mobile + the WireGuard multicast caveat

The bus discovers peers via **mDNS** (`agent-mesh-discovery`). On a shared
**local Wi-Fi** segment this works directly, and the platforms have native
support to lean on if needed (Android NSD, iOS Bonjour). The phone browses for
the `newt-session` capability tag and shows a peer list.

**Honest caveat for the WAN phase:** mDNS is *multicast*, and a plain WireGuard
tunnel is point-to-point and **does not forward multicast by default**. So even
though WireGuard gives L3 reachability, mDNS browsing will likely *not* work
across it. The design therefore includes a **direct-dial / known-peer path**:

- The phone caches resolved peers as `(agent_fp, endpoint_addr, last_seen)`
  records.
- Enrollment (§4) can additionally hand the phone a **rendezvous record** for
  the worker (stable fingerprint + reachable address/port within the WireGuard
  subnet), so the phone can **dial by address without mDNS**.
- This implies a small **upstream ask of agent-mesh**: a "dial known peer by
  endpoint" path that bypasses mDNS resolution. On local Wi-Fi today we can use
  mDNS; the direct-dial path is what makes the WireGuard WAN phase a no-op for
  the app. Flagged in §12 as the main dependency.

---

## 10. End-to-end flows

**Enrollment (once, local Wi-Fi):**
phone generates key → builds enrollment request → SSH to the **worker** host →
§4.5 numeric-comparison pairing (worker shows SAS on its local console) →
`newt mesh enroll` **delegates a child cert** (root → worker → phone) → phone
stores cert + rendezvous record.

**Connect + drive (every session):**
biometric unlock → core browses mDNS (or dials known peer) → user picks the
agent → `SessionOpen` → `SessionOpened` → user types a goal → `SessionInput`
→ remote runs a Caveat-gated turn → `OutputChunk`s stream into the terminal
pane → user taps cancel (`ControlC`) or types follow-ups → `SessionClose`.

**Reconnect (Wi-Fi blip / backgrounded):**
app resumes → re-subscribe to `…/out` with `resume_from` → responder replays
buffered tail → terminal continues without data loss.

---

## 11. Out of scope / deferred

- **WAN transport.** WireGuard handles it at L3; the only app-side work is the
  direct-dial path (§9), and even that is exercised first on local Wi-Fi.
- **iroh relay / NAT hole-punching.** Not needed given WireGuard.
- **Real PTY pass-through** (`vim`/`htop` over the wire). A separate, heavier
  mode; the structured agent-terminal covers this phase.
- **Multi-tenant (multiple distinct operator roots), push notifications,
  app-store release.** (Same-operator shared / observed sessions are *in* scope
  — §7.1, Phase 1.)

---

## 12. Dependencies, risks, open questions

1. **agent-mesh direct-dial by endpoint (main dependency).** Needed so the
   WireGuard WAN phase requires no app change. Confirm whether `agent-mesh-bus`
   can already dial a known `(fingerprint, addr)` or whether this is a small
   upstream addition.
2. **Streaming on the bus.** The session protocol leans on `publish_to` +
   per-session output topics. Validate throughput/backpressure on a real mobile
   link. The historical `ChatStream`/`ChatChunk` candidate (ROADMAP 2.3) was
   retired; implementing the shared streaming substrate remains Phase 1b work.
3. **Revocation (§5.4).** Decide minimum-viable: short `expires_at` only, vs. a
   signed revocation topic / denied-fingerprint list at the responder.
4. **UniFFI + tokio + iroh on mobile.** De-risk early: a "hello, signed echo"
   spike that binds the core, enrolls, and round-trips one signed message on
   both Android and iOS before building any UI.
5. **iOS background socket lifetime.** Confirm the reconnect/resume design
   (§6.3) is sufficient given iOS's aggressive suspension; may need a
   user-initiated foreground model only.
6. **Crate placement.** `newt-mesh-mobile` should sit beside `newt-mesh`
   (out-of-workspace, path-dep to agent-mesh) to keep CI green per
   `mesh_integration.md`. Confirm whether the app repo is separate from
   `newt-agent`.
7. **Pairing parameters (§4.5).** Choose the SAS range and round count `k`
   (security vs. taps), and confirm the SAS KDF/transcript binding against the
   actual key-exchange agent-mesh performs at dial time — the SAS must commit to
   the *real* handshake transcript to be MITM-resistant.
8. **Multi-attach is a newt-agent change (§7.1).** Promoting the local console
   from "owns stdio" to "one attachment among many" touches the core session
   loop and `stdio_guard`. Confirm appetite and sequencing — Phase 1 depends on
   it, and per finding #4 Phase 1 now includes **shared / observed** attach, not
   just separate sessions.
9. **[finding #1] External-pubkey delegation + transport signer (hard upstream
   dependency).** agent-mesh must gain (a) issue/delegate a cert over an
   *externally supplied* public key, and (b) an external / platform signer
   abstraction for `agent-mesh-transport` (today it assumes the local signing
   seed). Until (b), the non-exportable-key property is unattainable and the
   §8.3 software-key fallback applies. This is the gating dependency for
   enrollment as designed.
10. **[finding #2] Operator-root unification.** This design assumes `newt-mesh`
   migrates onto `newt-identity`'s `~/.newt/identity.pem` root so phone, worker,
   coder, and plugins share one operator identity. Scope and sequence that
   migration (does the legacy `~/.agent-mesh/user.key` get a one-time import?).
11. **[finding #6] Mobile crypto / key-storage spike.** Before any UI work,
   confirm hardware-backed ed25519 (signing op + attestation) on the Android /
   iOS deployment floor, or commit to the software-key fallback. Couples to #9.

---

## 13. Suggested phasing (each ≈ one focused PR)

| Phase | Deliverable |
|---|---|
| 0 | This design doc; agree the §6 wire types and §12 open questions/dependencies. |
| 0a | **Upstream agent-mesh** (finding #1): external-pubkey delegation + transport signer abstraction. *Gates enrollment.* |
| 0b | **Operator-root unification** (finding #2): migrate `newt-mesh` onto `newt-identity` (`~/.newt/identity.pem`). Mobile crypto/key-storage spike (finding #6) runs in parallel. |
| 1 | Multi-attach session model (§7.1): local console as one attachment among many; session registry; **separate *and* shared/observed attach** (finding #4). Prereq for the rest. |
| 1b | `newt/session/v1` wire types + `NewtSessionService` **plus the new streaming + cooperative-cancel surface** through the backend/ACP/coder path (finding #3); in-process round-trip tests. |
| 2 | `newt mesh enroll` (worker-delegated child cert) + enrollment types (§4) **with §4.5 numeric-comparison pairing** (worker local-console SAS display). |
| 3 | `newt-mesh-mobile` Rust core + UniFFI bindings; "signed echo" spike on both platforms (§12.4). |
| 4 | Android Compose terminal UI on the core (enroll → discover → session). |
| 5 | iOS SwiftUI terminal UI; reconnect/resume hardening (§6.3). |
| 6 | Direct-dial path (§9) + WireGuard WAN validation. |
| 7 | Revocation (worker-as-revoker) + biometric gating + budget/expiry polish (§5). |

---

## Appendix A — Review dispositions (PR #202, @hartsock)

| # | Finding | Disposition |
|---|---|---|
| 1 | `issue`/`delegate` can't certify an external pubkey; transport assumes the signing seed | **Accepted.** New upstream dependency in §4 (note), §8.3, §12.9, Phase 0a. Non-exportable key now contingent; software-key fallback documented. |
| 2 | Trust root split (`~/.agent-mesh/user.key` vs `~/.newt/identity.pem`) | **Accepted — unify on `~/.newt/identity.pem`.** `newt-mesh` migrates onto `newt-identity`; §2, §4, §12.10, Phase 0b. |
| 3 | `newt-acp-worker` has no streaming to reuse | **Accepted.** Reworded to "reuse `Session`/`TaskReply` shape; streaming + cancel is new work." §6.4, §7, Phase 1b. |
| 4 | Shared sessions listed as non-goal but in Phase 1 | **Resolved — shared/observed attach is *in* Phase 1.** Removed from non-goals (multi-*tenant* stays out). §1, §7.1, §11, Phase 1. |
| 5 | Phone caveat topology ambiguous | **Resolved — worker-child delegation** (root → worker → phone). Worker is enroller and revoker. §2, §4, §5.1, §5.4. |
| 6 | Hardware ed25519 assumed, not spiked | **Accepted.** Downgraded to a gated spike with a software-key fallback. §5.3, §8.3, §12.11, Phase 0b. |

These dispositions reflect decisions taken with the maintainer; the doc text
above is authoritative where it differs from the original draft.

[UniFFI]: https://mozilla.github.io/uniffi-rs/

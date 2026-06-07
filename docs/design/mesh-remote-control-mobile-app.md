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
2. **The trust root already exists.** `UserKey` issues attenuated `AgentKey`s.
   The phone becomes just another agent under the same user — its authority is
   a *down-set* of the user's, enforced structurally (§5).
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
- **Multi-user / shared sessions.** One user-trust-root, one human, one phone
  identity per enrollment.
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

- **UserKey** — the ed25519 trust root. Lives on the user's primary host
  (today: `~/.agent-mesh/user.key`). Issues every `AgentKey`. **Never leaves
  that host.**
- **Worker AgentKey** — what `newt-agent` runs as; advertises capability tags
  (`newt-inference`, and new `newt-session`).
- **Phone AgentKey** — a *delegated, attenuated* key minted for the phone at
  enrollment (§4). The phone holds the private half in its platform keystore
  (Android Keystore / iOS Secure Enclave / Keychain).

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

So **SSH survives only as the enrollment bootstrap** (the one moment we need to
reach a host that holds the `UserKey` and get a signed delegation), plus as the
*mental model* for the UX. The interactive session never travels over SSH.
This keeps the system peer-to-peer: phone ↔ agent, no shell host in the middle.

> **Why not just SSH the whole thing?** An SSH shell would put the phone's
> authority at "whatever the Unix account can do," re-creating the confused-
> deputy problem the ocap design exists to kill. Driving the agent over the
> mesh means the phone's authority is *structurally* a down-set of the user's
> (§5), and every message is independently attributable.

---

## 4. Identity and enrollment (the one place SSH is used)

The phone must obtain an `AgentKey` **signed by the `UserKey`** without the
`UserKey` ever leaving its host. This is a one-time, local-Wi-Fi flow:

1. **Phone generates a keypair** in its hardware-backed keystore (Android
   Keystore / iOS Secure Enclave). The private key is non-exportable.
2. **Phone produces an enrollment request**: its public key + requested
   `AgentMetadata` (role `"newt-phone"`, host = device name, desired capability
   tags) + a proposed `Caveats` attenuation (§5).
3. **Operator runs a one-liner on the UserKey host**, reachable on local Wi-Fi.
   Two interchangeable transports:
   - **SSH (default this phase):** the phone (or the operator) `scp`s/pipes the
     enrollment request to the host; a small `newt mesh enroll` command loads
     `~/.agent-mesh/user.key`, calls `AgentKey::issue` (or `parent.delegate`)
     with the *intersection* of requested and policy-max caveats, and returns
     the signed `CertChain` back over the SSH channel.
   - **QR / out-of-band:** the host prints the enrollment request as a QR for
     the phone to scan, or vice-versa (no SSH dependency). Listed for
     completeness; SSH is the chosen path for this phase.
4. **Phone stores the returned `CertChain`** (public material only) alongside
   its keystore-held private key. It is now a mesh peer.

Key properties:

- The `UserKey` private material never transits the network — only a **signed,
  attenuated cert** comes back.
- Enrollment is **revocable**: short `expires_at` on the phone's cert + re-
  enrollment, or a revocation list checked at the responder (see §5.4 / open
  questions).
- Because issuance is **attenuation-only**, the host's `enroll` command can
  *cap* what a phone may ever request, regardless of what the request asks for.

---

## 5. Security model — the phone holds attenuated authority

This is the heart of the design and the reason to prefer mesh over SSH.

### 5.1 The phone's `AgentKey` is a down-set, not the user's authority

The phone is minted with `Caveats` strictly below the worker's. Example policy
for a remote-control phone:

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
safety is structural, not "the model behaved."

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

- Private key is **hardware-backed and non-exportable**; an attacker with the
  device gets it only while the OS unlock/biometric gate is satisfied. The app
  gates session start behind biometric re-auth.
- The cert is **short-lived** and the budget (`max_calls`, `expires_at`) bounds
  damage between revocation and expiry.
- `net: Scope::none()` and a repo-scoped `fs_*` mean even a live, unlocked
  stolen phone can't exfiltrate beyond the repo or reach the internet via the
  agent.

### 5.4 Revocation (open item)

agent-mesh today is enrollment-based with no CRL. Minimum viable: short
`expires_at` + manual re-enroll. Better: a small **revocation topic** the
worker subscribes to (signed by `UserKey`), or a denied-fingerprint list the
`NewtSessionService` checks before opening a session. Flagged in §12.

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
the streaming `OutputChunk` maps cleanly onto the planned `ChatChunk` /
`ChatStream` types (ROADMAP Step 2.3).

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
the workspace is bare). The turn runs **under the phone's caveats** (§5.1) — the
responder constructs the turn's authority as `meet(worker_caveats,
phone_caveats)`, so the agent can never do more for the phone than either party
allows. Streamed `AgentThought` / `ToolCall` / `Diff` chunks give the terminal
its "watch the agent work" feel.

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
- **Turn driver**: bridges `SessionInput` → a `newt-coder` turn (or
  `newt-acp-worker` `Session`, which already has streaming + `TaskReply`), and
  fans the turn's streamed events out as `OutputChunk`s.
- **Cancellation**: holds a `tokio_util::sync::CancellationToken` per active
  turn; `ControlC` trips it.

This reuses `newt-acp-worker`'s existing `Session`/`TaskReply`/streaming
machinery rather than inventing a second agent loop.

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

### 8.3 Key storage

- Private key: **Android Keystore** / **iOS Secure Enclave** (non-exportable,
  biometric-gated for session start).
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
phone generates key → builds enrollment request → SSH to UserKey host →
`newt mesh enroll` issues attenuated cert → phone stores cert + rendezvous
record.

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
- **Multi-user / multi-tenant sessions, push notifications, app-store release.**

---

## 12. Dependencies, risks, open questions

1. **agent-mesh direct-dial by endpoint (main dependency).** Needed so the
   WireGuard WAN phase requires no app change. Confirm whether `agent-mesh-bus`
   can already dial a known `(fingerprint, addr)` or whether this is a small
   upstream addition.
2. **Streaming on the bus.** The session protocol leans on `publish_to` +
   per-session output topics. Validate throughput/backpressure on a real mobile
   link; confirm the planned `ChatStream`/`ChatChunk` types (ROADMAP 2.3) are
   the right substrate.
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

---

## 13. Suggested phasing (each ≈ one focused PR)

| Phase | Deliverable |
|---|---|
| 0 | This design doc; agree the §6 wire types and the §12 open questions. |
| 1 | `newt/session/v1` wire types + `NewtSessionService` (responder) with in-process round-trip tests (mirroring `newt-mesh`'s test style). |
| 2 | `newt mesh enroll` command + enrollment request/response types (§4). |
| 3 | `newt-mesh-mobile` Rust core + UniFFI bindings; "signed echo" spike on both platforms (§12.4). |
| 4 | Android Compose terminal UI on the core (enroll → discover → session). |
| 5 | iOS SwiftUI terminal UI; reconnect/resume hardening (§6.3). |
| 6 | Direct-dial path (§9) + WireGuard WAN validation. |
| 7 | Revocation + biometric gating + budget/expiry polish (§5). |

[UniFFI]: https://mozilla.github.io/uniffi-rs/

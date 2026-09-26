# Decision: newt-web mesh docking — a hub cockpit for remote newt-agent sessions

**Status:** Accepted (plan approved by Shawn Hartsock, 2026-08-11).
**Date:** 2026-08-11
**Related:** `docs/decisions/newt_web_htmx.md` (D1 placement, **D2** mirror+inject / co-drive
never, D3 auth), `docs/decisions/plain_scroller_tui.md` (2026-08-11 amendment — RichTUI may host
a live dock overview), `docs/decisions/mesh_integration.md` (excluded-crate pattern),
`agent-mesh/docs/decisions/session_streams.md` (the duplex bus primitive this depends on),
`docs/design/mesh-remote-control-mobile-app.md` (the multi-attach session model), ROADMAP W5
(mesh presence, previously unbuilt).

## The thing being decided

`newt-web` today exposes only the **local** box's sessions. This grows it into a **hub cockpit**:
other newt-agents — **the same operator's**, on other machines — **dock** into it over agent-mesh
and surface *their* sessions in its HTMX. The operator watches and prompts docked remote sessions
alongside local ones, navigates them via a tab/pane overview, and can forcibly undock everything
from the TUI.

## Decisions

**K1 — Mirror + inject is preserved *across the mesh*; the hub never writes a remote transcript.**
D2's single-writer claim is inviolate and it does not weaken at a network boundary. A docked
remote session is **driven only via its own host's inject seam**: the hub sends a `SessionInput`
over the mesh, the remote host's service calls **its** `ConversationStore::inject_prompt`
(`newt-core/src/store.rs:1357`), and the **remote** claim-holding REPL consumes it via
`take_injected_prompt` at its own turn boundary. The hub only *mirrors* the remote transcript
(view) and *enqueues* prompts (inject). No co-driving, no second claim, no cross-machine
transcript write. This is D2, extended one hop.

**K2 — Same operator first.** Docked peers share one agent-mesh `UserKey`, so the transport
handshake already auto-trusts them (`agent-mesh` refuses a different-`UserKey` peer outright). The
docking ceremony therefore does **not** establish cryptographic trust from scratch — it
establishes **operator intent + a scoped, revocable approval** ("this specific peer agent is
approved to dock, with this authority"). Cross-operator docking (different `UserKey`, via
`AgentKey::delegate_external` minting an attenuated child cert over the peer's externally-held
key) is a **deferred** phase and must additionally resolve the handshake channel-binding gap
before it ships.

**K3 — "Approved newt-web / approved peer" is a signed, fail-closed, revocable registry.**
Modeled on `credential_registry.rs`: records name the peer agent fingerprint, a scope `Caveats`,
an `issued_generation`, the ceremony `transcript_id`, and a `revoked` flag, signed by the
operator's `UserKey`. Unverifiable rows are dropped fail-closed on load and re-verified at
authorize time. The mesh session responder checks this registry **at session-open**; an
unapproved, revoked, or foreign peer is refused.

**K4 — The docking ceremony reuses the shipped SAS machinery; the terminal promotes, the web only
proposes.** Commit-then-reveal transcript + 6-word BIP-39 SAS (`sas_transcript.rs`,
`sas_confirm.rs`, golden-vectored), binding the (currently-empty) `mesh_agent_fingerprint` slot to
the exact peer. **Stage-then-promote** exactly like passkey enrollment: the web can only write an
expiring proposal (no web-writable verdict); only the **terminal**, holding the root `UserKey` and
a sealed `PromptWindow`, promotes it into a signed approval after the operator compares the SAS
across the two terminals. A fully compromised browser can at most stage a proposal that expires.

**K5 — The TUI holds the kill-switch.** `/undock <peer>` and `/undock all` revoke approvals
(bump `issued_generation` / set `revoked`, re-signed with the root key, `PromptWindow`-gated,
exactly like `newt ocap revoke-credential`). `/dock disable` sets a durable "remote-HTMX enabled =
false" flag that fail-closes the whole remote surface. Revocation **terminates live docks**, not
just future ones: a bumped generation fails the responder's pull-check, which closes the mesh
session (fail-closed on verify failure), which drops the hub tab. Multiple docks are each
independently revocable; `/undock all` revokes every approval atomically.

**K6 — Coequal views, within each surface's morphology.** "Coequal, refresh each other" is
symmetric *views* + symmetric *enqueue*, never symmetric authority (K1). The web cockpit and the
RichTUI dock overview both mirror the same session set and can both enqueue; the LEAN TUI surfaces
remote/web activity as **provenance-tagged printed lines** through the line arbiter plus
`/dock-status` — staying within the plain-scroller charter (the 2026-08-11 amendment lets RichTUI,
but not LeanTUI, host the live overview pane).

**K7 — Transport is `session_streams`; composition follows the excluded-crate pattern.** Docking
rides the duplex `bus.open_session`/`handle_sessions` primitive (implemented upstream in
`agent-mesh` per its ADR). `newt-web` reaches the mesh by path-depending the already-excluded
`newt-mesh` crate (which grows a `NewtSessionService` responder + a dock client), so axum and the
mesh's QUIC stack never enter the agent workspace graph (D1). Dockability is advertised with a
`"newt-session"` capability tag.

## Non-goals (this decision)

- Co-driving / a second writer (K1 forbids it; the `session.rs` `Driver` role stays observer-only
  over the mesh for now).
- Cross-operator docking (K2 — deferred, needs `delegate_external` + handshake channel binding).
- A durable web-granted authority of any kind (web authority stays ephemeral, per
  `newt_web_htmx.md`).
- A LEAN-TUI live pane (charter amendment permits it only on RichTUI).

## Consequences

- newt-web gains a `newt-mesh` path-dependency and thus the QUIC stack in its own (isolated) build
  — a heavier web binary, accepted for the isolation D1 buys.
- The `session_streams` primitive is a hard upstream dependency in the sibling `agent-mesh` repo;
  its cadence gates the mesh phases. An interim chunked `publish_to`+`subscribe` fallback exists
  but is costlier and is avoided unless the primitive stalls.
- The dormant multi-attach model in `newt-core/src/session.rs` finally gets its first consumer.
- Revocation latency is bounded by the pull-check cadence; if that proves too slow, an explicit
  session-close signal is added.

## As-built (2026-08 security closure, PR #1643 + agent-mesh `5ff8f3f`, landed #75)

This section is authoritative where it differs from the aspirational K-text above.

- **K2 restated — same operator is *authentication*, not *authorization*.** One `UserKey` proves
  the caller is the operator; it does **not** grant access. Distinct `AgentKey` principals under one
  `UserKey` carry distinct authority: an unapproved sibling agent is denied even though the
  handshake admits it.
- **K3 as-built — authorization is at the RESPONDER, per request.** The transport is request/reply
  (not `session_streams` yet), so `NewtDockService` resolves the **verified caller agent
  fingerprint** — from `agent-mesh`'s `RequestContext` (the envelope signer, authenticated by
  `env.verify()`), never a value in the request body — against **its own** signed dock registry on
  every request, before any disclosure or side effect. The hub-side gate remains as defense in
  depth, but is no longer the sole check. **Fail-closed by default**; `NEWT_INSECURE_DOCK_NO_APPROVAL`
  is the one named, unsafe opt-out. The registry additionally enforces `fingerprint == BLAKE3(pubkey)`
  (no decoupled label) and is written crash-safely under a lock (`newt-core::atomic_fs`).
- **K3 scope — `DockScope` is typed authority, enforced per operation.** `Mirror` may list + read
  transcript; `MirrorInject` may also inject (D2). An unknown scope token fails to deserialize
  (fail-closed). CLI defaults to least authority (`--scope mirror`).
- **K4 as-built — the "SAS" is an honest pubkey cross-check, not a two-party ceremony (yet).** The
  6 words are derived from the peer **pubkey alone** (`dock_registry::pubkey_words`), so the peer's
  own newt-web prints the identical words for the operator to compare — a real fingerprint
  cross-check with no exchanged secret. A genuine two-party commit-reveal SAS (each side contributes
  entropy) is the cross-operator Phase-6 work; the earlier "compare the SAS across two terminals"
  framing described a comparison the peer could not satisfy and is retired.
- **K5 as-built — revocation linearization.** There is no live `session_streams` to close; the
  responder re-reads the registry per request, so once a revocation commits, the next request from
  that caller is denied (`approved()` excludes revoked rows). The `verify_at(gen)` pull-check the
  older text implied does not exist and its doc claim was removed.
- **K7 as-built — transport is request/reply over the bus.** `session_streams` (live duplex push)
  remains the future refinement; list / mirror / inject are covered by request/reply today.

### Dock grant audience — location-scoped bearer authorization (decided 2026-08-12)

The signed `DockRecord` preimage (`newt-core/src/dock_registry.rs` `signing_payload`) binds the
issuer (operator root `UserKey`), the subject, the **approved caller** agent fingerprint / label /
pubkey, the `DockScope`, the generation, the ceremony transcript, and the `revoked` flag. It does
**not** bind the resource-owning **responder**'s own identity. The hostile question this raises:

> If a valid signed approval authorizing caller *A* is copied verbatim from responder *B*'s
> `docks.d` into responder *C*'s `docks.d` (where *B* and *C* are distinct `AgentKey`s under the
> same operator `UserKey`), does *C* now authorize *A*?

Mechanically, yes — *C* verifies the record against the shared operator root and resolves *A*. That
is **intended**, and the audience semantics are made explicit here rather than left accidental:

```text
Dock grants are location-scoped bearer authorization records.
Possession in a responder's protected registry is itself the audience binding.
Copying a valid record into another responder's protected registry is an
operator-authority action, not a valid remote replay.
```

**Why this is sound — the write boundary is exactly the operator-root-key boundary.** Three
code-grounded facts:

1. **A responder can only *evaluate* a grant if the operator's *private* root key sits beside it.**
   `authorize_caller` → `load_docks_with_identity` → `agent_mesh_core::UserKey::load(identity.pem)`
   loads a **private** PKCS#8 key (created `0600`), and `docks.d` lives in the *same* `state_dir`
   under the *same* filesystem permissions. Any principal that can *write* a record into a
   responder's `docks.d` therefore also holds the private root key next to it and could **mint** any
   grant directly (including a correctly-targeted one) — so target-binding raises no bar against the
   only principal who can perform the copy.

2. **No remote path writes or resolves the registry.** The mesh transport is request/reply over
   QUIC and exposes no file-write primitive; every `DockRequest` handler (list / transcript /
   inject) reads sessions or enqueues into the conversation store — none touches `docks.d`. The
   web "stage-then-promote" flow (K4) lets a browser stage only an expiring proposal; only the
   terminal (holding the root key) promotes it to a signed record. A machine holding a synced
   `docks.d` but *not* `identity.pem` fail-closes (nothing resolves). So "A→B replayed at C" is not
   transport-reachable — it requires local operator-level filesystem access to *C*, under which the
   attacker already *is* the operator.

3. **There is no stable responder identity to bind to.** The dock service mints an **ephemeral**
   AgentKey per process (`mint_agent` → `AgentKey::issue` → `SigningKey::generate`), so a responder
   has no persistent mesh fingerprint a grant could name as its audience. The only stable,
   authenticated audience the architecture provides *is* the local registry (the `state_dir` +
   co-located operator root key).

**Honest caveat (non-blocking, future hardening).** If an operator syncs their whole `~/.newt`
(both `identity.pem` and `docks.d`) across machines, an A→B grant will authorize A at every synced
machine. That is a consequence of the operator declaring "these machines share my identity" (each
then holds the root key and is a full operator root), not a remote vulnerability. A future
hardening could bind a **persistent** responder dock-agent fingerprint (`target_agent_fingerprint`,
obtained from the local authenticated mesh identity and verified at authorize time) into the signed
preimage, making grants least-authority under config sync. It is **not** a landing blocker: it does
not change the trust boundary (already the root key) and it first requires introducing a persistent
responder dock-agent identity (today's is ephemeral). Tracked as a security-hardening residual.

Regression: `dock_registry.rs::a_dock_grant_is_a_location_scoped_bearer_record_gated_by_root_key_possession`
pins both halves — a copied grant resolves under a registry that holds the operator root key, and is
**inert** under one that does not.

## Amendment K8 — the docked host dials out (proposed 2026-09-26)

**Status:** Proposed. Operator direction, 2026-09-26; not implemented. Where it lands, it supersedes
K7's dial direction and nothing else: K1 (mirror + inject, single writer), K3 as-built
(authorization at the docked host, per request), the `DockScope` typed authority and the K5 kill
switch all stand unchanged.

### The thing being decided

The operator's workflow: on a laptop, `/dock newt.home.lab` makes that newt's sessions drivable
from the hub cockpit on the home cluster, **without the laptop running a web server or accepting
inbound connections**. The same shape serves always-on hosts: a k8s-hosted hub routes the operator
to sessions on `gnuc`, `nuc1` and `nuc2`.

### Why the as-built K7 cannot serve it

Each fact below is what forces a decision here.

- **The hub dials the docked host.** A mesh peer is configured on the hub as
  `label=mesh:<agent_pubkey_hex>@<ip>:<port>` and direct-dialed (`newt-web/src/dock.rs`,
  `parse_peers`). The docked host must therefore be reachable at a stable address and accepting.
- **The docked side is a newt-web process.** It becomes dockable by setting `NEWT_WEB_MESH_BIND`,
  which binds a `NewtDockService` on a UDP port inside newt-web (`newt-web/src/main.rs`,
  `init_mesh_dock`). A laptop would run the web binary and a listening socket: the thing the
  operator wants to avoid.
- **agent-mesh dial-back does not reach an idle laptop.** A reply "dial-back" is a fresh QUIC dial
  to the address a request arrived from (`agent-mesh-bus/src/bus.rs`, `dial_reply_peer` /
  `reply_dial_candidates`). Behind NAT it succeeds only while the laptop's outbound mapping is
  alive — seconds after the laptop last spoke, not hours later when the operator wants to take
  over. `BusOptions` has only `announce`; a quiet bind still accepts inbound connections.
- **Both dock keys are minted per process.** The hub's dock client and the docked host's responder
  each call `mint_agent` at startup (`newt-web/src/main.rs`, `init_mesh_dock`). An approval that
  names the hub's key is void after the hub restarts, and a hub cannot keep a durable list of the
  hosts that dock into it.

### Decisions

**K8.1 — The docked host initiates; the hub never dials a docked host.** A docked host holds one
long-lived outbound connection to each hub it is docked to, with keepalive and jittered
reconnect-with-backoff. It accepts no inbound connection for docking.

**K8.2 — Connection direction is decoupled from authority.** After the uplink opens, the hub is
still the *requester* (list, transcript, inject) and the docked host is still the *authorizer*: it
checks the verified caller — the hub — against its own signed dock registry on every request,
exactly as K3 as-built. Inject still only enqueues (K1); the docked host's running session remains
the sole writer. Opening an uplink grants the hub nothing the registry does not.

**K8.3 — Carrier: `session_streams`, with a named interim.** The preferred carrier is a
`session_streams` duplex session *initiated by the docked host*, over which the existing
`DockRequest` protocol runs unchanged; the hub is the session responder and checks the host's
caveats at open. That primitive is still design-only upstream (`agent-mesh`
`docs/decisions/session_streams.md`, status "proposed"; no `open_session` at `c137d9e`). Until it
lands, the interim is an **outbound long-poll over today's request/reply**: the host sends a
`dock/uplink/poll` request, the hub holds it until it has a `DockRequest` to deliver, and the
host's next request carries the result. Each reply follows a request by seconds, so dial-back
reaches the host while its NAT mapping is still alive. The interim is replaced, not extended, when
`session_streams` ships.

**K8.4 — Persistent dock identities on both ends.** The hub and every docked host hold a
persistent dock `AgentKey` under the operator `UserKey`, stored beside `identity.pem` in their
state dir. This is a prerequisite for K8.5, and it also closes the "persistent responder dock-agent
identity" hardening residual recorded above. A hub name such as `newt.home.lab` is an **address
candidate only** (agent-mesh `floating_identity.md`): the hub's identity is the pubkey pinned at
the ceremony, and a hub answering at that name with any other key is refused.

**K8.5 — The ceremony is two-sided and terminal-promoted.**
- *Host side.* The first `/dock <hub>` prints the hub's 6 pubkey words (`pubkey_words`) and the
  requested scope, and the operator confirms at the host's terminal. That writes the signed
  `DockRecord` approving the hub, `Mirror` by default and `MirrorInject` only on explicit request.
- *Hub side.* An unknown host's first uplink is only a staged, expiring proposal showing its words
  — the K4 stage-then-promote rule. It is promoted by `newt dock approve` on the hub's terminal
  (for a k8s hub, `kubectl exec` into the pod that holds the root key). An unpromoted host's
  sessions never appear in the cockpit.

**K8.6 — Verbs: serve and dock are different acts.**
- `/web` serves newt-web from this process: loopback by default, under the D3 auth tiers.
- `/dock <hub>` opens (and, the first time, pairs) an uplink.
- `/undock <hub>|all` revokes the grant and closes the uplink (K5).
- `/dock disable` is the existing kill switch and now also closes every uplink.
- `/dock status` adds each uplink's state (connected, retrying, revoked).

A `/remote-control` alias is a presentation choice outside this decision.

**K8.7 — The hub is a router for many hosts.** The hub cockpit lists the sessions of every
promoted host, grouped by host and addressed as `session@host`, in the same sidebar
(`docs/decisions/newt_web_htmx.md`, as restyled in #2605). The LAN direct-dial path of K7 remains
until K8 is proven, then is reviewed for retirement: two dock directions are two protocols to keep
secure.

### Non-goals

- NAT hole-punching, iroh relays, or hub-to-hub relaying: the host must be able to reach the hub
  (LAN, VPN, or an exposed hub endpoint).
- Cross-operator docking (K2, still deferred).
- Seat status, forwarded permission prompts and chat adapters in the hub — later rungs that build
  on K8, not part of it.

### Consequences

- **The hub concentrates authority.** It can inject into every host that granted `MirrorInject`.
  The mitigations are the ones already specified: `Mirror` by default, per-request authorization
  at each host, the host-side kill switch, and the hub behind the D3 ingress gate.
- **The host needs no web binary to be dockable.** The uplink lives in the newt process (via
  `newt-mesh`); newt-web runs only where the cockpit is served.
- **One upstream dependency.** agent-mesh needs a bind mode that accepts no inbound connection,
  so a docked host's "outbound only" is enforced by the transport rather than by the absence of
  a peer that knows its address.

### Ladder (one concern per PR)

- [ ] **K8-a** — agent-mesh: accept-none bind mode (Gilamonster-Foundation/agent-mesh#92).
- [ ] **K8-b** — persistent dock `AgentKey` for hub and host (K8.4); regression that an approval
  survives a hub restart.
- [ ] **K8-c** — `newt-mesh` uplink, interim long-poll carrier (K8.3); loopback test proving the host
  serves list/transcript/inject while accepting no inbound connection.
- [ ] **K8-d** — hub side: accept uplinks, stage and promote hosts (K8.5), list sessions by host
  (K8.7).
- [ ] **K8-e** — TUI verbs `/dock <hub>`, `/undock`, `/dock status` (K8.6).
- [ ] **K8-f** — swap the carrier to `session_streams` when agent-mesh ships it.

# Decision: SSH-carried mesh docking and an SSH-gated newt-web cockpit

**Status:** Proposed (for Shawn Hartsock review).
**Date:** 2026-08-14
**Related:** `docs/decisions/newt_web_docking.md` (hub cockpit, K1 mirror+inject, K3 registry,
K4 SAS), `docs/decisions/newt_web_htmx.md` (D1 placement, D2 single-writer, D3 auth),
`docs/decisions/mesh_integration.md` (excluded-crate pattern),
`agent-mesh/docs/decisions/ssh_transport.md` (the SSH carriage, PR #77), and NVIDIA OpenShell
(`OpenShell/docs/reference/gateway-auth.mdx`) as the comparison point.

## The goal being decided

Run **one operator's** newt-agent + newt-web as a **hub** that controls *multiple* other agents
that **dock** into the hub's newt-web for a given newt. Carry **everything over SSH**. Secure
newt-web *behind* an SSH + authN/Z ceremony. Do it easily, with the strongest security stance
available.

## What already exists (ground truth, do not rebuild)

- **Docking is real and shipped** (PR #1643 + agent-mesh `5ff8f3f` / #75). A hub newt-web
  surfaces docked remote sessions; the **responder** authorizes per request against a signed,
  revocable dock registry (`dock_registry.rs`), enforcing `fingerprint == BLAKE3(pubkey)` and a
  typed `DockScope` (`Mirror` / `MirrorInject`). Fail-closed; the only opt-out is the named
  `NEWT_INSECURE_DOCK_NO_APPROVAL`.
- **Identity is operator-rooted.** One `UserKey` authenticates the operator; distinct `AgentKey`s
  under it carry distinct authority. The root key cross-signs the operator's SSH/GitHub ed25519
  key (`amesh bind github`), so the SSH key is *already* the identity root.
- **newt-web already has the auth seams.** Fail-closed WebAuthn relying-party checking
  (`webauthn.rs`), a fail-closed trusted-forward-auth header gate (`NEWT_WEB_AUTH_HEADER`), and a
  stage-then-promote enrollment ceremony (`enroll.rs`) where the browser can only stage an
  expiring proposal and the terminal (holding the root key) promotes.
- **The SSH carriage now exists** (`agent-mesh-transport-ssh`, PR #77): the same signed envelopes
  over an authenticated SSH channel (`ssh -W` direct-tcpip, system OpenSSH binary; v1 defers the
  server accept to system `sshd` + loopback).

So the question is not "build docking" — it is "how do we gate the whole thing behind SSH cleanly."
The gaps are small and named below.

## Comparison: what OpenShell does (and why we are not copying it)

OpenShell solves a **different problem**: it is a **sandboxed runtime** that confines a single
agent's blast radius (deny-by-default egress via a CONNECT proxy + OPA, Landlock filesystem,
seccomp process, inference proxy). Its **gateway auth** (`gateway-auth.mdx`) is how its CLI
reaches its control plane: **mTLS** by default for local/single-user gateways, **OIDC** for
Kubernetes, with a per-gateway credential bundle on disk.

| Concern | OpenShell | This design |
|---|---|---|
| Primary job | Confine one agent's execution | Hub controls many docked agents |
| Control-plane auth | mTLS (local) / OIDC (k8s) | **SSH (transport) + operator-root-signed mesh envelopes** |
| Identity root | TLS client CA / OIDC issuer | **operator `UserKey` cross-signed to the SSH key** |
| Threat it leads with | data exfiltration / sandbox escape | unauthorized dock / rogue peer |

We borrow one OpenShell idea — **fail-closed, deny-by-default, no-permissive-mode** — which newt
already practices (the WebAuthn RP and dock registry are both fail-closed). We do **not** adopt
its mTLS/OIDC control plane: our identity root is the operator key, and SSH is the mandated
carriage, so a parallel PKI/OIDC stack would be redundant machinery.

## Decisions

**S1 — SSH is the carriage for docking; the mesh envelope is the authN/Z.** Docking rides
`agent-mesh-transport-ssh`. SSH (machine-authenticated, encrypted, key-authorized) replaces the
raw LAN-QUIC assumption. The mesh layer's signed envelopes + responder-side dock registry remain
the *authorization* — two independent layers, two failure modes. A spoke needs only **outbound
TCP/22** to the hub; no mesh ports are exposed.

**S2 — newt-web binds to loopback; SSH is the only way in.** The hub cockpit never listens on a
routable interface. The operator reaches it over an SSH local forward:
`ssh -L 8880:127.0.0.1:8880 operator@hub`. sshd *is* the outer authN ceremony (key-only,
`AllowTcpForwarding local`, `PermitOpen 127.0.0.1:8880`). This needs **zero new newt-web code**.

**S3 — Inside the SSH tunnel, newt-web keeps its existing fail-closed auth as defense in depth.**
Loopback is *not* the authorization boundary; it is a transport convenience. newt-web still runs
its trusted-identity-header gate (or WebAuthn) so that a process on the hub box that can reach
loopback is not automatically the operator. SSH gets you *to* the box; the in-app gate decides
*who you are*. This mirrors how the dock registry gates even after the SSH channel is open.

**S4 — Cross-network docking today uses a persistent SSH forward; first-class `transport-ssh`
dialing is the build item.** Until the SSH backend's direct-dial path is wired into the hub's
dock client, carry the existing LAN-QUIC mesh over an `autossh` persistent forward. Then flip the
hub's dock peer config from `label=mesh:<pubkey>@<ip>:<port>` to an `ssh://` target so the dial
itself is SSH-authenticated. This is the one real piece of new work.

**S5 — One revocation story, two levers.** A docked agent is killed at the **mesh layer**
(`/undock <peer>` / `/undock all`, re-signed registry bump, responder denies next request) and
independently at the **transport layer** (remove the spoke's pubkey from the hub's
`authorized_keys`). Neither depends on the other; both are fail-closed.

**S6 — The hub-side SSH accept is system `sshd` + loopback (v1); in-process `russh` server is the
planned v2.** v1 terminates SSH at sshd and forwards to the hub's loopback mesh port — zero new
server code, leaning on hardened sshd for authN/crypto/revocation. v2 (in-process russh) binds
the authenticated SSH username directly into the mesh router for a stronger identity story, and is
deferred until the `p256`/`p521` rc-dependency conflict with iroh's pinned `ed25519-dalek`
resolves (documented in `agent-mesh/docs/decisions/ssh_transport.md`).

## Threat-model fit

| Threat | Mitigated by |
|---|---|
| Rogue host tries to dock | sshd `authorized_keys` (needs a real key) **and** responder-side dock registry |
| Stolen mesh token alone | Useless — cannot open an SSH channel without an authorized key |
| Stolen spoke SSH key alone | Opens a channel but cannot produce valid signed envelopes / a dock approval |
| Process on the hub box hits loopback newt-web | Blocked by the in-app trusted-header / WebAuthn gate (S3) |
| Docked agent goes rogue | `/undock` (mesh) and/or `authorized_keys` removal (transport) — kill at either layer |
| MITM on the path | SSH host-key verification **+** the 6-word pubkey cross-check at the mesh |
| Spoke behind NAT / no inbound ports | Spoke dials outbound 22; hub reaches back over the same connection |

## Honest caveats

- **v1 terminates SSH at sshd, then plaintext loopback to the mesh/newt-web port.** Fine on a
  single trusted host (same trust model as the newt-web loopback listener). If the SSH auth must
  *cryptographically* flow into the mesh router (authenticating even the loopback hop), that is
  exactly the v2 in-process russh server. Named now so it is a planned upgrade, not a surprise.
- **No double crypto in v1.** SSH already encrypts; we do not also run Noise inside the channel.
  Revisit only if the threat model demands end-to-end past the sshd termination point.
- **OpenShell is a complement, not a dependency.** If we later want to *confine what a docked
  agent can do on its spoke* (egress/fs/process), OpenShell's sandboxing is the right tool for
  that — orthogonal to the docking transport and the web gate.

## Consequences

- The runbook (`.scratch/.../ssh-mesh-docking-runbook.md`) operationalizes S2/S4: sshd hardening,
  the `-L` forward, the `autossh` persistent forward, and the peer-config flip to `ssh://`.
- The one build item is S4's first-class `transport-ssh` direct-dial path in the hub's dock
  client (`newt-web/src/dock.rs` mesh branch), reusing PR #77's client connect.
- No new PKI/OIDC; identity stays operator-rooted and SSH-native.

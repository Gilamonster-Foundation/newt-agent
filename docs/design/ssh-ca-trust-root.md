# SSH-CA trust root + dual-transport mesh

**Status:** Design note (no implementation). 2026-06-18.
**Builds on:** `docs/decisions/mesh_integration.md` (the iroh/QUIC + ed25519
`CertChain` substrate), `docs/decisions/agentic_object_capability_security.md`
(the `Caveats` lattice), `docs/design/captured-shell-ocap.md` +
`captured-shell-cross-platform.md` (the authority plane + per-OS sandbox),
`docs/design/mesh-remote-control-mobile-app.md` (UserKey→AgentKey enrollment /
attenuated delegation), `docs/design/centaur-swarm-architecture.md` (the three
planes), `newt-core/src/agent_identity.rs` (#329), and `newt-core/src/git_caveats.rs`
(#454 — the sibling custom OCAP surface this mirrors).

---

## TL;DR

- **The human's SSH public key is the swarm's CA / root of trust.** Registered at
  GitHub (`https://github.com/<user>.keys`), GitLab, or enterprise LDAP/SSO, it
  signs **attenuated OpenSSH certificates** for swarm members; the cert's
  principals/validity/extensions carry (or reference) the OCAP `Caveats`. This is
  the mesh-remote §4 `root → worker → phone` delegation, expressed as SSH certs and
  anchored on a directory the human already controls.
- **That identity is transport-independent.** The mesh runs over **two transports**:
  **iroh/QUIC for LAN** (mDNS, P2P hole-punch, low latency — unchanged) and **SSH
  for long-haul** (WAN, bastion/jump-host traversal, and Windows reliability). The
  signed ed25519 envelope + caveats ride *inside* whichever transport.
- So "all mesh over SSH" is refined to: **the SSH *key* is the unified identity on
  both transports; the SSH *protocol* is the long-haul transport.** iroh is not
  abandoned — the change is strictly **additive (no regression)**.
- Engine: **russh** (pure-Rust client + server, ed25519, crates.io, no C) for the
  SSH transport; OpenSSH **certificate validation** rides the `ssh-key` crate russh
  already depends on. OCAP surface: **`SshCaveats { hosts, keys }`** — a sibling of
  `GitCaveats`.

---

## 1. Motivation

Two problems today:

1. **Three identities.** The git SSH key, the mesh UserKey (`~/.newt/identity.pem`),
   and the OCAP root of trust are separate keys. They are all ed25519; there is no
   reason they cannot be **one** key.
2. **One transport with reach gaps.** agent-mesh is iroh/QUIC + mDNS today. mDNS is
   multicast and does not cross a WireGuard tunnel (mesh-remote §9), QUIC/UDP is
   firewall-hostile in many enterprise networks, and **iroh/QUIC has been unreliable
   on Windows**. A second, TCP-based, NAT/bastion-friendly transport that reuses
   existing SSH reachability closes those gaps.

The unification: make the SSH key the single root of trust, and add SSH as a second
transport — without touching the iroh path that already works on LAN.

## 2. The trust root — the human's SSH key as a CA

- **The key is the UserKey.** An ed25519 SSH key is the same primitive as the mesh
  UserKey, so it *is* the root. AgentKey delegation (`mesh_integration.md`,
  agent-identity #329) becomes **SSH-certificate signing**.
- **Signed SSH certs = attenuated delegation.** The CA (human key) signs a member's
  key into an OpenSSH certificate carrying:
  - `principals` — the swarm role / identity the member may claim;
  - `valid before/after` — short-lived, so expiry *is* the baseline revocation;
  - `extensions` — where the member's `Caveats` (and `GitCaveats` / `SshCaveats`)
    are encoded or referenced.
  A member cert's authority is `⊑` the CA's; `meet` composes along the chain. This
  is the same attenuation-only algebra as `Caveats` — amplification is impossible by
  construction.
- **External anchoring proves the human, not the PKI.** `github.com/<user>.keys`
  (or GitLab / LDAP / SSO key registration) publishes the human's public keys, which
  lets any peer verify "this CA key belongs to `hartsock`." GitHub does **not** host
  SSH-CA semantics — the CA→member *signing* is the swarm's own PKI; the directory
  only anchors **identity**. Multi-provider by design (not GitHub-only).
- **Revocation.** Short cert validity + a Key Revocation List (KRL); the issuer
  (CA, or a delegating worker) drops a fingerprint. The mesh-remote §5.4
  "worker-as-revoker" story, in SSH-cert form.

## 3. Identity is transport-independent (the key insight)

The ed25519 key authenticates over **both** transports:

- **Over iroh/QUIC** — it is the iroh node identity *and* the per-envelope signer
  (the mesh already signs ed25519 envelopes, `mesh_integration.md` / mesh-remote §5.2).
- **Over SSH** — it is the SSH session key, presented as the CA-signed certificate.

The **signed envelope** (per-message ed25519 signature + monotonic sequence + nonce)
rides *inside* whichever transport, and **`Caveats` ride the envelope**. So
per-message attribution, replay defense, and authority are **identical regardless of
carrier**. The transport is a dumb pipe; identity + authority live above it.

## 4. Dual transport — iroh/QUIC (LAN) + SSH (long-haul, Windows)

| Axis | iroh/QUIC | SSH (russh) |
|---|---|---|
| **LAN / local** | ✅ primary — mDNS discovery, P2P hole-punch, low latency | available, not preferred |
| **Long-haul / WAN** | needs WireGuard/relay; mDNS can't cross | ✅ primary — bastion/jump-host, reuses SSH reachability + the CA |
| **Windows** | ⚠️ unreliable (QUIC/UDP firewall, observed trouble) | ✅ native OpenSSH/sshd, TCP, firewall-friendly |
| **Identity** | the SSH key (node id + envelope signer) | the SSH key (CA-signed cert) |

- **Selection is reachability- and platform-aware.** A peer advertises both
  reachabilities (iroh node addr / mDNS, and SSH `host:port`). The dialer prefers
  iroh when same-LAN-reachable; falls back to SSH for long-haul; **prefers SSH on
  Windows**. Mirrors the mesh-remote "direct-dial known peer" path.
- **Bounded surface.** A `Transport` trait with `IrohTransport` + `SshTransport`
  impls; the bus/envelope/caveats layer is transport-agnostic, so the two carriers
  share everything above the wire. Over SSH, BusMessages flow on an SSH channel
  (subsystem or `exec`/`direct-tcpip`); session auth is the CA-signed cert.
- **No regression.** The iroh path is untouched; SSH is added alongside. A peer with
  only iroh keeps working exactly as today.

## 5. The OCAP surface — `SshCaveats`

A sibling of `GitCaveats` (#454): a separate capability lattice composed by `meet`
*alongside* the signed `Caveats`, never merged in.

```rust
pub struct SshCaveats {
    /// Hosts this surface may connect to (git remotes, mesh peers, bastions).
    pub hosts: Scope<String>,   // default none — fail-closed
    /// Keys (by fingerprint) this surface may use / accept.
    pub keys: Scope<String>,    // default none
    // actions (exec/forward/subsystem) deferred — hosts + keys first.
}
```

- `permits_host(host)` gates the SSH **client** (git push, mesh long-haul dial);
  `permits_key(fp)` gates which key/cert may be used or accepted.
- The SSH **server** (mesh accept, captured shell) validates the inbound cert against
  the CA, then consults the peer's `Caveats` — `meet(local_policy, peer_caveats)`.
- Keep to **hosts + keys** for now (per the directive); `actions` (port-forward,
  subsystem, exec class) is a later axis.

## 6. Reconciliations (so the design isn't self-contradictory)

- **"All mesh over SSH" vs keeping iroh** → resolved by §3–4: the SSH *key* is the
  identity on both transports; the SSH *protocol* is the long-haul/Windows transport.
  iroh stays for LAN.
- **mesh-remote explicitly *rejected* SSH** as the interactive transport (a real SSH
  shell = the unix account's full authority — the confused deputy the OCAP design
  kills). **Still holds.** Here SSH is **envelope transport + cert identity only** —
  the agent gets an OCAP-gated channel whose authority comes from the cert's caveats,
  **never a PTY / unix shell**. The distinction must stay explicit in code (no
  `session-exec` to a shell on the OCAP path).
- **GitHub anchors identity, not CA semantics** — see §2.

## 7. OCAP deviation-ratchet ties

Long-haul networking is exactly what the open deviations gate
(`docs/security/ocap-deviations.md`):

- **Accepting inbound SSH** (the mesh / captured-shell server) is dangerous —
  untrusted bytes + remote principals with no OS sandbox. It carries
  `OCAP-DANGER: b1-os-isolation` (+ `disclosure-gate-live-path` for content reaching
  the model) and ships **fail-closed** until `verify_b1()` passes — same teeth as
  grit's network slice (PR5).
- **Outbound SSH** (git push, mesh dial carrying credentials) is gated likewise.
- `SshCaveats` is the *capability* layer; the deviation ratchet is the
  *enforcement-readiness* layer. Both must pass.

## 8. Engine notes (russh)

- Pure-Rust, no mandatory C deps; **client + server**; ed25519; crates.io. MSRV to
  confirm in a spike (like grit; the workspace floor is now 1.88 after #452).
- **OpenSSH certificate validation is not in russh's own auth API** — it rides the
  `ssh-key` crate (RustCrypto) that russh already pulls. So cert-chain verification
  (CA signature, principals, validity window, KRL check) is **glue we write in the
  `server::Handler`**. This is the main integration point to prove in the spike.

## 9. Phasing + relation to grit

- **grit LOCAL ops (PR2–PR4)** are independent of all of this — they proceed now.
- **grit's NETWORK slice (PR5)** rides the SSH **client** transport once it exists,
  instead of inventing its own.
- **SSH/russh track** (much of it in the agent-mesh sibling repo):
  1. russh spike — MSRV on 1.88, cert verify via `ssh-key`, client+server echo.
  2. `SshCaveats` type (newt-core, sibling of `GitCaveats`) + tests.
  3. SSH **client** transport — git push/pull + mesh long-haul dial, gated by `SshCaveats`.
  4. SSH **server** — mesh accept / captured shell, fail-closed under the ratchet.
  5. The SSH-CA **cert identity** — sign/verify member certs; GitHub/GitLab/LDAP anchor.
  6. The `Transport` trait + iroh/ssh **selection** (reachability + platform aware).

## 10. Open questions

- **Cert format:** adopt OpenSSH certs *as* the mesh `CertChain`, or bridge
  agent-mesh-protocol's `CertChain` ↔ OpenSSH cert? (russh/`ssh-key` speak OpenSSH;
  agent-mesh has its own ed25519 cert.)
- **Where `Caveats` live in a cert:** a custom OpenSSH cert `extension` vs a
  side-channel keyed by fingerprint.
- **KRL distribution:** a signed revocation topic on the mesh (mesh-remote §5.4).
- **Directory abstraction:** `github.com/.keys` polling/caching + a multi-provider
  (GitLab / LDAP / SSO) trait.
- **Ownership:** does the `Transport` trait + SSH live in agent-mesh (likely) or
  newt? The trust root (`agent_identity` / UserKey) spans both.
- **Transport advertisement/negotiation** wire format.

## 11. Out of scope

- Implementation (this is design only).
- The mobile app itself (mesh-remote-control-mobile-app.md owns that — this is the
  transport + identity substrate beneath it).
- Replacing iroh/QUIC (kept for LAN — explicitly no regression).
- The `actions` axis of `SshCaveats` (port-forward / subsystem / exec) — hosts + keys first.

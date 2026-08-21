# Centaur swarm — top-level architecture

**Status:** Architecture map (2026-06-16). The spine above the detail notes:
[`crew-loadout.md`](crew-loadout.md), [`captured-shell-ocap.md`](captured-shell-ocap.md),
[`context-scheduler.md`](context-scheduler.md), [`workflow-swarm-harness.md`](workflow-swarm-harness.md),
[`model-support-kit.md`](model-support-kit.md), and the OCAP deviation register
[`../security/ocap-deviations.md`](../security/ocap-deviations.md). Serves epic #314.

## The system in one line

**A human pilots a diverse swarm from a phone, across the house's machines, safely** —
and **agent-mesh is the one wire through all of it**.

## Three planes, one substrate

agent-mesh (iroh-QUIC P2P, mDNS presence/discovery, BLAKE3 identity, signed cert chains
with attenuated Caveats) carries three planes. Everything we build sits on one of them:

| Plane | Carries | Pieces | Gate |
|---|---|---|---|
| **Control** | phone → host pilot commands (attach a flight, deploy a crew, approve a carve-out, watch) | Phone/Pilot plan (epic #314, `mesh-remote-control-mobile-app.md`), `newt pilot` | **multi-attach (#62)** |
| **Data** | host → inference peers; dispatch roles across the breathing pool | crew loadout, `BackendPool`, `MeshAsker`/model-pin/presence | the `newt-scheduler` pool layer |
| **Authority** | attenuated Caveats on *every* envelope (`delegate ⊑ parent`) | captured-shell OCAP, the policy assistant, per-voice attenuation | **the deviation ratchet (#84)** |

The phone, the inference pool, and the per-voice containment are **the same network** —
the human's authority flows from their root key, down through attenuated delegations, to
each voice, over the same channel that carries the work.

## The layered picture

```
   📱 PHONE — pilot: deploy · approve · judge · steer        (control plane)
        │
   🎛  flight — a running swarm the human attaches to (#62 multi-attach)
        │
  ┌─────┴───────────────┐
  ▼                     ▼                         loadout = one voice
 CREW                 PANEL                        provider→model→kit→role→settings
 division of labor    decorrelated diversity
  │                     │                                    (data plane)
  ▼                     ▼   dispatch by model-pin, with failover
   🌐 agent-mesh BackendPool — DGX + gpu-runner + intermittent Windows, *breathing*
  │                     │                                    (authority plane)
  ▼                     ▼   each voice runs attenuated + contained
   🔒 captured-shell OCAP per voice — the Confused-Deputy defense
```

## The thesis: diversity ↔ containment

Diversity broadens the solution space (decorrelated voices beat groupthink) — that's the
*panel*, and it's why you want heterogeneous model families on different machines.
Containment (attenuation + curated-context + verify-gate, all riding agent-mesh) is what
makes a weird/foreign/remote voice *safe* — that's the captured-shell OCAP. **They are the
same coin: you can only widen the swarm to the degree each voice is contained.** And the
human holds the stamp — from the phone. The Centaur split: the swarm generates and
explores; the human judges and authorizes.

## The authority plane operates by a *deviation ratchet*

We will ship before OCAP is fully enforced (the threat model found B1 unbuilt, the
disclosure gate off the live path, etc.). To do that *without lying about security*, the
authority plane runs on one rule:

> **Effective authority = `meet`( the human's grant, what the currently-verified invariants
> can actually enforce ).**

Every dangerous capability (seed-a-live-credential, run-an-untrusted-remote-voice,
exec-an-interpreter, write-outside-workspace) declares the OCAP invariants it requires. At
runtime newt verifies which invariants hold; a capability is available **iff** all its
invariants verify, else it is **fail-closed OFF** with an honest banner. A *deviation* is
an invariant currently absent — and the system **caveats its own authority down** to match
what it can enforce, rather than pretending. Closing a deviation (building the invariant)
removes a self-caveat and **unlocks** its capabilities — raising the ceiling toward, never
above, the human's grant. Monotonic convergence: **zero deviations = full OCAP = full
function.** The register and the mechanism live in
[`../security/ocap-deviations.md`](../security/ocap-deviations.md).

## The critical path (three gates, mostly parallel)

1. **Data plane moves now.** The crew MVP (#85) — the boring two-pass machine on gpu-runner+DGX
   — proves dispatch + failover across the pool. Its mesh substrate is built; the net-new
   is the `newt-scheduler` `BackendPool` layer.
2. **Control plane is the Phone gate.** #62 multi-attach (epic #314) + the pairing flow in
   `mesh-remote-control-mobile-app.md`. The phone is a mesh peer on the auto-team; "pilot a
   flight" is attach-by-flight-id.
3. **Authority plane is the trust gate.** The deviation ratchet lets the swarm run
   **trusted-code tasks across the pool today** (no live credential, no fully-untrusted
   remote voice). Closing the OCAP deviations (#84) unlocks the dangerous, high-value parts
   (credentials, genuinely-untrusted diversity) one ratchet-click at a time.

**Net:** the swarm-over-the-pool and the phone-pilot can both progress now; the OCAP
deviation ratchet is what lets us take function *today* while guaranteeing a guided,
enforced path back to the full OCAP vision.

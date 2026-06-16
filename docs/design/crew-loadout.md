# Crew loadout — a role-specialized ensemble over a heterogeneous, availability-adaptive pool

**Status:** Design (2026-06-16). MVP not built. Builds on the loadout/kit work
(merged), the context-scheduler spec (#396), and `workflow-swarm-harness.md`.

## 1. The idea

A **crew loadout** is a named ensemble of *role-loadouts* + a *control program*. It is
the **division-of-labor sibling** of the diversity panel: a panel runs the *same* task
through *diverse* voices and votes (decorrelation, anti-groupthink); a **crew** runs
*different roles* in *sequence* with hand-offs. **Same machinery — a named set of
loadouts + a scheduler — opposite intent.**

The motivating instance is the three-model coding pattern: a **planner/editor**, a
**repo-navigator**, and a **lint/test triage** model. The harness owns the loop; the
models are workers, admitted, given bounded context, allowed one role, then preempted —
"keep the dragon in the lab." This is the cooperative scheduler of #396, made concrete.

```toml
[loadouts.planner]
provider = "dgx"            # or gnuc — both have qwen3-coder:30b
model    = "qwen3-coder:30b"
kit      = "coder"          # plan / patch / review techniques
role     = "planner-editor"
  [loadouts.planner.settings]   # temperature 0.2 → ModelTuning

[loadouts.navigator]
provider = "dgx"            # devstral-small-2:24b is DGX-only
model    = "devstral-small-2:24b"
kit      = "navigator"      # read-only repo-search tools
role     = "repo-navigator"

[loadouts.triage]
provider = "gnuc"          # small, local, always-resident
model    = "qwen2.5-coder:3b"
kit      = "triage"
role     = "test-triage"
  [loadouts.triage.settings]    # temperature 0.0

[crews.coder]               # NET-NEW: the ensemble + control program
planner = "planner"; navigator = "navigator"; triage = "triage"
loop    = "patch-revise"
  [crews.coder.budgets]
  max_attempts = 4; max_files_touched = 12; max_lines_changed = 800
  require_human_review_on = ["auth","crypto","migrations","test-deletion","deps"]
```

## 2. The control loop (the "boring two-pass machine" first)

```
navigator (Devstral) → context packet
   → planner (Qwen3-Coder) → plan + patch
   → harness applies patch in an ISOLATED worktree (never model→fs directly)
   → harness runs ONE test command
   → triage (small) → compact failure packet
   → planner revises once
   → emit diff   (repeat until pass / budget / human-review)
```

Guardrails (from the proposal, and they are our existing seams): patches not direct
writes; test execution outside the model; structured JSON contracts (the canonical
`Plan`/`Subtask` structs, #71); budgets; record every step (the failure corpus). The
harness is the scheduler; the models are processes.

**Build the boring version first** — a two-pass patch machine, *not* swarm orchestration.
That exposes the real problems (latency, swap cost, failover) on real hardware before the
scheduler crate exists.

## 3. The heterogeneous pool (gnuc + DGX today; intermittent peers later)

The measured pool:

| Role | Model | Where | Note |
|---|---|---|---|
| planner | `qwen3-coder:30b` | DGX **or** gnuc | failover-able |
| navigator | `devstral-small-2:24b` | **DGX-only** | pin to DGX (or pull to gnuc as backup) |
| triage | `qwen2.5-coder:3b` | gnuc | always-resident, never swaps |

With **two machines**, the primary mechanism is **placement** (put roles on different
backends), not **model-swapping** on one GPU. Happy-path placement → **zero swaps**.
Swapping becomes a *fallback* for when a role's model is resident nowhere and no free
backend can take it — exactly the case the scheduler is for.

### Model-residency scheduling — the gap #396 didn't cover
`#396`'s `kv-warden` schedules **KV-cache admission for one resident model**. A crew on a
*tight* GPU also needs **model-residency scheduling** — *which weights are loaded*.
Principle: **treat model-load as the serialized expensive resource.** Batch by model to
minimize swaps; keep the small triage model co-resident; time-share the big-model slot.
With the gnuc+DGX pool this mostly dissolves into placement; it re-appears only when a
single backend must serve two big models. The `Engine` trait abstracts the mechanism
(Ollama `keep_alive` pin/evict vs vLLM server lifecycle). **The DGX is itself
intermittently busy** (70b models hog VRAM) — "busy" is not "dead"; the pool must
distinguish them.

## 4. agent-mesh as the availability layer (grounded against the real code)

The pool must **breathe**: model "available backends" as *who is announcing right now*,
not a static machine list. A flaky Windows box is then normal-mode (in when it announces,
gone when it leaves), not an error case. A code-grounding pass (`wf wdjb57cxu`) mapped
exactly what agent-mesh / newt-mesh provide:

### Reuse — already built in agent-mesh / newt-mesh
- **Presence + discovery (mDNS):** `Announcer` publishes TXT records (agent_fp,
  `capabilities` incl. `CAPABILITY_TAG="newt-inference"`, role, host, port); `Browser` +
  `PeerResolver` maintain a live `HashMap<Fingerprint, PeerInfo>` and `resolve(fp,
  timeout)`. This is the "who's here right now" signal — **for free, per LAN segment.**
- **Attenuated request/reply:** `MeshAsker::ask(peer_fp, InferenceRequest, timeout) ->
  InferenceReply` over iroh-QUIC with correlation-id matching (`newt-mesh/src/ask.rs`).
- **Caveats over the wire:** every message rides a `SignedEnvelope` carrying the sender's
  `CertChain` (ed25519, BLAKE3-fingerprinted); `attenuate()`/`delegate()` structurally
  refuse to widen, re-checked `⊑` at every link. **Per-voice attenuation is built.**
- **Reply honesty:** `InferenceReply` carries a mandatory `model_id` + inline `error`, so
  "peer reachable but declined / model-pin mismatch" is distinguishable from "unreachable"
  — the exact signal a failover loop needs.
- **Model pin:** `InferenceRequest.model = Some(id)` forces the responder to serve *that
  exact model* or return an inline error — makes "navigator MUST be devstral-small-2:24b"
  *enforceable*, not hopeful.
- Transport security (per-envelope ed25519 sig, sequence+nonce replay defense, ALPN-pinned
  QUIC, same-`user_fp` auto-team handshake); `EndpointKind {Ollama, OllamaLb, InCluster,
  Vllm}` already describes gnuc + DGX; `Plan`/`Subtask` + per-subtask default-deny
  `CaveatPolicy`.

### Net-new — a focused `newt-scheduler` layer (NOT a mesh rebuild)
1. **`BackendPool`** (spec-only today): a registry of `0..N` `PoolBackend`
   (`LocalOllama | DgxNode | MeshPeer`) each with tier support + health + reachability.
   Answers *"given Tier + optional model pin, which LIVE backend?"* and reports `len()` to
   pick strategy (`0`→refuse, `1`→time-slice, `N`→fan out). **The core net-new artifact.**
2. **Pluggable `PoolSource`** trait: `StaticSource` (gnuc+DGX from config, today) +
   `MeshSource` (drains `PeerResolver`/`Browser` events, filters `capability ==
   "newt-inference"`, reads model+tier from the signed TXT). **The pool must not know
   whether an entry came from config or from mDNS** — that's what makes intermittent peers
   *additive*.
3. **Health/reachability model:** agent-mesh has **no heartbeat** (mDNS TTL + `port!=0` is
   the only liveness). Reuse the manual probe precedent (`DgxTelemetry::try_connect`, 3s
   timeout) for local/DGX; treat mDNS `Removed` as "gone" for mesh peers. Must track
   `last_probe` and a **busy-vs-down** distinction.
4. **Failover/retry loop:** `MeshAsker` is single-peer, single-timeout, no retry. On
   timeout / unreachable / inline-error, re-select another LIVE pool member satisfying the
   *same* tier+model and re-dispatch; bounded attempts; fail the subtask only when the pool
   genuinely can't serve it.

### MVP recommendation
Wire gnuc + DGX as **direct backends** today (no mesh needed for two stable machines) —
but behind the `BackendPool` + `PoolSource` abstraction, so adding the Windows peers later
is "they announce" (a `MeshSource`), **not a rewrite**. Even with two backends you need the
health + failover path, because the DGX is intermittently busy.

## 5. Composition with the OCAP threat model

The crew runs on the captured-shell OCAP substrate
([`captured-shell-ocap.md`](captured-shell-ocap.md)). The signed Caveats ride *with* the
mesh request, so **per-voice attenuation extends to remote peers** — a Windows box is a
*fine* home for an untrusted diverse voice precisely because it's network-isolated from the
DGX. **But:** per the threat-model verdict, **no live credential is seeded into a remote
peer until OS-isolation (B1) is a fail-closed precondition** — remote peers are the least
trusted. The crew MVP is unblocked because coding tasks on the repo don't need the
`pa` credential; credential-seeding is a later, B1-gated step.

## 6. Where it lives & open questions
- **Crate:** `newt-scheduler` (default build), mesh fan-out behind the `mesh` feature
  (`newt-mesh` path-deps `agent-mesh`; cargo validates eagerly even behind a feature).
- **Open:** crew config grammar (`[crews.*]`); the control-loop as a first-class program
  vs a `Workflow`-style script; the residency scheduler's swap-minimization policy;
  whether `MeshSource` capability claims need cryptographic verification before a peer is
  poolable (mDNS TXT is *claimed*, not yet verified).

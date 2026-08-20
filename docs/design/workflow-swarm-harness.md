# Workflow / Nested-Context Swarm Harness — design note (Workstream C)

Status: design note (no implementation) — the spike comes after this lands
Crates touched (proposed): new `newt-scheduler`, leaning on `newt-core`
(`dgx`, `router`, `agentic::driver`, `caveats`), `newt-identity`, and —
behind a feature flag, outside the default workspace — `newt-mesh`.
Related: [Workstream A](./progressive-disclosure-memory.md) and
[Workstream B](./coder-symbolic-memory.md) (the same disclosure principle at the
turn / session scale); [`mesh_integration.md`](../decisions/mesh_integration.md)
(why `newt-mesh` is excluded from the default workspace);
[`agentic_object_capability_security.md`](../decisions/agentic_object_capability_security.md)
(the attenuation-only authority model this builds on);
[`role-profiles.md`](./role-profiles.md) (one airframe, many roles).

**Be honest up front: this is the biggest and most aspirational of the three
workstreams.** A and B are pure `newt-core`/`newt-coder` work with no infra
dependencies. C is an orchestration layer that touches identity, the mesh, the
DGX formation model, and the agentic driver all at once, and its end-state
(parallel fan-out across a DGX array) depends on hardware that does not exist
yet. The deliverable *here* is the design note. The first code is a deliberately
small single-backend spike (§9). Everything past that grows as the hardware does.

---

## 1. Thesis — the disclosure principle at orchestration scale

The through-line of all three workstreams: *context is a budgeted, addressable
resource the agent navigates on demand — not a blob you summarize and hope.*

- Within a **turn** (the #321 re-read breadcrumb): "here's a pointer, pull the
  file back."
- Within a **session** (Workstream A/B): a budgeted index in the working set;
  the model fetches detail on demand.
- Across **agents** (this workstream): an **outer agent holds a budgeted plan
  and hands each child a *curated* sub-context** — the relevant slice plus that
  child's one task — **not the parent's whole history.** The children run as
  curated clones, scheduled across an adaptive pool of inference backends.

This is, almost exactly, the Workflow harness this project is *already built
with*: a planner agent decomposes work, spawns sub-agents with scoped context,
and aggregates their results. Workstream C is the bet that we can bring that
pattern **in-tree** and make it **backend-adaptive** — so it runs against one
local Ollama today and a DGX array (or any reachable pool, including cloud and
mesh peers) tomorrow, with the *same* scheduler code.

The adaptive claim is the load-bearing one, so state it plainly:

> **0 backends → refuse with an actionable error. 1 backend → time-slice /
> queue the subtasks. N backends → fan out in parallel.** The plan is
> backend-count-agnostic; only the scheduler's dispatch strategy changes, and it
> changes from config, not from a code fork.

---

## 2. What already exists (reuse, don't rebuild)

The exploration that produced this note found that **the primitives are all
present.** The gap is the connective harness, not the foundations. Each reuse
below is cited at the file:symbol the harness will consume.

### 2.1 Mesh inference delegation — one newt asks another

- `newt-mesh/src/ask.rs` — **`MeshAsker::ask(peer_fp, InferenceRequest,
  timeout)`** is exactly "send a peer a request, get an `InferenceReply` back."
  It is a single request/reply; the harness parallelizes by issuing N of them
  and `join_all`-ing the futures (the asker uses an ephemeral port and is fully
  `Send`, so unlike `TurnDriver`'s `!Send` turn future, mesh asks ride
  `tokio::spawn` / `join_all` directly).
- `newt-mesh/src/service.rs` — **`NewtMeshService::bind(user, agent, backend,
  port)`** is the responder side: a peer that *has* a backend registers a
  handler on the inference topic. A mesh-resident DGX node or a peer laptop is a
  `NewtMeshService`; the harness is a `MeshAsker` (or holds one per pool slot).
- `newt-mesh/src/protocol.rs` — **`InferenceRequest { prompt, tier, model,
  max_tokens }`** / **`InferenceReply { content, model_id, usage, error }`** on
  topic **`newt/inference/v1`** (`INFERENCE_TOPIC`). Note the request *already*
  carries a `tier` hint and an optional `model` pin, and the reply *already*
  carries a mandatory `model_id` and an inline `error` (so "peer reachable but
  backend declined" is distinguishable from "peer unreachable"). That is the
  per-subtask dispatch envelope the scheduler needs, almost unchanged.

### 2.2 Direct-dial for WAN + mDNS discovery for LAN (agent-mesh)

- mDNS / LAN discovery: `agent-mesh-transport`'s `PeerResolver` (bridging
  `agent-mesh-discovery`'s `Browser` → `PeerInfo`) resolves a peer fingerprint
  to a live address on the local network — zero-config for a home lab.
- WAN / explicit dial: `agent-mesh-transport`'s **`Endpoint::dial(peer_pubkey,
  addrs)`** dials a peer by `(iroh PublicKey, SocketAddr…)` directly — the
  direct-dial-by-`(agent_pubkey, SocketAddr)` path (#29, the "`PeerEndpoint`"
  concept) that lets a backend that is *not* on the LAN (a remote DGX, a cloud
  box) still be a first-class pool member. The harness does not need to choose
  between them: a pool slot is "resolve by mDNS" *or* "I was handed a
  `(pubkey, addr)`," and both end at a dialable connection.

### 2.3 DGX formations + the task→tier→backend hinge

- `newt-core/src/dgx.rs` — **`DgxConfig` / `DgxNode` / `DgxFormation` /
  `EndpointKind` / `resolve_endpoint`**. A `DgxNode` already names up to four
  endpoint flavors (`ollama`, `ollama_lb`, `in_cluster`, `vllm`); a
  `DgxFormation` is a saved `(model, endpoint)` preset; `resolve_endpoint`
  resolves the active flavor's URL with env-override precedence and *no leaky
  defaults*. **This is the backend-description model the pool reuses wholesale**
  — a DGX-flavored pool entry *is* a `(DgxNode, EndpointKind, model)` triple.
  The `[backend_pool]` config (§5) is the natural extension: today `[dgx]`
  describes one active node; the pool generalizes to a *list* of reachable
  things, of which DGX nodes are one kind.
- `newt-core/src/router/mod.rs` — **`Router::classify_detailed(prompt) →
  Classification { tier, confidence, reasons }`** (and the thin
  `classify → Tier`). This is the **task → tier** hinge. The scheduler runs it
  per subtask to get a `Tier`, then the pool answers **tier → which backend +
  model** (a backend advertises which tiers it supports; the formation picks the
  model). The reply's mandatory `model_id` closes the loop for attribution.

### 2.4 Driving N concurrent children from one loop

- `newt-core/src/agentic/driver.rs` — **`TurnDriver` / `TurnDriverConfig`**
  (#308). The driver is *non-blocking by construction*: `submit` → `poll` →
  `cancel`, each turn running headless on its own OS thread with a current-thread
  runtime (because `chat_complete`'s `ChatCtx` is `!Send`). **The harness owns N
  `TurnDriver`s — one per in-flight child — and pumps all of them from a single
  scheduler loop**, exactly as a `ratatui` app pumps one. Critically,
  `TurnDriverConfig` already carries `url`, `model`, `kind`, `workspace`, **and
  `caveats`** — so "this child runs against *that* backend under *these*
  narrowed caveats with *this* curated transcript" is expressible today via
  `TurnDriver::with_transcript(config, curated_messages)`. The driver is the
  child-execution engine; the scheduler is the thing that owns the fleet of them.

### 2.5 Curated, cert-anchored authority per child

- `newt-identity/src/lib.rs` — **`UserKey` → `session_root(user)` →
  `attenuate(parent, caveats)` → `enforced_caveats(key)`**, wrapping
  `agent_mesh_protocol::AgentKey::issue` / `delegate`. The parent mints a child
  key whose authority is *signed into its cert* and provably `⊑` the parent's;
  `delegate` **refuses to amplify** and `CertChain::verify` re-checks attenuation
  at every link. So a child can only ever *narrow* the authority it was handed,
  never widen it — cryptographically, not by convention.
- `newt-core/src/caveats.rs` — the **`Caveats` / `Scope` / `CountBound`** lattice
  (re-exported from `agent-mesh-protocol`) plus the dispatch-site `permits_*`
  adaptors and `permits_one_more` budget check. This is the *shape* of a child's
  curated authority: `fs_read`/`fs_write`/`exec`/`net` scopes + a `max_calls`
  ceiling.
- **The precedent already in-tree:** `newt-acp-worker/src/identity.rs`'s
  **`WorkerIdentity::caveats_for_dispatch(backend_host)`** (#94) *already mints a
  fresh attenuated, signed, verified `AgentKey` per dispatch* — narrowing `net`
  to exactly the backend host and `max_calls` to `WORKER_TURN_CALL_BUDGET = 32`.
  The swarm scheduler does the *same move per child*, just with a per-subtask
  caveat policy instead of one fixed worker policy. We are not inventing
  per-dispatch minting; we are generalizing a pattern the headless worker already
  ships.

### 2.6 The existing headless dispatch surface

- `newt-acp-worker/src/server.rs` — the ACP server (`AcpServer`, `Session`,
  `TaskReply`) is the *existing* headless "dispatch a coding goal to a worker"
  surface, and it already attenuates per turn and produces an attributable
  `model_id` + captured diff. A swarm child is conceptually one ACP `prompt`
  turn with a curated transcript and a narrowed key. The outer-agent entry point
  (§6) is a sibling of this server, not a replacement for it.

---

## 3. What's missing — the net-new harness

Four pieces, all in the orchestration layer. Nothing below requires touching
inference, the mesh wire protocol, or the identity crate's internals — they
*compose* the primitives in §2.

### 3.1 `BackendPool` — the adaptive set of 0..N backends

A registry of reachable inference backends and the policy for choosing among
them. A backend entry is a tagged union over the existing transports:

```text
enum PoolBackend {
    LocalOllama  { url, model },                  // newt-core::dgx::resolve_endpoint or NEWT_*
    DgxNode      { node: DgxNode, endpoint: EndpointKind, model },  // reuse dgx.rs
    MeshPeer     { peer_fp, dial: MeshDial, model },               // MeshAsker + PeerInfo/Endpoint::dial
}
```

Each entry tracks: **health** (reachable? last probe?), **tier support** (which
`Tier`s this backend will serve — a `qwen2.5-coder:32b` box serves `Standard`/
`Complex`; a tiny model serves `Fast`), and **reachability** (LAN via mDNS, WAN
via direct-dial, or in-process for the local case). The pool's one job is to
answer: *given a `Tier` (and optional model pin), which live backend should run
this subtask?* — and to report `len()` so the scheduler can pick its strategy.

The pool is **count-adaptive but strategy-agnostic**:

- `len() == 0` → `Err` (actionable, like `DgxNotConfigured`: "no backends — set
  `[backend_pool]` or `NEWT_DGX_*`").
- `len() == 1` → every subtask routes to the one backend; concurrency is
  bounded to that backend's capacity (time-slice / queue).
- `len() >= 2` → subtasks fan out; the pool load-balances by tier-fit + health +
  (later) cost/latency.

### 3.2 `WorkflowScheduler` — decompose, assign, dispatch, aggregate

The engine that turns a `Plan` into results. Its loop:

1. **Topologically order** the plan's subtasks by their dependency edges; find
   the ready set (no unfinished deps).
2. For each ready subtask, **assign a worker**: classify with
   `Router::classify_detailed` → `Tier`; ask the `BackendPool` for a backend +
   model for that tier; mint a **narrowed child `AgentKey`** (§4); compose the
   **curated sub-context** (§4); build either a `TurnDriverConfig` (local /
   in-process backend) or a `MeshAsker` request (mesh peer).
3. **Dispatch up to the concurrency budget.** With one backend the budget is 1-ish
   (time-sliced); with N backends it is min(ready, N, configured cap). In-process
   children run on `TurnDriver`s pumped from this loop; mesh children are
   `join_all`-ed asks.
4. **Poll / await**, collect each child's result, mark its node done, unlock
   dependents, repeat until the plan is drained.
5. **Aggregate** per the plan's aggregation strategy (§3.3) and hand the result
   back to the outer agent.

The scheduler never blocks on a single child: local children are `poll()`ed,
mesh children are awaited concurrently. This is the same one-loop-drives-many
shape `TurnDriver` was built for (#308).

### 3.3 `Plan` — the budgeted plan model

A serializable plan the outer agent holds. Sketch:

```text
struct Plan {
    goal: String,
    subtasks: Vec<Subtask>,
}

struct Subtask {
    id: SubtaskId,
    instruction: String,             // the child's one task
    deps: Vec<SubtaskId>,            // edges; the DAG the scheduler orders
    parallel_ok: bool,              // may this run concurrently with siblings?
    context: CuratedContext,        // the slice the parent discloses (§4)
    caveat_policy: CaveatPolicy,    // how to narrow the child key (§4)
    tier_hint: Option<Tier>,        // overrides the classifier when set
}

enum Aggregation { Concat, LastWins, Reduce(reducer), Custom(name) }
```

The plan is a **DAG, not just a list**: deps express ordering, `parallel_ok`
expresses which siblings are safe to run at once. Where the plan *comes from* is
an open question (§10) — human-authored TOML/JSON, or LLM-generated by the outer
agent in a planning turn, or both.

### 3.4 Outer-agent entry point

A CLI subcommand (`newt swarm run <plan>` or similar) and/or an ACP mode that
binds the scheduler to the same headless surface as `newt-acp-worker`. The outer
agent: loads/derives the plan, holds the `UserKey`-rooted `session_root` (§7),
constructs the `BackendPool` from config, runs the `WorkflowScheduler`, and
prints/returns the aggregated result. This is the only piece a human or a parent
process drives directly.

---

## 4. The curated-context point (the disclosure tie-in)

This is the heart of the workstream and the reason it belongs in the same plan
as A and B. **Each child gets a sub-context the parent composes — the relevant
slice plus that child's task — NOT the parent's whole conversation history.**

Concretely, `CuratedContext` is assembled by the outer agent from exactly the
material the subtask needs:

- the subtask's own instruction,
- the specific files / notes / prior-subtask outputs it depends on (pulled, not
  dumped — this is Workstream A's budgeted-index "fetch on demand" applied at the
  orchestration layer, and Workstream B's `[SYMBOLS]` index is the ideal
  compact form of "the relevant code slice"),
- nothing else. Not the parent's planning deliberation, not sibling subtasks'
  internals, not the full transcript.

It lands in the child via `TurnDriver::with_transcript(config, curated_messages)`
for local children, or as the `prompt` string of an `InferenceRequest` for mesh
children. The child is a **clone of the agent airframe with a deliberately
small, deliberately scoped window** — which is *why* it can run on a cheaper
tier/backend and *why* its results compose cleanly: a child that never saw the
parent's whole history cannot smuggle the parent's assumptions into its output.

Curation is a budget decision *and* a security decision: the context you don't
disclose is context the child cannot leak, misuse, or hallucinate over.

---

## 5. Config shape

A new optional `[backend_pool]` sub-table of `newt.toml`, modeled directly on
`[dgx]`'s "no leaky defaults, env overrides win" discipline (`DgxConfig`):

```toml
[backend_pool]
# Dispatch strategy is derived from the live backend count, but the ceiling
# and the selection policy are configurable.
max_concurrent = 4          # hard cap on in-flight children (independent of N)
selection      = "tier_fit" # tier_fit | round_robin | cost_aware (later)

# Local in-process backend (the N=1 spike target).
[[backend_pool.backend]]
kind  = "local_ollama"
url   = "http://127.0.0.1:11434"
model = "qwen2.5-coder:14b"
tiers = ["FAST", "STANDARD"]

# A DGX node reused straight from the [dgx] model.
[[backend_pool.backend]]
kind     = "dgx"
node     = "home"           # references a [dgx] node by name
endpoint = "ollama"
model    = "qwen2.5-coder:32b"
tiers    = ["STANDARD", "COMPLEX", "REVIEW"]

# A mesh peer reached by fingerprint (mDNS) or explicit dial (WAN).
[[backend_pool.backend]]
kind     = "mesh_peer"
peer_fp  = "ab12…"          # resolve via mDNS PeerResolver…
# addr   = "203.0.113.5:51820"  # …or direct-dial (pubkey,addr) for WAN (#29)
model    = "llama3.1:70b"
tiers    = ["COMPLEX", "REVIEW"]
```

`[dgx]` stays as-is (single active node for the `newt dgx` suite);
`[backend_pool]` is the multi-backend generalization the swarm consumes. When
`[backend_pool]` is absent, the harness falls back to a single-backend pool
synthesized from the resolved DGX/Ollama endpoint — so the N=1 case needs zero
new config.

---

## 6. Crate placement

**Proposed: a new `newt-scheduler` crate** in the default workspace, depending on
`newt-core` (for `dgx`, `router`, `agentic::driver`, `caveats`) and
`newt-identity` (for `session_root` / `attenuate`). Rationale:

- The scheduler is a *consumer* of `newt-core`, not part of its core data model;
  a separate crate keeps `newt-core`'s already-large surface from growing a
  whole orchestration subsystem.
- **`newt-mesh` is excluded from the default workspace** (it path-depends on
  `../agent-mesh`; see [`mesh_integration.md`](../decisions/mesh_integration.md)
  — cargo validates path deps eagerly even behind a feature, so default CI can't
  carry it). Therefore the mesh fan-out must sit behind a feature flag. The clean
  shape: `newt-scheduler` defines a `RemoteDispatch` trait; the **in-process /
  local** path (`TurnDriver`, local Ollama, DGX-over-HTTP) is the default build
  and is fully testable in default CI; the **mesh** implementation
  (`MeshAsker`-backed) lives behind a `mesh` feature that pulls `newt-mesh`, the
  same way `newt-mesh` itself is gated. The N=1 spike and the deterministic test
  (§9) live entirely in the default build.
- Identity stays in `newt-identity` (in-workspace, reaches the published
  `agent-mesh-protocol`) — the scheduler mints children through it without
  touching `newt-mesh`.

Alternative considered: extend `newt-core` with a `scheduler` module. Rejected —
it would entangle the mesh-feature flag with `newt-core`'s dependency graph,
which every other crate depends on.

---

## 7. Caveats / security — per-subtask child-key minting (will get adversarial review)

This surface mints authority dynamically, so it gets adversarial review when
built (per the plan's verification policy). Spell out the trust model and the
risks now.

### Trust model

- **The outer agent holds the root of trust.** Either the operator's `UserKey`
  (and mints `session_root` itself) or a delegable root `AgentKey` handed to it.
  Children **never** hold the `UserKey` or anything wider than what they were
  handed.
- **Children are attenuation-only.** For each subtask, the scheduler calls
  `attenuate(parent_key, child_caveats)` (= `parent.delegate(...)`), producing a
  signed child `AgentKey` whose authority is provably `⊑` the parent's.
  `delegate` refuses amplification structurally; `enforced_caveats` verifies the
  chain before the child runs (*attenuate, never amplify*,
  `steward-charter/docs/AUTHORITY.md`). A child given `fs_write = Only(["src/foo.rs"])`,
  `exec = none`, `net = Only([backend_host])`, `max_calls = AtMost(k)` cannot
  escalate to anything wider — the same property `WorkerIdentity` already relies
  on.
- **Verification at the handshake.** For a *mesh* child the narrowed key flows in
  the cert chain the bus already signs and `CertChain::verify`s on connect, so a
  peer can independently confirm "this request runs under an authority that
  chains to the operator's UserKey." For a *local* child, `enforced_caveats` is
  re-verified before the `TurnDriver` turn; the caveats are what the dispatch
  sites' `permits_*` adaptors enforce on every tool call.

### The precise gap vs. today

Today's certs are **fixed at issue time** — `WorkerIdentity` mints *one*
per-dispatch policy. The swarm needs the parent to **mint a *fresh* narrowed
`AgentKey` per child at dispatch**, with a *per-subtask* caveat policy derived
from the plan (a "write only these files" child differs from a "read-only
review" child). The machinery exists (`attenuate` is already per-call); what's
new is (a) the scheduler computing a distinct `CaveatPolicy` per `Subtask` and
(b) threading the resulting child key into both the local (`TurnDriverConfig.
caveats`) and mesh (cert chain on the asker) dispatch paths.

### Risks to review

- **Over-broad child caveats.** The default must be *deny*, narrowing up from the
  subtask's declared needs — not `Caveats::top()` minus a few axes. A bug here
  hands a child more authority than its task warrants. (Mirror the `no_top_leak`
  regression test: the swarm dispatch tree should carry zero literal
  `Caveats::top()`.)
- **Confused-deputy via curated context.** If the curated context for child A
  includes secrets only the parent should see, the child can exfiltrate them
  even with narrow *tool* caveats. Curation (§4) is itself a security control;
  the context budget is part of the threat model.
- **Untrusted plan → authority.** If the plan is LLM-generated (§10), an injected
  instruction could request a wide `caveat_policy`. The parent must clamp every
  child policy to a ceiling it sets, regardless of what the plan asks for — the
  plan *requests*, the parent *grants*, and `delegate` enforces `⊑`.
- **Mesh peer trust boundary.** A mesh child runs on someone else's hardware. The
  cert proves *authority*, not *honesty* — a malicious peer can return garbage or
  leak the prompt. Result aggregation (§10) must treat mesh results as untrusted
  input, and curated context must assume the peer sees everything it's handed.
- **Key/cert lifetime.** Per-child keys are short-lived; ensure they're not
  persisted or logged, and that `expires_at` is set tight enough that a captured
  child cert can't be replayed after the plan completes.

---

## 8. Adaptive scheduling — the 0 / 1 / N model

The same scheduler code, the same `Plan`, three behaviors selected by the live
backend count. This is the design principle from day one, even though the spike
runs only at N=1.

- **0 backends:** refuse. An actionable error in the spirit of
  `DgxNotConfigured::NoNodes` — name what's missing and how to set it. No leaky
  default, no silent local fallback that surprises the operator.
- **1 backend (dgx1 today):** **time-slice / queue.** All subtasks route to the
  one backend. The DAG still decides *order* (deps), but ready siblings run
  sequentially (or up to that backend's small concurrency capacity) rather than
  fanning out. The scheduler pumps one (or few) `TurnDriver`(s) at a time; the
  plan completes correctly, just without parallel speedup. **This is the spike
  target** — it proves the Plan → curate → mint → dispatch → aggregate loop end
  to end with the cheapest possible infra.
- **N backends (dgx1–4 aspired; conceptually any pool):** **parallel fan-out.**
  Ready, `parallel_ok` siblings dispatch concurrently across backends, bounded by
  `max_concurrent` and per-backend tier-fit. Local children pump on their own
  `TurnDriver`s; mesh children `join_all`. Speedup is bounded by the DAG's
  critical path, not the backend count.

**Why not limit the pool to DGX?** A "backend" is anything reachable that can
serve an `InferenceRequest`: a local Ollama, a DGX node (any of its four endpoint
flavors), a **mesh peer** (a teammate's laptop or a remote box reached by mDNS or
direct-dial), or — trivially, since the request is just `(prompt, tier, model)` —
a cloud endpoint behind an OpenAI-compatible shim. The DGX array is the
*motivating* N>1 case because it's the operator's hardware, but the abstraction
is "pool of backends," and the config (§5) and the scheduler treat a DGX node and
a mesh peer as the same kind of thing: a tier-tagged, health-checked, dialable
inference source.

The key honesty: **N=1 and N=4 differ only in the pool's `len()` and the
dispatch fan-out; the plan, the curation, the per-child minting, and the
aggregation are identical.** That is what makes the single-backend spike a true
proof of the design and not a throwaway.

---

## 9. Phasing — and acceptance per phase

Three phases, smallest-first, each independently mergeable.

### Phase C0 — this design note

**Deliverable:** this file. **Acceptance:** docs-only PR, `just check` green
(fmt/clippy/test on the unchanged tree), merge-on-green. No code.

### Phase C1 — single-backend (N=1) scheduler spike

The smallest thing that proves the model:

1. `Plan` model (2–3 subtasks, a dependency edge, per-child `CuratedContext` and
   `CaveatPolicy`).
2. A single-entry `BackendPool` (the local backend; `len() == 1` → time-sliced).
3. The `WorkflowScheduler` loop: order the DAG → for each subtask classify
   (`Router::classify_detailed`) → mint a narrowed child key (`attenuate`) →
   compose curated context → dispatch via `TurnDriver` (`with_transcript` +
   per-child `caveats`) → collect → aggregate.
4. A deterministic test against a **mock backend** (the established `MockBackend`
   / `wiremock` pattern already used by `TurnDriver` and `NewtMeshService` tests):
   a known 2–3-subtask plan runs to completion, each child runs under its
   *narrowed* caveats (assert the child key's `enforced_caveats` are strictly
   `⊑` the parent's and match the per-subtask policy), and the aggregated result
   is exact.
5. A **dgx1 live smoke** (gated/manual, like the existing `newt dgx` live
   smokes): the same plan runs against one real DGX endpoint and produces a real
   aggregated result.

**Acceptance:** deterministic test green; child caveat-attenuation asserted (the
adversarial-review surface gets its first regression guard); `just check` +
`cov-ci` green at the ≥80% floor; dgx1 smoke documented. Mesh fan-out is *out of
scope* for C1.

### Phase C2 — `BackendPool` + mesh fan-out (as the DGX array materializes)

Grow the pool to N>1 and add the `mesh` feature path:

1. Multi-entry `BackendPool` with health probing and tier-fit selection;
   `len() >= 2` enables fan-out.
2. The `RemoteDispatch` mesh implementation behind the `mesh` feature
   (`MeshAsker` → `InferenceRequest`/`InferenceReply`), with per-child cert
   chains carrying the narrowed key, verified at handshake.
3. Direct-dial pool entries (`Endpoint::dial(pubkey, addr)`) for WAN backends.

**Acceptance:** an N≥2 plan fans out (assert ≥2 concurrent in-flight children and
correct aggregation under reordered completion); mesh children verified to run
under attenuated keys; the `mesh` feature builds and tests outside default CI the
same way `newt-mesh` does; default-workspace CI stays green with the feature off.

---

## 10. Open questions / risks

- **Session relay over the mesh (multi-turn).** `InferenceRequest` is
  *single-turn* today (multi-turn is "encode history into the prompt"). A swarm
  child that needs a few back-and-forth turns on a *remote* backend has no
  session affordance — there's no mesh equivalent of a `TurnDriver`'s persisted
  transcript. C1/C2 sidestep this (children are one-shot or local), but a real
  remote multi-turn child needs a session-relay protocol that does not exist yet.
- **Backend health / failover.** What happens when a pool member goes dark
  mid-plan? Re-dispatch the subtask elsewhere? Fail the plan? Health probing,
  retry/backoff, and "drain a dead backend's in-flight subtasks" are unspecified
  beyond "the pool tracks health."
- **Result aggregation strategies.** `Concat` is trivial; `Reduce`/`Custom`
  (e.g. "have a `Review`-tier child judge the others", map-reduce, voting) is
  where the value is and where the design is thinnest. Also: how are *partial*
  failures aggregated (one child errored, the rest succeeded)?
- **Cost / latency-aware selection.** §5's `selection = "cost_aware"` is a
  placeholder. Choosing the cheapest backend that meets the tier, or the fastest
  for the critical path, needs a cost/latency model the pool doesn't have yet.
- **Where the plan comes from.** Human-authored (TOML/JSON, deterministic,
  reviewable) vs. LLM-generated by the outer agent (flexible, but an
  authority/injection risk — see §7). Likely both, but the LLM-generated path
  raises the security bar on per-child caveat clamping.
- **The `newt-mesh`-excluded-from-default-workspace constraint.** The whole mesh
  fan-out lives behind a feature that pulls a crate default CI can't compile. We
  must keep the local path a first-class, fully-tested default build (the
  `RemoteDispatch` trait seam in §6 is how), or the mesh phase rots silently
  outside CI. This is a known, accepted cost of the architecture, not a thing to
  rediscover later.
- **Backpressure & fairness.** With one backend and a wide ready set, what
  ordering is fair / optimal? FIFO? Critical-path-first? Tier-priority? Out of
  scope for the note, but it's a real scheduling question the moment N=1 has more
  ready subtasks than capacity.

---

## 11. Summary

| Layer | Status | Where |
|---|---|---|
| One newt asks another for inference | **exists** | `newt-mesh` `MeshAsker::ask` / `NewtMeshService::bind` / `InferenceRequest`/`Reply` |
| LAN discovery + WAN direct-dial | **exists** | agent-mesh `PeerResolver` (mDNS) + `Endpoint::dial(pubkey, addr)` (#29) |
| Backend description + task→tier hinge | **exists** | `newt-core::dgx` (`DgxConfig`/`resolve_endpoint`) + `router::classify_detailed` |
| Drive N concurrent children, one loop | **exists** | `newt-core::agentic::driver` `TurnDriver` (#308) |
| Per-child curated authority, cert-anchored | **exists** | `newt-identity` `attenuate`/`enforced_caveats`; precedent: `WorkerIdentity::caveats_for_dispatch` (#94) |
| `BackendPool` (0..N, adaptive) | **build** | new `newt-scheduler` |
| `WorkflowScheduler` (decompose/assign/dispatch/aggregate) | **build** | new `newt-scheduler` |
| `Plan` model (DAG, curated context, caveat policy) | **build** | new `newt-scheduler` |
| Outer-agent entry point (CLI/ACP) | **build** | `newt-cli` / ACP sibling of `newt-acp-worker` |

The foundations are done. Workstream C is the orchestration layer that composes
them — a `BackendPool` that adapts from one backend to many without a code fork,
a `WorkflowScheduler` that hands each child a curated slice and a freshly-minted
narrowed key, and a `Plan` that is a DAG, not a list. The first deliverable after
this note is the N=1 spike: small, deterministic, and a true proof of the whole
loop on the cheapest infra we have.

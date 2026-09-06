# Feature Proposal: Module Scopes (principal + context for a running agent)

> **Status:** Draft — proposal, not normative · **Owner:** hartsock · **Last review:** 2026-08-16 · **Builds on:** [ocap_confinement_model.md](../decisions/ocap_confinement_model.md), [agentic_object_capability_security.md](../decisions/agentic_object_capability_security.md), [agent_bridle_publishing.md](../decisions/agent_bridle_publishing.md), [1528b3-cid-spill-identity.md](../decisions/1528b3-cid-spill-identity.md), `newt-identity`, `newt_core::caveats`, `agent-bridle-core` (`Registry`, `Gate`, `Tool::required`, `ToolContext`) · **Supersedes/Superseded by:** —

Tracking: [#1737](https://github.com/Gilamonster-Foundation/newt-agent/issues/1737) (A3 — Kit = package / provenance; Module = principal + Grant,
under the [#1734](https://github.com/Gilamonster-Foundation/newt-agent/issues/1734) companion train);
attenuation lineage [#739](https://github.com/Gilamonster-Foundation/newt-agent/issues/739).
Index: [companion-roadmap.md](companion-roadmap.md). Sibling proposal: [kit-system.md](kit-system.md)
(what a **Kit** is — package / interface / provenance; this doc is about *who runs it and under what
grant*).

## One-paragraph summary

A **Module** is the runtime *context* an agent instance executes in. It answers **who is running**
(a `newt-identity` `AgentKey`; its `PrincipalId` is the key's id), **with what authority** (the
granted `Caveats` the key's certificate chain carries — the *Grant* — evaluated only by Agent
Bridle's `Gate`), **with which code** (a scoped view
of Kit exports plus the kit handles it has minted), **with how much** (a resource budget), **through
what mailbox**, and **in which lifecycle state**. Modules nest; a child's authority is always
`parent ⊓ requested ⊓ host_clamp`. Modules do **not** introduce a permission system of their own, and an
in-process module is **not** an isolation boundary — it is logical scoping plus accounting. Hard
isolation begins at WASM / subprocess / container, and is the execution layer's job, not the module's.

## Motivation

Multi-agent runs (crews, sub-agents, mesh peers, the Gilamonster matrix) put many agent instances in
one process or across processes. Today the per-instance boundary exists only piecemeal:

| Concern | Existing owner today | Gap |
|---|---|---|
| Role / kit selection (discovery, exposure) | `RoleProfile` (`newt-core/src/role_profile.rs`), `Loadout.kit` (`newt-core/src/config/loadout.rs`) → `[bundles.*]` (`newt-core/src/config/profile.rs`), `newt_core::kit` (`Axis`, `Tier`, `RegistryEntry`), `ExposureProfile` / `ExposureClass` (`newt-core/src/config/tool_exposure.rs`, `agentic/tools/exposure.rs`) | Chosen per session, not per nested principal |
| Authority | **Agent Bridle only**: `Caveats` (`newt-core/src/caveats.rs`, from `agent-mesh-protocol`) → `Gate::authorize` → `ToolContext` (`check_*` leashes). `PermissionGate` (`newt-core/src/agentic/permissions.rs`) and `OperatingModeControl` (`agentic/operating_mode.rs`) sit **in front of** the Gate — they decide which `Caveats` get minted; they do not evaluate calls | Attenuation for dispatched crews is done ad hoc: `CrewRunner` runs crews under `caveats.meet(crew_clamp())` (`newt-cli/src/crew_runner.rs`, #739) — correct, but not a first-class child relation |
| Identity | `newt-identity`: `UserKey` (root of trust, `~/.newt/identity.pem`) → `session_root(&UserKey) -> AgentKey` (issued at `Caveats::top()`) → `attenuate(&parent, &caveats) -> Result<AgentKey>` (signed, provably ⊑ parent; a wider request fails with `MeshError::CaveatAmplification`) → `enforced_caveats(&key) -> Caveats` (re-verifies the cert chain) → `delegate_for_plugin` / `plugin_child_metadata(role, caveats)` for subprocess plugins. `AgentKey` / `AgentMetadata` / `UserKey` are re-exported from agent-mesh-protocol. (`newt-core/src/agent_identity.rs` `AgentIdentity` is a *different* thing — the git / GitHub-App identity used for commits — and is not principal identity.) | The chain exists; it is not bound to a runtime context object |
| Budgets (resources) | `CrewBudgets` (`newt-core/src/config/crew.rs`: attempts / files / lines), `TokenBudget` (`newt-core/src/memory.rs`), Bridle `Gate` call budget (`with_budget`, grant `max_calls`); `send_budget.rs` is the *per-request context-window input-token* ceiling, not a quota | No per-principal quota for inference spend / memory / concurrency; must not be conflated with authority |
| Accounting | `newt_core::metrics::TokenUsage`, `newt-core/src/pricing.rs` | Not keyed by principal, no parent-ward roll-up |
| Lifecycle / mailbox | `TabSidecar` (`newt-tui/src/tabs.rs`), `CrewRunner`, `SessionRegistry` / `SteeringInbox` | Implicit; no spawn/ready/shutdown contract, no typed mailbox |

Module Scopes tie these into one runtime primitive: **agent instance = module**.

## What a Module is

```
Module
├── key: AgentKey       who        — newt-identity AgentKey; PrincipalId = key.fingerprint(); name is a label
├── Grant               may I?     — enforced_caveats(&key): the granted Caveats the cert chain carries; evaluated by Bridle Gate
├── DomainGrants        may I? (2) — host-held per-InterfaceId DomainCaveats beside the Grant (kit-system.md §3); never in the cert chain
├── Gate                mint site  — this module's Bridle Gate: with_budget(host_generation, grant.max_calls) + host sandbox/floor
├── ScopedKitView       may see    — exports selected by loadout data, annotated with grant ⊓ required
├── KitInstances        holds      — kit handles minted through the Gate (ToolContext / Session handles)
├── ResourceBudget      how much   — inference tokens/spend, memory, concurrency (NOT authority, NOT tool-call count)
├── Mailbox             talk to    — typed, bounded, in-process or mesh-proxied
└── LifecycleState      when       — Spawned → Ready → Draining → Stopped | Failed
```

**Crate placement.** `Module`, `ScopedKitView`, `ResourceBudget` and `Mailbox` live in **`newt-core`**
(`newt_core::module`), beside every reuse target below (`caveats`, `SteeringInbox`,
`SessionRegistry` / `OutputSink`, `DockRegistry`, `metrics::TokenUsage`, the widened `newt_core::kit`
catalog). `newt-identity` depends on `newt-core` (`newt-identity/Cargo.toml`), so newt-core cannot call
`newt_identity::attenuate` / `enforced_caveats` by name — but both are one-line wrappers over
agent-mesh-protocol calls newt-core already has (`AgentKey::delegate(meta)` and
`key.cert().verify()` + clone; newt-core depends on `agent-mesh-protocol` directly). The first Module
PR therefore **lifts those two functions down into `newt-core`** and has `newt-identity` re-export
them — one implementation, no fork — while `UserKey` loading, `session_root` and
`delegate_for_plugin` stay in `newt-identity` (they touch `~/.newt` and the plugin envelope). Every
invariant below is still spelled with the `newt_identity::` names because that is the contract; the
symbol just moves one crate down.

### The five axes are separate

Folding token budgets, filesystem roots and network allowlists into one `ModulePermissions` bag
would be exactly the vocabulary sprawl the reuse doctrine forbids. Each axis has one owner and one
question:

| Axis | Question | Owner | Lattice / shape |
|---|---|---|---|
| **Authority** | *May I?* | Agent Bridle: granted `Caveats` → `Gate::authorize(tool, granted)` on **the module's Gate** → `effective = granted.meet(tool.required())` → `ToolContext::check_*`. Includes the **tool-call count** (`Caveats::max_calls`, seeded into the module's `Gate::with_budget`). Domain axes (`SpeechCaveats`, `PaneCaveats`, … — kit-system.md §3) ride the host-held `DomainGrants` carrier and are met by the minting host, never by a second evaluator | Meet-semilattice (`top`, `leq`, `meet`); only ever narrows |
| **Resources** | *How much?* | `ResourceBudget`: inference tokens / spend, memory, concurrent tasks — **never** tool-call count | Quotas; exhaustion is a lifecycle event, never an authorization decision |
| **Scheduling** | *When?* | Host scheduler / mailbox back-pressure | Priorities, fairness, **cancel epochs** (a per-producer counter — speech-pipeline.md; deliberately not called "generation", which is Bridle's authority-revocation counter `Gate::generation` / `Caveats::valid_for_generation`) |
| **Accounting** | *What did I consume?* | Usage ledger keyed by principal (`TokenUsage`, `pricing.rs` lineage) | Append-only counters, rolled up parent-ward |
| **Provenance** | *Which principal, running which artifact?* | `newt-identity` cert chain + Kit artifact CID, recorded as `ProvenanceRecord` (kit-system.md) | "principal P received authority X while executing artifact CID Y" |

Where the line falls between the first two axes, concretely:

| Quantity | Axis | Where enforced | On exhaustion |
|---|---|---|---|
| Tool calls (`max_calls`, `CountBound::AtMost(n)`) | Authority | Bridle `Gate::authorize` (`charge_one`) on the **module's own Gate**, `Gate::with_budget(host_generation, grant.max_calls)` — the persistent counter is a property of the Gate *instance* in agent-bridle-core 0.7.15, so a per-module limit needs a per-module Gate (one shared host Gate would let every module draw down one counter, and `Registry::dispatch` mints a fresh Gate per call, so `AtMost(n>0)` would never exhaust) | `ToolError::Budget` — an authorize-time **denial** |
| Inference tokens / spend, memory, concurrent tasks | Resources | `ResourceBudget` (module runtime, after the Gate) | `Ready → Draining` — a **lifecycle** event |

A token budget is not a permission. Running out of tokens puts the module in `Draining`; it does not
change what the module *may* do, and being granted more budget never widens `Caveats`. Conversely the
call count lives in the Grant (`max_calls`), is seeded into the module's Gate at spawn, and is charged
by that Gate at authorize time; the module runtime must not charge calls a second time.

### Authority: no second evaluator

There is exactly one authorization plane: **Bridle**. A module holds a Grant (the host-minted granted
`Caveats`) and every kit/tool invocation terminates in one of Bridle's mint sites on the module's
`Gate` (built by the host at spawn: `Gate::with_budget(host_generation, grant.max_calls)` with the
host's `SandboxKind` and `AxisEnforcement` floor — a Gate is a Bridle object; the module only *holds*
it) — `authorize`, `authorize_with_discharge` (with a `Discharge`), or `authorize_step_up` — which
returns the `ToolContext` the call runs under. Per-operation refusal then happens inside the tool via the
context's `check_exec` / `check_net` / `check_path_read` / `check_path_write` leashes. Kit manifests
declare *required* authority as a **ceiling** (`Tool::required()`, default `Caveats::top()` = "confine
me entirely by the grant"); they never grant anything, and declaring more than the grant holds is not
an error — the meet intersects it away (see kit-system.md). Nothing named `PermissionEvaluator`,
`KitPermissions`, `ModulePermissions`, or a registry-level `call()` that authorizes exists in this
design.

`PermissionGate` prompting and `OperatingModeControl` are **upstream** of the Gate: they influence
which `Caveats` the host mints (a prompted "allow" re-mints a wider Grant at the root via
`widen_caveats`, still `⊑` the user root); they are not a second evaluator and are never consulted by a
module at call time.

### The scoped kit view

`ScopedKitView` is a **view**, not an enforcer, and it does **not** call the Gate. Two facts about Bridle make that mandatory:

- `Gate::authorize` never refuses on authority — `required()` is a ceiling and the meet cannot fail;
  it refuses only on generation or call budget. A "filter by `authorize().is_ok()`" would admit every
  export.
- `Gate::authorize` is an instance method with a side effect: it charges one call. Enumerating a view
  through it would burn `max_calls` per listing.

So visibility is **data**, and effective authority is a **pure lattice annotation**:

```rust
// sketch — illustrative, not compiled
pub struct ScopedKitView<'a> {
    catalog: &'a KitCatalog,                 // kit-system.md: descriptors only, never handles
    selection: &'a KitSelection,             // loadout / RoleProfile / ExposureProfile data
    grant: &'a Caveats,                      // this module's granted Caveats
}

/// A DESCRIPTOR + an annotation. No `dyn Tool`, no handle (kit-system.md acceptance #4).
pub struct VisibleExport<'a> {
    pub export: ExportDescriptor<'a>,        // (manifest CID, ExportId), interface, required, …
    /// `grant ⊓ export.required.bridle` — what a call *would* run under. For the
    /// Bridle `Tool`s `required.bridle` IS `Tool::required()` (kit-system.md
    /// mapping table). Computed with `Caveats::meet` only; no Gate, no charge.
    pub effective: Caveats,
}

impl ScopedKitView<'_> {
    pub fn visible(&self) -> impl Iterator<Item = VisibleExport<'_>> {
        self.catalog.all()
            .filter(|e| self.selection.exposes(e.export.id))                     // data decides visibility
            .map(|e| VisibleExport { effective: self.grant.meet(&e.export.required.bridle), export: e })
    }
}
```

The view iterates kit-system.md `Export`s rather than Bridle's `Registry`: agent-bridle-core 0.7.15
`Registry` has no tool iteration or `required(name)` accessor (only `builder`, `tool_definitions`,
`tool_names`, `contains`, `dispatch*`), and a view that handed out `&dyn Tool` would violate
kit-system.md's "no catalog method returns `dyn Tool`" rule anyway.

The view carries no `allowed_kits` / `denied_kits` lists of its own. Which exports a role sees is a
*loadout* concern (`Loadout.kit`, `[bundles.*]`, `RoleProfile`, `ExposureProfile`) — discovery /
presentation composition, resolved before the view. Nothing in the selection can admit authority the
Gate would not mint: the Gate is consulted, and charges, **only at invocation**, and `effective` shown
in the view is by construction the same `granted.meet(required)` the Gate computes.

**Non-Action exports.** `Gate::authorize` takes `&dyn Tool`, i.e. an `Action`-shaped export. For
`Source` / `Sink` / `Session` / `View` exports (kit-system.md — an STT `Session<AudioFrame,
TranscriptEvent>`, a TTS `Session<SpeechRequest, TtsEvent>`, a `View<PaneModel>`), the mint happens
**once**, when the handle is opened: the `ToolContext` returned by the Gate *is* the capability
handle, and thereafter the module holds it in `KitInstances` and streams through it without
re-authorizing per frame. Whether a long-lived handle
should be re-checked against the Gate's generation on parent revocation is open question 4.

## Principal identity — the `newt-identity` chain *is* the implementation

The principal is a `newt-identity` `AgentKey` — an ed25519 key whose certificate chain carries its
signed `Caveats` and provably `⊑` its parent. Nothing new is invented here; the chain in
`newt-identity/src/lib.rs` is used as-is:

| Concept in this doc | Is exactly |
|---|---|
| **`PrincipalId`** — the one principal identifier used across the companion-train docs (`ActorId` in animated-companion.md is an alias of it) | the `AgentKey`'s id: `AgentKey::fingerprint()` (an agent-mesh-protocol `Fingerprint` over the public key) |
| the root of trust | `UserKey` (`~/.newt/identity.pem`, `load_or_generate`) — never enters a module |
| the session root | `session_root(&UserKey) -> AgentKey`, issued at `Caveats::top()` |
| a Module's key | an `AgentKey` obtained by `attenuate(&parent, &caveats) -> Result<AgentKey>` — signed, provably ⊑ parent; a wider request fails with `MeshError::CaveatAmplification` |
| a Module's **Grant** | `enforced_caveats(&key) -> Caveats` — re-verifies the cert chain and returns the caveats it carries. The module keeps no second copy of authority that could drift from the key |
| a subprocess child's identity | `delegate_for_plugin` / `plugin_child_metadata(role, caveats) -> AgentMetadata`, serialized over `AGENT_KEY_ENV` |

(`newt-core/src/agent_identity.rs` `AgentIdentity` is the git / GitHub-App identity used to sign
commits — a different concern; it is never cited as principal identity.) Consequences:

- **Identity is cryptographic; the name is a label.** A module has a `display_name` for humans and
  logs, and a principal key for everything that matters (Grant binding, provenance, mesh addressing).
  There is no UUID-vs-name question: the key *is* the identity; UUIDs, if any, are local handles.
- **The Grant is bound to the principal** (*attenuate, never amplify*,
  `steward-charter/docs/AUTHORITY.md`). `attenuate` refuses to mint a child key that
  amplifies; `CertChain::verify` re-checks attenuation at every link. A module holding its own key but
  not the root `UserKey` can only narrow.
- **The `UserKey` never enters a module.** The root module runs under the *operating key*
  (`session_root(user)` attenuated by the preset via `attenuate`) — not `session_root` itself, which
  is `⊤` (the human's full authority). The root module's Grant is the operating key's caveats.
- **Provenance is queryable.** Combined with Kit artifact CIDs (kit-system.md), the ledger can answer
  "principal *P* received authority *X* while executing artifact CID *Y*" — the same content-identity
  discipline as [1528b3-cid-spill-identity.md](../decisions/1528b3-cid-spill-identity.md).

**The domain carrier.** The signed chain carries only the six `Caveats` axes. Per-interface
`DomainCaveats` (kit-system.md §3 — `SpeechCaveats { mic, speaker }`, `PaneCaveats`, `GitCaveats`)
are **host-held beside the Grant** in `Module.domain: DomainGrants` (a map `InterfaceId →
DomainCaveats`), never merged into the chain and never signed by `attenuate`. They obey the same
`top`/`leq`/`meet` laws, so at spawn the host attenuates them the same way — `child.domain[i] =
parent.domain[i].meet(&requested.domain[i]).meet(&host_clamp.domain[i])` — and the minting host for
a `Session`/`Source`/`Sink`/`View` export computes `effective_domain = module.domain[i].meet(&export.required.domain)`
once at handle open. The `Caveats` carrier is checked *and signed* by `attenuate`; the domain carrier is
checked by the host's `meet` (property-tested, not signed) — the two are not interchangeable and the
doc says which applies wherever it matters.

## Hierarchy and the child invariant

```
matrix-root (host process; principal = operating key = session_root ⊓ preset; UserKey stays outside)
├── coordinator      grant = root ⊓ role(coordinator) ⊓ host_clamp
│   ├── planner      grant = coordinator ⊓ requested ⊓ host_clamp
│   └── researcher   grant = coordinator ⊓ requested ⊓ host_clamp
├── coder            grant = root ⊓ role(coder) ⊓ host_clamp
│   ├── rust-specialist
│   └── frontend-specialist
└── reviewer
```

**Invariant (child attenuation).** For every child spawn:

```
child.grant = parent.grant.meet(requested).meet(host_clamp)          // parent.grant = enforced_caveats(&parent.key)
child.key   = newt_identity::attenuate(&parent.key, &child.grant)?   // signed; ⊑ parent by construction
// and afterwards: enforced_caveats(&child.key) == child.grant
```

- `meet` is `Caveats::meet` — the lattice meet, so `child.grant ⊑ parent.grant` always
  (`Caveats::leq`); `attenuate` would refuse (`CaveatAmplification`) anything wider, so the meet is
  what makes the call infallible on the authority axis. This is the general form of what
  `CrewRunner` already does with `caveats.meet(crew_clamp())` (#739); Module Scopes make it the
  *only* way to obtain a child.
- `requested` is the child's *asked-for* `Caveats` (from its role/loadout or the spawning agent); it
  can only remove authority, because meet with anything is ≤ the parent.
- `host_clamp` is a **host-minted `Caveats`** — the `crew_clamp()` lineage — applied at every level,
  so a permissive parent cannot re-widen something the host narrowed. How the host derives it (from
  operating mode, from config) is data. It is *not* a `GatePolicy`: `GatePolicy`
  (`agent-bridle-core` `config.rs`: `default_strength_floor: AxisEnforcement`,
  `max_freshness_window`, step-up `HumanGate`) is Gate configuration with no `meet`, and
  `OperatingModeControl` is a trait, not a state. Those are separate inputs: the fence-strength floor
  travels with the Gate (`Gate::with_strength_floor`) and on delegation only ever **raises**.
- The domain carrier attenuates in step, by the same laws: `child.domain[i] =
  parent.domain[i].meet(requested.domain[i]).meet(host_clamp.domain[i])` per `InterfaceId` — computed
  and checked by the host's `meet`, not signed into the chain ("The domain carrier" above).
- Budgets are *sub-allocated*, not met: `child.budget ≤ parent.remaining`, and the child's usage rolls
  up into the parent's accounting. Different axis, different rule.
- Kit view: `child.visible ⊆ parent.visible` because the child's selection is a subset of the
  parent's selection (data) and each `effective` is a meet with a smaller grant.

### Widening and revocation through the tree

The Grant is fixed at spawn; the two ways it changes at the root must have a defined effect on
children:

| Event at parent/root | Effect on children | Why |
|---|---|---|
| Prompted **widening** (`PermissionGate::ask` → `Allow` → root re-mints a wider Grant via `widen_caveats`) | **None.** Existing children keep their narrower Grant. A child that needs the new authority is re-spawned by its parent under the new ceiling. | Authority flows down only at spawn; automatic propagation would be ambient amplification. |
| **Narrowing / revocation** (host bumps the generation, or re-clamps the parent) | Whole subtree affected at once — **provided the Grants are pinned.** `Gate::check_generation` fails only when the Grant's `valid_for_generation` is `Scope::Only({..})` and excludes the Gate's generation; every Grant newt mints today is `Scope::All` (`newt-core/src/config/permissions.rs`, `agentic/tools.rs`, `role_profile.rs` — deferred under #755 / epic #749), under which nothing ever fails. So the **host clamp must pin `valid_for_generation = Scope::Only({current})`** for a spawned module (#755), and `child.grant.valid_for_generation ⊆ parent's` follows from the meet. `Gate::generation()` has no setter: a bump is the host rebuilding each module's Gate at the new generation, after which every pinned descendant's next `authorize` fails with `ToolError::Generation`; the runtime surfaces this as `Ready → Draining` (`revoked`) and reaps children. A **re-clamp is a re-attenuate**, never a mutation of the cached Grant (`grant == enforced_caveats(&key)` must keep holding): the parent re-spawns the child with `attenuate(&parent.key, &new_grant)`; a child that cannot function under it is stopped. | Revocation is a Bridle mechanism (`Gate::generation`, `Caveats::valid_for_generation`); modules only react. |

Every level therefore: holds its own principal, has a strictly-not-larger Grant, has its own
sub-budget, communicates only via the mailbox, and can (future) be proxied across the mesh.

## What isolation a Module actually provides

Be honest about the guarantee, per execution mode. A module is a **context**; the strength of the
boundary is a property of *where the code runs*, not of the module type.

| Execution mode of a kit instance | Status | Trust | What the boundary really is |
|---|---|---|---|
| Built-in Rust (same process) | exists | Trusted | **Logical scoping + accounting only.** Authority is enforced at each Bridle `Gate` mint and `ToolContext::check_*`; nothing stops trusted code from ignoring the module. Sufficient because the code is ours. |
| Native dylib (`dlopen`) | proposed (no `libloading` in the workspace) | Trusted-only | Same as built-in: in-process native code shares the process's full power. There is **no capability sandbox after `dlopen`**; a dylib is loaded only if it would be trusted as a built-in. |
| WASM component | proposed (no `wasmtime` in the workspace) | Constrained | Real memory / syscall confinement from the runtime; authority still flows only through host imports the module hands it (capability handles). |
| Subprocess — inference-provider plugin (`plugins-protocol`: `PluginClient` / `PluginServer` / `PluginHandler`) or the proposed command plugin ([command_plugin_runtime.md](command_plugin_runtime.md)) | provider plugin exists (spawned unconfined via `tokio::process::Command`; the floor is applied to MCP via `ConfinedCommand` today, not to provider plugins); attenuated-key spawn wiring pending; command plugin proposed | Constrained by **Bridle's enforcement floor** (target; see kit-system.md Motivation for today's state) | OS process boundary. The provider protocol is `initialize` / `list_models` / `complete` — there is **no call-back channel to the host's Gate**; the child enforces on its own side from the caveats in its envelope (`caveats_from_envelope`). The child is *designed to* receive an attenuated key via `newt_identity::delegate_for_plugin` over `AGENT_KEY_ENV` (mechanism exists; the host-side caller `plugin_envelope_for` is test-only today). The floor is Bridle's OS-level fences: `Gate::with_sandbox` / `SandboxKind`, `Gate::with_strength_floor(AxisEnforcement)` (Advisory vs Kernel, fail-closed), `spawn.rs` / `rootfs.rs` / `net_proxy.rs`. |
| Remote principal (mesh peer, `newt-mesh` — excluded crate) | exists | Constrained principal + delegated grant | Network boundary; the peer holds its own key and a delegated, attenuated `Caveats`; the host verifies the cert chain and applies its own `Gate`. |

So: an in-process module gives *scoping, accounting, provenance and lifecycle*. **Hard isolation
starts at WASM / process / container.** Docs and UI must not describe modules as sandboxes.

## Mailbox

Modules communicate through a bounded mailbox rather than shared state. Honest about typing: the
**envelope** is typed (variant, ids, sender principal, reply channel); the **payload** is
`serde_json::Value`, because the mailbox is the lowest common shape that also rides the mesh.
Per-interface typing comes from the *handles* a module is given (`Subscription<ResponseEvent>`,
`Session<AudioFrame, TranscriptEvent>` — kit-system.md shapes), not from the mailbox. Requests
carry the sender's principal so the receiver can attribute and, where relevant, apply its own Gate.

```rust
// sketch — illustrative, not compiled
pub enum ModuleMessage {
    /// Request from parent or peer; the reply channel is one-shot and non-clonable.
    Request { id: MessageId, from: PrincipalId, payload: serde_json::Value,
              reply: oneshot::Sender<ModuleResponse> },
    /// Fan-out event on a subscription the receiver was *handed* (a capability
    /// handle), not an ambient global topic.
    Event { subscription: SubscriptionId, from: PrincipalId, payload: serde_json::Value },
    /// Lifecycle signal from the parent/host.
    Shutdown { graceful: bool },
}
```

Rules:

- **Bounded** (`mpsc` with capacity): back-pressure is a scheduling concern; a full mailbox slows the
  sender, it does not drop authority or budget.
- **Handles, not buses.** A module receives `Subscription`/`Publisher` handles from its parent at
  spawn; it cannot name arbitrary topics. This matches the pane contract in
  [tui-panel-system.md](tui-panel-system.md) and keeps the mailbox out of the authority vocabulary.
- **Cross-process/mesh:** the same message shapes ride `newt-mesh` (excluded crate) behind a proxy;
  the sender's principal is then a verified remote key.
- Existing lineage: `SteeringInbox` (`newt-core/src/agentic/steering.rs`) and `SessionRegistry` /
  `OutputSink` (`newt-core/src/session.rs`) are the in-process precedents to widen, not fork.

## Lifecycle

```
Spawned ──ready──▶ Ready ──shutdown / budget-exhausted / revoked ──▶ Draining ──▶ Stopped
   │                 │                                                             ▲
   └────error────────┴──────────────────────────▶ Failed ──────────────────────────┘ (children reaped)
```

| Hook | Fires | Typical use |
|---|---|---|
| `on_spawn` | after principal + Grant are minted, before any kit call | register with matrix / dock (`DockRegistry`), open mailbox |
| `on_ready` | first time the module can accept requests | announce to parent |
| `on_child_spawn` | parent-side, after the child invariant is checked | attach accounting roll-up |
| `on_revoke` | parent Grant narrowed or generation bumped (module Gates rebuilt at the new generation) | drain, or re-spawn under a re-attenuated Grant; cascade to children |
| `on_error` | unrecoverable fault | emit provenance record, notify parent |
| `on_shutdown` | entering `Draining` | flush ledger, deregister, cancel children (children never outlive their parent's Grant) |

Hooks are async and non-blocking with respect to the mailbox; a hook that needs ordering awaits an
explicit ack rather than blocking the runtime thread (`newt-tui` already runs the session on its own
thread — same discipline).

Resource-budget exhaustion (tokens / spend / memory / concurrency) is a lifecycle transition
(`Ready → Draining`), surfaced as an event to the parent and to the UI; it is not an authorization
failure and must not be rendered as one. Tool-call exhaustion (`max_calls`) is the opposite case — a
Bridle `ToolError::Budget` denial at authorize time — and is rendered as what it is: authority.

## Module runtime (sketch)

```rust
// sketch — illustrative, not compiled — home: newt_core::module (see "Crate placement")
pub type PrincipalId = agent_mesh_protocol::Fingerprint;   // AgentKey::fingerprint()

pub struct Module {
    key: AgentKey,                            // cryptographic identity; PrincipalId = key.fingerprint()
    display_name: String,                     // label only
    grant: Caveats,                           // == newt_identity::enforced_caveats(&key), cached at spawn; never mutated
    domain: DomainGrants,                     // host-held per-InterfaceId DomainCaveats beside the Grant (not signed)
    gate: Gate,                               // THIS module's Bridle Gate: with_budget(host_gen, grant.max_calls) + host sandbox/floor
    view: ScopedKitView<'static>,             // selection data + grant ⊓ required annotation
    kits: KitInstances,                       // ToolContext handles minted through the Gate (Session/Source/View)
    budget: ResourceBudget,                   // tokens / spend / memory / concurrency
    mailbox: Mailbox,                         // bounded; typed envelope, JSON payload
    state: LifecycleState,
    parent: Option<PrincipalId>,
    children: HashMap<PrincipalId, Arc<Module>>,
}

/// What the host contributes to every module's Gate: the generation counter and the
/// OS sandbox / fence-strength floor. A Gate is a Bridle object; the host builds it.
pub struct HostGatePolicy { pub generation: u64, pub sandbox: Arc<dyn Sandbox>, pub floor: AxisEnforcement }

impl HostGatePolicy {
    fn gate_for(&self, grant: &Caveats) -> Gate {
        Gate::with_budget(self.generation, grant.max_calls)      // per-module persistent call budget
            .with_sandbox(&*self.sandbox)
            .with_strength_floor(self.floor)                      // on delegation only ever raises
    }
}

impl Module {
    pub fn principal(&self) -> PrincipalId { self.key.fingerprint() }

    /// The only constructor for a non-root module. Enforces the child invariant.
    pub async fn spawn_child(&self, req: ChildRequest, host_clamp: &HostClamp, host: &HostGatePolicy)
        -> Result<Arc<Module>, ModuleError>
    {
        let grant = self.grant.meet(&req.requested).meet(&host_clamp.caveats);   // host_clamp pins valid_for_generation (#755)
        debug_assert!(grant.leq(&self.grant));
        let key = newt_identity::attenuate(&self.key, &grant)?;   // CaveatAmplification is unreachable after the meet
        debug_assert_eq!(newt_identity::enforced_caveats(&key)?, grant);
        let domain = self.domain.meet(&req.domain).meet(&host_clamp.domain);     // same laws, host-checked, unsigned
        let gate = host.gate_for(&grant);                          // fresh persistent budget for THIS child
        let budget = self.budget.sub_allocate(req.budget)?;       // ≤ remaining
        /* … mailbox handles, hooks, register child … */
    }

    /// Every Action-shaped call is a Bridle mint on THIS module's Gate against THIS grant.
    /// Signature per agent-bridle-core 0.7.15: `Gate::authorize(tool, &granted) -> ToolResult<ToolContext>`;
    /// `Tool::invoke(args, &ToolContext) -> ToolResult<serde_json::Value>`. The `&dyn Tool` comes from
    /// the host's tool table (kit-system.md, "Bridle widening this depends on") — not from
    /// `Registry::dispatch`, which would mint its own fresh Gate and bypass the module budget.
    pub async fn call(&self, tool: &dyn Tool, args: Value) -> Result<Value, ModuleError> {
        let _slot = self.budget.reserve_slot()?;              // concurrency: Resources axis (RAII)
        let ctx: ToolContext = self.gate.authorize(tool, &self.grant)?;   // effective = granted ⊓ tool.required(); charges the call ONCE
        let out = tool.invoke(args, &ctx).await?;             // per-op refusal via ctx.check_*
        self.budget.account(usage_of(&out));                  // tokens/spend: Accounting axis, after the fact
        self.emit_provenance(&ctx, tool);                     // ProvenanceRecord { principal, grant_digest, manifest_cid, export_id, artifact_cid }
        Ok(out)
    }

    /// Non-Action exports (Session / Source / Sink / View): the Gate is consulted ONCE at open;
    /// the returned `ToolContext` is the capability handle and lives in `KitInstances`. The
    /// domain carrier is met once here too (`self.domain[iface] ⊓ export.required.domain`).
    pub async fn open(&self, export: &dyn Tool /* the "open" action of the export */)
        -> Result<KitInstance, ModuleError>
    {
        let ctx: ToolContext = self.gate.authorize(export, &self.grant)?;
        Ok(self.kits.insert(KitInstance::from_context(ctx)))     // frames/events are never re-authorized
    }
}
```

## Integration with Gilamonster-Agent

gilamonster-agent's per-role spec is **external** to this workspace and not a type this doc
resolves (`gila matrix` is a stub today). Whatever shape it takes, it maps onto a module as:
role → `RoleProfile` / loadout (kit *selection*), role's authority preset → `requested` `Caveats`
(met against the parent and the host clamp), per-role budget → `ResourceBudget`, and matrix
register/deregister → `on_spawn` / `on_shutdown` hooks. Nothing in the matrix mapping mints authority;
the matrix root's Grant (the operating key's caveats) is the ceiling.

## Dependencies and acceptance

```
Bridle authority (Gate, granted Caveats, meet, ToolContext)   ── exists (agent-bridle-core, newt-core caveats)
        │
        ├─▶ Kit = package / interface / provenance   (kit-system.md, #1737)
        └─▶ Module = principal / context (this doc, #1737; lineage #739)
                    │
                    ├─▶ normalized ResponseEvent stream, panes, media   (consume modules' events)
                    └─▶ newt-desktop, companion                          (project them)
```

Acceptance criteria for a first Module PR (no schedule; ship when green):

1. `spawn_child` is the only way to obtain a non-root module, and a test proves
   `child.grant.leq(&parent.grant)` for arbitrary `requested` / `host_clamp` (property test over
   `Caveats`).
2. **No module-level evaluator.** Every kit/tool call from a module terminates in one of Bridle's mint
   sites (`Gate::authorize` / `authorize_with_discharge` / `authorize_step_up`) on the module's Gate;
   the module owns no `allow`/`deny` decision of its own. Tested with a mocked Gate: a Gate refusal
   (`ToolError::Generation` / `Budget`) or a `ToolContext::check_*` refusal *is* the module's
   refusal.
3. `ScopedKitView::visible()` is a pure function of `(selection data, grant)`: it never calls the
   Gate, never charges a call, yields no `dyn Tool` or handle, and each listed `effective` equals
   `grant.meet(&export.required.bridle)` — which for `shell` / `web_fetch` equals
   `grant.meet(&tool.required())` (property test; a spy Gate asserts zero `authorize` calls during
   enumeration).
4. Resource-budget exhaustion (tokens / memory / concurrency) produces `Ready → Draining` and an
   event, never an authorization error; call-count exhaustion is Bridle's `ToolError::Budget` on
   **the module's own Gate**: a child spawned with `max_calls = AtMost(n)` gets exactly `n`
   successful `authorize` calls and the `(n+1)`th fails, while a sibling and the parent keep their
   own counters (test with two children); the module charges calls exactly once (through its Gate,
   never itself).
5. Principal is a `newt-identity` `AgentKey` and `PrincipalId == key.fingerprint()`; for every module
   `grant == enforced_caveats(&key)` (property test over spawn trees); the root module's Grant is the
   operating key's caveats and no `UserKey` is reachable from a module (type-level: `Module` holds no
   `UserKey` field and no constructor takes one); the display name appears in logs only as a label;
   `ProvenanceRecord`s carry `PrincipalId` + artifact CID.
6. Revocation: with the host clamp pinning `valid_for_generation = Scope::Only({gen})` (#755),
   rebuilding module Gates at `gen + 1` makes every module's next `authorize` fail with
   `ToolError::Generation` and drains the whole subtree; a re-clamp re-attenuates (new key, new
   Grant) and never mutates a cached Grant (`grant == enforced_caveats(&key)` still holds
   afterwards); a root widening leaves existing children's Grants unchanged (test all three).
7. Docs/UI never call an in-process module a sandbox; the isolation table above is reproduced in
   the `newt_core::module` crate docs.

Unit tier fully mocked (`mockall` for `CrewRunner`/hooks/Gate, in-memory registry, injected clock).

## Overlaps with existing code (reuse map)

| Concept here | Widen this, don't fork |
|---|---|
| Grant, `meet`, `leq`, call budget | `newt_core::caveats::Caveats` (agent-mesh-protocol), Bridle `Gate` (`with_budget`, `charge_one`, `ToolContext::check_*`) — one Gate per module |
| Child attenuation, host clamp | `CrewRunner::crew_clamp()` (`caveats.meet(crew_clamp)`, #739), `newt_identity::attenuate` / `enforced_caveats` (lifted into `newt-core`, re-exported by `newt-identity`) |
| Root widening / operating mode | `PermissionGate` + `widen_caveats` (upstream of the Gate), `OperatingModeControl` |
| Kit selection per role | `RoleProfile`, `Loadout.kit` + `[bundles.*]`, `newt_core::kit`, `ExposureProfile` |
| Resource budget | `CrewBudgets` (`newt-core/src/config/crew.rs`), `TokenBudget` (`newt-core/src/memory.rs`); `send_budget.rs` stays what it is (per-request input-token ceiling) |
| Accounting | `newt_core::metrics::TokenUsage`, `newt-core/src/pricing.rs` |
| Mailbox / lifecycle | `SteeringInbox`, `SessionRegistry` / `OutputSink`, `TabSidecar` |
| Subprocess trust floor | Bridle `Gate::with_sandbox` / `SandboxKind` / `with_strength_floor`, `spawn.rs` / `rootfs.rs` / `net_proxy.rs`; `plugins-protocol` (provider plugins), `command_plugin_runtime.md`; `newt_identity::delegate_for_plugin` (spawn wiring pending) |

## Open questions

1. **Hook ordering:** which hooks may block spawn completion (`on_spawn` yes, `on_ready` no?).
2. **Migration snapshot:** what of a module is serialisable for mesh migration — principal cert +
   Grant + budget remaining + mailbox cursor; kit instances are re-resolved on the far side.
3. **Default budgets and host clamps per role / operating mode:** data (loadout config), not code — a
   three-Cs candidate; which `Caveats` each operating mode clamps to must be one table.
4. **Long-lived handles vs revocation:** a `Session` / `Source` handle is minted once via the Gate; on
   a generation bump should the runtime close held handles eagerly (`on_revoke`) or let the next
   `check_*` fail lazily? Eager is safer for media streams (mic / speaker) — leaning eager.
5. **Budget top-up and the provenance ledger — owner unresolved.** Who may *raise* a module's
   `ResourceBudget` after spawn (parent only? host config? the human via a prompt like
   `PermissionGate::ask`?), and which component owns the append-only ledger that stores
   `ProvenanceRecord`s and usage roll-ups (a `newt-core` module beside `metrics.rs`, or the session
   transcript)? Both are open; the first Module PR ships budgets fixed-at-spawn and an in-memory
   ledger behind a trait so the owner can be decided without re-plumbing.

## Change log

- 2026-08-16: `ScopedKitRegistry` renamed `ScopedKitView` (a pure view, never an evaluator); the
  single `ModulePermissions` bag split into the five axes; principal identity and child attenuation
  bound to the existing `newt-identity` chain instead of a new type.

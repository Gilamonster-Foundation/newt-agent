# Design: the tool-exposure controller

Status: in progress (Pass 1 + budget-clip milestone). Supersedes the ad-hoc
"progressive tool-schema disclosure" sketch. Related: #725 (tool_search;
lazy-load was deferred there), #1387 (Code Navigator added 10 `Gate::Always`
tools), #546 / #712 (progressive disclosure epics), the model self-tuning
subsystem (`newt-tui/src/probe.rs` — live `safe_context`).

## The problem

newt advertises the whole gate-satisfied catalog on **every** request. #1387
alone added ten `Gate::Always` navigator schemas; connect one large MCP server
and the tool block dwarfs a small model's usable window. Tool schemas are
counted against the send budget (`tool_tokens` at `agentic/mod.rs` ~1293), so a
fat catalog directly steals room from history and evidence.

The same model needs different tool exposure on turn 30 than on turn 1, and a
3.4k-context model cannot afford the same surface a frontier model enjoys.
**Name count and parameter count are poor proxies for what actually fills the
window.** The primary control signal must be the model's *live, usable context
budget*, not its name or a static per-model config.

## Control theory

> Profile seeds the envelope, live usable context sets the budget, deterministic
> task intent fills it, sticky use stabilizes it, and `tool_search` repairs
> mistakes — while authorization stays a separate plane and schemas stay
> projections of harness law.

### The four-stage pipeline (the load-bearing invariant)

```text
let known      = registry.all();                       // every dispatchable name
let present    = filter_presence(known, session);      // Gate / injected capability
let authorized = filter_authority(present, persona, disposition, caveats);
let exposed    = exposure_policy.select(authorized, budget, task, active_set);
let on_wire    = provider_contract.project(exposed);  // e.g. OpenAI <= 128 tools
```

**Exposure is never authorization.** Dispatch checks `authorized`, never merely
`exposed`. Hiding a schema to save tokens must never widen *or* narrow what the
model is permitted to run — it only changes what it is *shown*. A model that
calls a real, authorized-but-unexposed tool is not hallucinating (see
`ToolReach::KnownHidden` below).

### Budget is the primary signal

```text
schema_budget_tokens = live_usable_budget * schema_budget_pct
```

`live_usable_budget` is the SAME number the loop already steers on — the
send budget derived from probed `safe_context` / `max_ok_input` /
`num_ctx_input_ceiling` (`agentic/send_budget.rs`). Never the model name. When
no live budget signal exists (e.g. a cloud endpoint with no `/api/show`,
headless with no probe), the controller does **not** clip — no signal means no
starvation.

Selection each turn:

1. Always include **Kernel** tools (session-critical loop control + fundamental
   workspace verbs) regardless of budget. If Kernel alone exceeds budget, that
   is a Kernel-definition bug, not an eviction case.
2. Include the **sticky active set** (recently/successfully used this phase) —
   oscillating affordances are cognitively poisonous, so a tool the model just
   used does not vanish because a score shifted.
3. Fill remaining budget by priority (ByIntent task bundles, then
   Recovery/OnDemand) until `estimate(exposed) <= schema_budget_tokens` or an
   optional `max_initial_tools` circuit-breaker trips.

`max_initial_tools` and per-family count caps are **safety rails, not the
governor** — the budget is.

## Exposure classes (data, not logic — three Cs)

Every dispatchable tool carries an exposure class. The classification is pure
data with a drift test asserting it covers exactly the known catalog, so a new
tool cannot silently ship unclassified.

| Class | Meaning | Budget behavior |
|-------|---------|-----------------|
| `Kernel` | Session-critical control + fundamental workspace verbs (base tools, `tool_search`) | Never evicted |
| `ByIntent` | Loaded when task intent / recent use suggests it (navigator, git, crew, code_search, scratchpad, experiential, scheduled, operating-mode) | Evictable under budget |
| `RecoveryOnly` | Surfaced when the backing artifact/context exists (`resume_context`, `prompt_read`, `artifact_read`, `render_report`, `request_user_input`, `get_context_remaining`) | Evict early; event-gating is a later pass |
| `OnDemand` | Only after explicit discovery / a `KnownHidden` retry (MCP `server__tool`, deferred families) | Not exposed until promoted |

## Schema projection (a later pass, seam reserved now)

Even an exposed tool need not carry its full statute book every turn. Three
projections of one source of truth:

- **Full** — teaching, caveats, examples (frontier models / rich sessions).
- **Compact** — action + key constraint + argument meaning (micro tier).
- **Index** — name, family, 6–10-word purpose (for `tool_search` listings).

The harness still enforces the full policy regardless of which projection the
schema carried.

## Hidden-tool recovery (a later pass, outcome enum reserved now)

```text
enum ToolReach { Exposed, KnownHidden, KnownUnauthorized, Unknown }
```

`KnownHidden` (a real, authorized, unexposed tool) returns a coaching message
that activates the tool into the working set and asks for a retry against the
now-visible contract — clearer audit semantics than silently executing
arguments the model never saw a schema for.

## Config

```toml
[tool_exposure]
profile = "full"              # full | auto | minimal
schema_budget_pct = 15        # % of live usable budget spent on tool schemas
max_initial_tools = 0         # 0 = unlimited; a safety rail, not the governor
supports_dynamic_catalog = true  # reserved for per-round updates (Pass 5)
```

`profile = "full"` (the default) keeps the **exposure controller** as a
bit-for-bit identity. `auto` / `minimal` engage budget-driven selection. A
provider's final wire projection still applies independently; OpenAI-compatible
requests retain Kernel tools and fit at most 128 function schemas.

## Implementation sequence (separable PRs)

1. **This PR — separate availability / authority / exposure + budget clip.**
   Introduce the exposure stage as a real pipeline step wired at all three loop
   sites, the exposure-class data + drift test, the `[tool_exposure]` config,
   and budget-driven selection seeded by live `safe_context`. Default `full`
   does not clip at this exposure stage; final provider-contract projection is
   separate. First live-testable milestone: a small model sees a pocket
   multitool instead of the whole hardware aisle at turn start. No mutable
   mid-turn catalogs required.
2. Schema measurement + Compact/Index projections; emit catalog-token metrics.
3. Deterministic task-intent bundles fill the budget (data-driven family map).
4. `tool_search` searches and activates hidden authorized tools; sticky working
   set + `ToolReach::KnownHidden`.
5. Per-round working-set updates where the backend `supports_dynamic_catalog`.
6. MCP under the same law: `mcp_search` / server-category bundles instead of
   appending every MCP schema.

## Out of scope for Pass 1

Mid-turn / per-round catalog mutation; schema projection; `tool_search`
activation of hidden tools; sticky working set; `ToolReach` plumbing; MCP
`mcp_search`; event-gating RecoveryOnly on artifact existence. The *seams* are
reserved (classes, config field `supports_dynamic_catalog`) but not wired.

Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock |
Time: 10:57 EDT | Date: 2026-08-13

# Decision: the Gilamonster agent line

**Status:** Accepted as target architecture (Shawn Hartsock, 2026-08-17).
*Ratified* in this document means merged to `main`. This ADR is ratified when
newt-agent PR #1753 merges; the charter amendment is ratified when wyvern-agent
PR #53 merges. Anything gated on ratification stays blocked until then.
**Supersedes in part** the wyvern-agent charter ratified 2026-06-04, in the two
places named under "What this supersedes". The companion change is wyvern-agent
PR #53, open at the time of writing.
**Date:** 2026-08-17
**Related:** wyvern-agent `docs/CHARTER.md`, wyvern-agent ADR-0002 (crate map),
`docs/decisions/plain_scroller_tui.md`, `docs/decisions/ocap_confinement_model.md`,
`docs/decisions/mesh_integration.md`.

---

## Capability ordering

```
wyvern  ≤  newt  ≤  gilamonster
```

Read `≤` as "is a capability subset of". wyvern is the smallest thing that
works; gilamonster is the largest. This restates the charter's
`newt-agent ⊇ wyvern-agent` and says nothing by itself about which crate
depends on which. Dependency topology is stated separately below, because
conflating the two is what muddied earlier drafts.

## Current state, 2026-08-17

Verifiable today, as distinct from anything intended.

```
agent-mesh-protocol "0.6"      root of the graph; owns Caveats, Scope,
        ▲                      CountBound, UserKey, AgentKey, CertChain
        │
agent-bridle                   owns Gate, ToolContext, Registry, Policy,
        ▲                      Landlock and seccomp confinement
        │  registry 0.7.10-0.7.15, per crate
   newt-agent  ◄── git rev ba56944 ── gilamonster-agent
                                      (also agent-bridle-core 0.7, registry)

   wyvern-agent                       isolated: no edge to any of them
```

| | today |
|---|---|
| newt-agent | Working harness. `agent-mesh-protocol = "0.6"` and `content-addressable = "0.1.0"` in `[workspace.dependencies]`. agent-bridle is **not** a workspace dependency; it is pinned per crate at three versions: `0.7.15` in newt-core, `0.7.10` in newt-mcp-server and newt-mcp-client. |
| wyvern-agent | Five crates, about 1.7k lines, mostly seams and stubs. Depends only on `blake3`, `tokio`, `async-trait` and its own crates. |
| gilamonster-agent | Single package, shipping two binaries (`gila` and `newt-net-guard`). Consumes newt over a **git dependency pinned to one rev**, not published crates. Ambient-first: it starts its coder with host filesystem, network and command authority, and `--ocap` opts into newt's posture. That posture is recorded in an ADR still marked *Editing / Accepted: TBD*, so it is not ratified. |

Three facts a reader should not have to discover the hard way:

- wyvern is isolated. It has no dependency on newt, gilamonster,
  agent-mesh, or agent-bridle, in either direction. The charter's reuse
  contract states intent that is not yet wired.
- gilamonster consumes newt by git rev, not by published crates as the
  charter describes, and has no wyvern edge at all.
- `Caveats` belongs to agent-mesh-protocol, not agent-bridle. bridle
  depends on mesh and enforces against mesh's type. (`agent-mesh-core` is not
  a crate; it is a local package rename of `agent-mesh-protocol` used inside
  `newt-mesh` and `newt-web`.)

Also drifting: ADR-0002 maps twelve wyvern crates. Five crates ship, but only
three of them (`wire`, `flight`, `hangar`) are in the map; `wyvern-dispatch`
and `wyvern-agent` are not. ADR-0002 also records as a consequence that "the
Crawl scaffold creates all 12 members as stubs", which is not the case.
Separately, the charter's reuse contract names `agent-bridle-core 0.1.0` while
every real consumer is on 0.7.x.

## Target architecture

wyvern-agent becomes a small, headless, containerized worker, deployed under
[OpenShell](https://github.com/NVIDIA/OpenShell) (a sandboxed runtime for
agents, governed by declarative policy), that newt-agent or gilamonster-agent
dispatch OCAP caveats to for
work that runs headless or on a schedule. Aspirationally it reaches that shape
as a rewrite of newt-agent that is lighter, faster and smaller, at which point
newt's crates are retired in favour of it.

Two graphs. They point in different directions and must not be conflated.

**Library dependency (compile time).** Shared functionality moves *down*.

```
gilamonster ──┬──► newt ──► wyvern ──► agent-bridle ──► agent-mesh-protocol
              └╌╌╌╌╌╌╌╌╌╌╌╌╌╌►┘
                 (dashed: gilamonster's direct edge to wyvern is a working
                  assumption, see open question 4)
```

- Shared, stable functionality belongs in wyvern, the minimal layer.
- newt-specific functionality stays in newt. gila-specific stays in gila.
- **A lower layer must never depend on a richer one.** wyvern must not depend
  on newt or gilamonster. This is the rule that keeps the ordering real.

**Runtime dispatch (process).** Authority and work flow *down* from the richer
agent to the worker.

```
newt or gilamonster ──dispatches caveats──► wyvern worker (OpenShell container)
```

## What this supersedes

| wyvern charter, 2026-06-04 | superseded by |
|---|---|
| Roles table: the Desk (dispatcher) is wyvern, the Pilot (worker) is a `newt worker` process. | The roles invert. wyvern is the dispatched worker; newt and gilamonster dispatch to it. |
| "newt is a superset of wyvern by default: as newt grows it builds the wyvern airframe *into itself*." | The capability claim `newt ⊇ wyvern` stands and is restated above. The direction of absorption does not: wyvern is rewritten as the smaller base and newt is rebuilt on it, rather than newt absorbing wyvern. Note the charter's claim was always about capability, never a crate edge; no such edge exists in either direction today. |
| Reuse contract: "**newt-agent** (dogfood): the worker is `newt worker` (ACP / stdio). Reuse `newt-eval` for scorecard plumbing and `newt-core` where it fits." | Superseded. That clause makes wyvern depend on newt, which the dependency rule above forbids. wyvern keeps the rest of the reuse contract (agent-mesh, agent-bridle, gix/git2) and drops the newt edge. |
| Crate map entries that encode the old roles: `wyvern-hangar` "launch + supervise `newt worker` processes"; `wyvern-scramble` "the Desk's dispatch engine". | Superseded with the roles table. Both describe wyvern dispatching newt. Their replacements are for wyvern-agent to specify, not this document. |
| "we do **not** want to slow newt down; we want a *separate*, deliberately light agent." | Partly superseded. wyvern stays deliberately light, but it is no longer *separate*: newt is rebuilt on it. |

Everything else in the charter stands, including headless-only, patch-not-prose,
no-vendor-code, `yolo ⇒ hermetic`, and the non-newt half of the reuse contract.

Two consequences an implementing agent must not get wrong. First, no wyvern
crate may take a dependency on newt, gilamonster, or their published crates,
whatever any surviving charter or ADR-0002 sentence says. Second, the charter's
term "`newt pilot`" belongs to the old vocabulary in which newt was the worker;
read it as "the richer agent's dashboard", which is not wyvern's job either way.

## The authority floor

The floor is a **shared authority model**, not a required posture. A descendant
may run maximally permissive. It may not invent a second authority system.

Shared, and not to be reimplemented per repo:

- One mediation seam. agent-bridle `Gate` / `ToolContext` / `Registry`.
- One capability vocabulary. `Caveats`, `Scope`, `CountBound` from
  agent-mesh-protocol. One spelling of a permission across all three agents.
- One identity and provenance chain. `UserKey` / `AgentKey` / `CertChain`,
  and content-addressed journals for the record of what happened.
- One observability contract. Structured logs and ndjson, so an authority
  decision is visible after the fact in every layer.

Posture is free. wyvern's `yolo ⇒ hermetic` invariant and gilamonster's
permissive default are both legitimate positions within this floor.

**Recommended, not yet decided:** today's permissive paths are *bypasses*.
wyvern no-ops the `Gate` under yolo; newt has `unbounded_debug_fallback` and the
`NEWT_FULL_ACCESS` family. A bypass cannot be audited, because nothing records
what authority was in force. Making these an explicit `AllowAll` (a top
capability) that flows through the same `Gate` would keep the permissive posture
and make it observable, which is what `yolo ⇒ hermetic` needs to be
checkable rather than asserted.

## Contracts survive a rewrite; implementations do not

newt-agent is transitional, so care belongs where a rewrite inherits it. Preserve
and carry forward:

- Wire and schema contracts. Payload types, envelope formats, on-disk schemas.
- Identity and provenance contracts. Key handling, fingerprints, signed
  records, content-addressed chains.
- Configuration and capability vocabulary. Config keys and the permission
  names they resolve to.
- Observable behavioral invariants. What an operator can rely on seeing,
  including the committed-output contract.
- Security invariants. Fail-closed defaults, attenuation-only rules,
  `yolo ⇒ hermetic`.
- Protocol and conformance tests. The tests that pin the contracts above.
- Compatibility fixtures and evals. Recorded vectors and eval cases that
  prove a rewrite still behaves like the thing it replaces.

Implementations, surfaces, and internal structure are expected to be rewritten
and should not be defended.

## Deduplication policy

- Deduplicate downward once semantics are stable and shared: move them into
  the minimal layer that needs them.
- Tolerate duplication while layers are still discovering their abstractions.
  Two implementations that are converging are cheaper than one wrong shared one.
- Do not merge divergent TUI or authority behaviour for reuse alone. Those
  differ deliberately between layers; a shared abstraction over them would encode
  a similarity that is not real.

## Sequencing

1. Get newt-agent working. It is where behaviour is proven.
2. Rewrite it into wyvern, smaller and faster.
3. Rebuild newt and gilamonster on that base.
4. Retire newt-agent crates in favour of rewrites.

## Relationship to the plain-scroller migration

`docs/decisions/plain_scroller_tui.md` carries a migration notice moving the
LeanTUI *input surface* to wyvern. **That migration depends on this document
being ratified, and on one unresolved conflict.** The wyvern charter says
"Headless. No TUI, ever. A ratatui dependency is exactly the weight wyvern
refuses." A crossterm input surface is not ratatui, but it is still an
interactive surface, and moving one into wyvern needs the charter amended or the
migration retargeted. Until that is settled, treat the notice as intent, not as
an established plan, and move no code.

The plain-scroller *output* contract is separate and not in conflict: structured,
greppable, headless output is what the charter already asks for.

## Open questions

1. Does wyvern host any interactive surface at all? The charter says "No TUI,
   ever", and ADR-0002 is stronger still: "No TUI dependency anywhere in the
   workspace (no ratatui/crossterm)", which names crossterm explicitly and is
   not superseded by anything. The plain-scroller migration assumes wyvern will
   host a crossterm input surface. **Until this is resolved the migration is
   blocked**, and nothing in this document unblocks it.
2. What is "lighter, faster, smaller" measured against? An aspiration with no
   number loses to feature pressure later. A binary-size or startup budget
   recorded now is cheap.
3. Is `AllowAll` adopted in place of the current bypasses?
4. Does gilamonster depend on newt, or only on wyvern? The charter has it
   assembling from published crates of both. That is a DAG rather than a chain,
   and it is recorded here as the working assumption rather than a ratified one.

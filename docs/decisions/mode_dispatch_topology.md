# Decision: `/mode` Dispatch Topology (single · crew · mesh · remote) + plan

**Status:** Proposed
**Date:** 2026-06-21
**Tracking issue:** Gilamonster-Foundation/newt-agent#554
**Extends** `docs/decisions/agentic_object_capability_security.md` (the OCAP
Caveats lattice is the authority substrate this composes with) and the `/mode`
named-permission-preset mechanism (#307). **Relates to**
`docs/design/crew-swarm-overseer.md`, `docs/design/crew-loadout.md`,
`docs/design/thinking-effort-and-plan-mode.md`,
`docs/design/workflow-swarm-harness.md`.

---

## TL;DR

`/mode` today (#307) controls **one** axis: it loads a named skill body and
clamps authority to that mode's **floor** (a `meet` against the session's
caveats). That is the *authority* axis.

The fleet is built **inside-out** across three passes — (1) build everything
into one newt airframe, (2) let **co-equal** newt complexes share tasks/skills/
knowledge over agent-mesh, (3) long-lived remote residents — and roles are
**personas on one airframe**, not separate binaries. That progression is a
*second, orthogonal axis*: **dispatch topology** — *where* a turn executes.

This decision adds that axis to `/mode`:

| `/mode <topology>` | Meaning | Pass | Code today |
|---|---|---|---|
| `single` | one Reasoning Kernel — the plain REPL | baseline | **built** (default) |
| `crew` | local in-process crew on this airframe | 1 | **built** — `newt-scheduler` + `LocalCrewRunner` |
| `mesh` | co-equal newt peers share tasks/skills/knowledge | 2 | **designed** — `MeshCrewRunner`, wyvern-agent#42 |
| `remote` | long-lived parked residents (`RemoteCrewRunner`) | 3 | **roadmap** — Phase 23 |
| `plan` | human authors a `Plan`; dispatch it into the active topology | — | `Plan` exists (#338); plan *mode* designed |

Two invariants make this safe to ship and safe to explore:

1. **Topology never widens authority.** It chooses *where* work runs; the
   authority floor (the existing `/mode` clamp) and per-hop attenuation
   (`meet`) decide *what* it may do. Escalating topology widens **reach**, never
   **authority**. Dispatch is an *amplify* and therefore gated by attestation.
2. **Dark by default.** A compile-time cargo feature (`mode-topology`, default
   **off**) keeps it out of release builds; a runtime config gate
   (`[experimental].dispatch_topology`, with per-topology sub-gates) keeps it
   off until we turn it on. A disabled topology **fails closed** with an honest
   refusal — never a silent fallback to a wider one.

---

## 1. Why a second axis (and why on `/mode`)

`/mode` already owns the right shape: it is an atomic, mid-session,
authority-affecting switch that loads a skill and clamps caveats to a floor
(`newt-tui` `build_mode`/`active_mode`; `[modes.<name>]` in `newt-core/src/
config.rs`). The `/loadout` design note (`docs/design/loadout-composition.md`)
already observes a loadout is "isomorphic to today's `/mode` clamp." Adding
topology is the same kind of switch on a different axis, so it belongs on the
same command rather than a new one.

The axes are genuinely orthogonal:

- **Authority axis** (existing): *what may this turn touch?* — the floor clamp,
  a down-set of caveats. `on-call-triage` = read-only; `ship` = write+push; etc.
- **Topology axis** (new): *where does this turn execute?* — in this process, in
  a local crew, across mesh peers, or on a parked remote resident.

You compose them: `/mode crew` runs a crew **under whatever authority floor is
active**. A read-only floor + a crew is a read-only crew. The cross-product is
the point — neither axis subsumes the other.

> **Decision:** topology is a property of the active mode, set via `/mode`, not
> a separate `/dispatch` command. (See Open Question 9.1 if the cross-product
> proves confusing in practice.)

## 2. The topology ladder = the three passes

The ladder is exactly the inside-out build, and each rung is a **`CrewRunner`
trait swap**, not a rewrite. The seams already exist:

- `Dispatcher` — `LocalDispatcher` ↔ `MeshDispatcher` (remote *model*).
- **`CrewRunner`** — `LocalCrewRunner` ↔ `MeshCrewRunner` ↔ `RemoteCrewRunner`
  (remote *crew member*).
- `PoolSource` — `StaticSource` ↔ `MeshSource` (peer discovery).
- `Workspace` — `WorktreeWorkspace` ↔ `RemoteWorkspace` (effects).

**`single`** — the Reasoning Kernel alone: `newt-core::agentic` driver over an
`InferenceBackend`. No crew. This is the floor of the ladder and today's
default.

**`crew`** (Pass 1, *built*) — `newt-scheduler`'s `Crew`/`Team`/`Panel`/`Roster`
over a `BackendPool`, dispatched through `newt-core::agentic::crew_tool` and
backed by `newt-cli`'s `LocalCrewRunner` on isolated `WorktreeWorkspace`s.
Honest gates (verification is harness-owned), caveat attenuation at apply,
attestation structure already present.

**`mesh`** (Pass 2, *designed*) — swap in `MeshCrewRunner`: ship a
`CrewTask{goal, caveats, workspace_ref}` to a **co-equal** newt peer over
agent-mesh and get back a `CrewResult{diff, status, ledger}`. Peers are
discovered via `MeshSource` (mDNS `Browser`, capability-filtered) and addressed
by `MeshAsker`; every envelope carries a signed `CertChain` + attenuated
`Caveats`. "Share tasks/skills/knowledge" decomposes into three sub-channels of
very different maturity (see §4).

**`remote`** (Pass 3, *roadmap*) — a long-lived `RemoteCrewRunner`: a Newt/Wyvern
resident *parked* on a project that accepts `CrewTask`s across sessions. Adds
resident lifecycle (park/update/version), persistent workspace ownership across
turns, and SSH-CA + iroh dual transport. Gated on the BOOT root-of-trust and
`pre-receive` teeth (an attestation must ride with any push a remote crew makes).

**`plan`** — the human seat. In plan mode the operator authors a canonical
`Plan` (`newt-core/src/plan.rs`, #338/#334; see
`docs/design/thinking-effort-and-plan-mode.md`). "Dispatch" then feeds that
`Plan` into the **active topology** — by default `crew`, so a reviewed plan
fans out to a local crew (the `workflow-swarm-harness` budgeted-plan model).
Plan mode itself runs under a read-only authority floor until the operator
dispatches.

## 3. OCAP: topology composes with authority, it does not widen it

This is the load-bearing invariant. The OCAP decision
(`agentic_object_capability_security.md`) establishes that authority is a
**bounded meet-semilattice**, delegation is **attenuation-only**
(`child.caveats ⊑ parent.caveats`), and chains compose by `meet` — there is no
amplify operation reachable by a child. Topology must live entirely inside that
algebra:

1. **`/mode <topology>` grants no authority.** Choosing `crew`/`mesh`/`remote`
   selects an execution site. The authority in force is still the active mode's
   floor — `effective = session.caveats ⊓ mode.floor`. A read-only mode stays
   read-only regardless of topology.

2. **Every hop attenuates.** A crew member, a mesh peer, and a remote resident
   each receive a freshly-minted child `AgentKey` whose caveats are `⊑` the
   dispatcher's. `issue()` enforces `child ⊑ parent` at signing time. Three
   levels deep, the resident still cannot exceed the root grant — by
   construction, not by good behaviour. This is why recursion (a crew that
   dispatches sub-crews) is safe.

3. **Reach widens; authority does not.** The ladder `single → crew → mesh →
   remote` increases *where* work can run and *how many* kernels participate. It
   must never be a back door to *more* authority. Concretely: the floor is
   applied **before** the topology fan-out, and `meet` at each hop can only
   narrow it further.

4. **Dispatch is an amplify → it requires attestation.** Handing a sub-agent
   *any* authority is an amplification of the sub-agent's (empty) starting set,
   even though it is `⊑` the parent. The code already models this:
   `newt-core::agentic::crew_attest` (`crew_authz` / `crew_step_up_policy`)
   requires a human-presence gesture before a crew effect, failing closed on
   insufficient presence (`Presence::Prompt` today; passkey after the BOOT
   work). Therefore:
   - `single` — no amplify, no extra gesture.
   - `crew` — local amplify, presence gesture (already enforced).
   - `mesh` / `remote` — amplify **across a trust boundary**; the step-up is
     mandatory and should be the stronger factor once available. A mesh/remote
     dispatch with no attestation **denies**.

5. **The bare-repo / RTB rule still holds.** Mesh and remote crews push only to
   local bare repos under attenuated keys; exactly one privileged RTB gate
   projects approved work to a forge. Topology does not give a remote crew a
   forge token.

> **Net:** the topology axis is a router, not a grant. Authority is decided on
> the authority axis (the floor) and the OCAP lattice (per-hop `meet` +
> attestation on amplify). A fully compromised crew/peer/resident still cannot
> exceed the down-set it was minted with.

## 4. "Share tasks, skills, knowledge" — three sub-channels (`mesh`)

Pass 2's promise decomposes into three channels at three maturities; the ADR
names them so they can be gated and built independently:

- **Tasks** — `MeshCrewRunner`/`MeshAsker` dispatch of `CrewTask`→`CrewResult`.
  *Transport built, runner designed.* The mature channel; caveats travel and
  attenuate per hop.
- **Skills** — `newt-skills` loads `SKILL.md` (+ optional caveats frontmatter,
  currently **parse-only**). Sharing skills across peers wants a distribution
  protocol with **progressive disclosure** (advertise name+description; fetch
  body on activation) and caveat **enforcement** on load. *Design-stage.*
- **Knowledge** — `ConversationStore` (SQLite + FTS5 recall) is **per-session**;
  cross-peer sharing is a future cross-signed merkle-log reconcile (the
  `agent-mesh-store` direction), **not** a central blackboard. *Design-stage.*

Skill/knowledge sub-gates default off even when `mesh` tasks are on.

## 5. Feature-switching: dark by default, two layers

We want to merge and explore this without it being reachable in a release build
or by an unaware operator. Mirror newt's existing belt-and-suspenders style
(the rich/lean TUI split is a cargo feature with a `--no-default-features` lean
build, #416; OCAP enforcement is gated belt-and-suspenders):

1. **Compile-time — cargo feature `mode-topology` (default OFF).** When the
   feature is off, the topology axis is **compiled out**: `/mode crew|mesh|
   remote|plan` are not registered, help text omits them, and release artifacts
   carry no mesh/crew dispatch surface beyond what already ships. This lets the
   ADR and scaffolding land on `main` without exposing anything.

2. **Runtime — config gate `[experimental].dispatch_topology` (default false),
   plus per-topology sub-gates.** When compiled in, the operator still opts in:
   ```toml
   [experimental]
   dispatch_topology = true          # master switch for the axis
   [experimental.topology]
   crew   = true                     # Pass 1 — safe to enable now
   mesh   = false                    # Pass 2 — off until MeshCrewRunner + BOOT
   remote = false                    # Pass 3 — off until Phase 23
   plan   = true
   ```
3. **Disabled ⇒ fail closed.** `/mode mesh` when `mesh = false` returns an
   honest refusal ("topology `mesh` is experimental and disabled; enable in
   `[experimental.topology]`") and **does not** fall back to `crew` or `single`.
   Silent widening of either reach or authority is forbidden.

This composes with OCAP: a topology being *enabled* is necessary but not
sufficient; the authority floor and the attestation gesture still apply.

## 6. Phasing

1. **Scaffold + `crew` (low-risk).** Add the axis behind the off-by-default
   feature; wire `/mode crew|single|plan` onto the existing `LocalCrewRunner`
   and `Plan`. crew is already built — this is exposure + gating, not new
   mechanism. Land the ADR with it.
2. **`mesh` (high-risk).** Enable the `mesh` sub-gate when `MeshCrewRunner`
   (wyvern-agent#42) and the BOOT attestation land. Build the three sub-channels
   (§4) in order: tasks → skills → knowledge.
3. **`remote` (high-risk).** Phase 23 — resident lifecycle + `pre-receive`
   teeth; enable the `remote` sub-gate last.

Each PR carries the 80% coverage acceptance contract and the no-vendor-names
guard.

## 7. Alternatives considered

- **A separate `/dispatch` command.** Rejected for now: topology and authority
  are switched together often enough (a plan reviewed under a read-only floor,
  then dispatched to a crew under a write floor) that one command reads better.
  Revisit if the cross-product confuses operators (9.1).
- **Topology as a persona/role-profile field.** Personas already bind
  prompt+tools+caveats+model; topology is execution-site, a different axis.
  Folding it into the persona would couple "who I am" to "where I run," which
  the persona/airframe split deliberately avoids.
- **No feature gate, just config.** Rejected: an experimental cross-trust-
  boundary amplifier should not be *present* in release builds at all while we
  explore. Compile-time off is the stronger guarantee.

## 8. Acceptance

- `/mode crew` dispatches a goal to the local crew under the **active authority
  floor**; a read-only floor yields a read-only crew.
- `/mode mesh|remote` **fail closed** when their feature/config gate is off.
- No authority amplification occurs on any topology without an attestation
  gesture; a mesh/remote dispatch with no attestation denies.
- The topology axis **composes with** (does not replace) the #307 authority
  clamp; `/mode` with no arg still reports the active mode, now including its
  topology.
- With the cargo feature off, the axis is absent from help and unreachable.
- 80% coverage; no-vendor-names guard green.

## 9. Open questions

1. **Cross-product ergonomics.** Is `/mode <authority>` + `/mode <topology>` on
   one command legible, or do operators want `/mode <authority>` and
   `/dispatch <topology>` split? (Lean: one command; measure.)
2. **Conversation reset.** A persona swap resets the conversation today. Should
   a *topology* swap preserve context (per the persistent-actor principle)?
   Likely yes — topology is not a new identity.
3. **Plan-dispatch home.** Does `plan` dispatch live in `newt-core` (next to
   `Plan` + the agentic loop, post-9.7) or in the TUI? (Lean: core, so headless
   `newt worker` can dispatch a plan too.)
4. **Step-up factor for cross-boundary dispatch.** `Presence::Prompt` is enough
   for local `crew`; should `mesh`/`remote` *require* passkey (not just prefer
   it) once BOOT lands?
5. **Naming.** `remote` (the mode) vs `RemoteCrewRunner` (the impl) — keep the
   operator-facing word short (`remote`) and the type explicit.
6. **Skill/knowledge sharing wire format.** Progressive-disclosure skill packs
   + cross-signed merkle-log knowledge — does this reuse `agent-mesh` envelopes
   wholesale (no new wire format, per the mesh decision), or need a typed
   sub-protocol?

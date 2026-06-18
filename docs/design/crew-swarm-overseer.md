# Crew, swarm, and the overseer — the agent-as-instance architecture

> Status: design of record for the orchestration stack. Implementation tracked
> across PRs #425 (crew), #468 (panel), #474/#477 (team), #478 (hosted dispatch),
> #480 (roster composer), and **#479** (the agent-callable tools — in progress).

## The one idea

**An agent is not a program. It is the one runtime, scoped.**

A "crew member" — a coder, a planner, or a standing service like the drake-swarm
`keysmith` / `herald` — is **not** a bespoke codebase. It is a **newt-agent /
wyvern-agent instance** differentiated by four dials:

| dial | mechanism | example: keysmith |
|---|---|---|
| **brains** | a *loadout* (provider → model → support-kit) | a small local model |
| **tools** | the per-tool toggles (`with_git`, `with_team`, …) | `{key-ops}` only |
| **authority** | OCAP `Caveats` (access + permission) | `{vault-read}` caveat |
| **role** | the persona / system prompt | "you mint and rotate keys" |

```
keysmith = newt + {key-ops}          + {vault-read caveat}      + keysmith persona + mesh inbox
herald   = newt + {announce/publish} + {mesh-broadcast caveat}  + herald persona   + mesh inbox
coder    = newt + {git, code_edit}   + {workspace-write caveat} + coder persona     + mesh inbox
```

The profound part is **capability-as-identity**: the caveat spec is *simultaneously*
the empowerment (keysmith **may** touch the vault) and the least-privilege bound
(a coder **may not**). One mechanism is both the grant and the containment — so the
Confused-Deputy guard falls out for free. You don't *write* a keysmith; you
**instantiate** one. A **crew-member spec** is just those four dials bundled and
named — the loadout grown into a full instance identity.

## The orchestration stack (`newt-scheduler`)

Each layer is pure orchestration over three injected seams — `Dispatcher`
(inference), `BackendPool` (placement), `Workspace` (effects) — so each is
unit-testable with mocks and no network.

```
roster   compose_roster: survey live models + priors → propose role→model (#480)
  │
  ├─ crew    one task, roles divide labor: planner/navigator/triage (#425)
  ├─ team    a lead decomposes a GOAL → a crew per subtask, per-subtask verify (#474,#477)
  └─ panel   the SAME task to N decorrelated voices, verify-gate each (#468)
```

- **crew** — division of labor on one task; honest `NeedsHumanReview` on cap-exit.
- **team** — `run_team`: the lead decomposes; each subtask runs sequentially over a
  shared workspace, **stopping at the first block**, and installs its **own**
  verification (`Workspace::set_test_command`). Validated live (`examples/team_live.rs`).
- **panel** — decorrelation: each voice a distinct model *family* so no single
  blind spot decides; accept passers by agreement; all-fail is honest review.
- **roster** — the `/crew-roster` composer: `BackendPool::live_models()` surveys
  what the environment *actually* offers; `compose_roster` proposes which model
  fills which role (panel = strongest-per-distinct-family) with a **per-pick
  rationale**. It **proposes**; it never runs. The human approves.

Dispatch is hosted-capable: `BackendKind::Openai` routes to the `/v1/chat/completions`
wire with a bearer token (#478), so any of these can run on a hosted LLM, not just
local Ollama.

## The overseer pattern (the target UX)

The conversational agent loop **is** the overseer — no separate supervisory loop is
built; the model's own reasoning between tool calls *is* the oversight.

```
human      approves the plan, approves the roster, reads reports
overseer   plans · composes/selects the roster · dispatches · reviews diffs · reports
crew       does the labor, one plan-step at a time, verify-gated
```

The canonical session:

1. *"Look at X and make a plan."* → overseer reads the repo, emits a plan, shows
   you. **(gate 1: approve the plan)**
2. *"Use `/team`, select or build a roster."* → `/team` flips the toggle that
   advertises the crew tools; overseer calls `compose_roster` (or `list_crews`),
   proposes the roster with rationale, shows you. **(gate 2: approve the roster)**
3. *"Work the plan, you act as overseer."* → overseer loops: `crew(step, roster)` →
   reads the returned **diff + verify status** → accept or re-dispatch → reports
   progress, escalates on a block.

Two human gates; crews run under `meet`-attenuated caveats. Results are **structured**
(diff + status + ledger) so the display surface — rustyline progress per crew member,
an interleaved crawl, or gilamonster's multi-pane view — is a pure rendering choice,
not baked into the orchestration. (newt's own chat path stays a plain scroller.)

## Remote crew over agent-mesh (wyvern-agent#42)

The next layer: long-lived residents **parked** on a project/suite, addressed over
agent-mesh, accepting crew *or* planning tasks (and free to recurse into their own
crew). This is the agent-mesh-native, caveat-carrying productization of the
drake-codex NATS workers.

**It is a runner swap, not a rewrite.** The agent-callable `crew` tool calls a
`CrewRunner`:

- `LocalCrewRunner` runs `run_crew` / `run_team` here.
- `MeshCrewRunner` ships the task to a resident — **same tool, same `RosterSpec`,
  same approval flow.**

(Mirrors `dispatch.rs`'s `LocalDispatcher → MeshDispatcher` for remote *inference*.
Two levels: remote **model** = a Dispatcher swap; remote **crew member** = a
CrewRunner swap.)

**Recursion is safe by construction.** `agent-mesh-protocol`'s signed `Caveats`
attenuate **per hop** (`meet`, never widen): a resident receives caveats ≤ the
caller's, and if it recurses they attenuate again — a three-levels-deep resident can
never exceed the root grant. The single invariant every hop must honor: **`meet`,
never widen.**

**The cross-repo contract:**

```
CrewTask  { goal, caveats, workspace_ref }  →  CrewResult { diff, status, ledger }
```

carried over the SSH-CA / iroh dual transport — iroh/QUIC for a gnuc-resident on the
LAN, OpenSSH-cert SSH for a cloud-resident long-haul.

## Design invariants

1. **Honest gates.** Verification is the harness's, never the model's self-report;
   all-fail / cap-exit is `NeedsHumanReview`, never a false success.
2. **Attenuation only.** Every authority boundary — tool toggle, crew dispatch,
   mesh hop — can only `meet` (narrow) the caller's caveats, never widen.
3. **Propose, then act.** The composer and the planner propose; the human approves
   the plan and the roster before any crew with real authority runs.
4. **One runtime.** New crew-member *kinds* are new specs (loadout + tools + caveats
   + role), not new programs.

## Status / next

Shipped: crew, panel, team (+ per-subtask verify), hosted dispatch, roster composer.
**#479** wires the agent-callable `crew` / `compose_roster` tools (the `CrewRunner`
trait in `newt-core`, mirroring `git_tool.rs`; the `with_team` toggle; the impl in
`newt-cli` under attenuated caveats) — built remote-ready and caveat-carrying from
the start. Empirical role priors arrive from the rig (#80) and feed the composer.

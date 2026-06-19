# TODO — Newt-Agent near-term

Working scratchpad for the active thread. The durable plan lives in
`docs/ROADMAP.md`; the architecture in `docs/design/crew-swarm-overseer.md`. This
file is the "what's next, in order" view.

_Last updated: 2026-06-18._

## ✅ Just landed — the crew/team/overseer stack (#479 complete)

| What | PR |
|---|---|
| panel (anti-groupthink) | #468 |
| MCP-git dev-kit surface | #469 |
| tiered verify gate + honest cap-exit banner | #470 |
| team orchestrator + per-subtask verify | #474, #477 |
| live team harness (`examples/team_live.rs`) | #475 |
| hosted-LLM dispatch (OpenAI `/v1` + api_key) | #478 |
| roster composer (`/crew-roster`) | #480 |
| architecture doc | #481 |
| agent-callable crew/team tool surface | #482 |
| `LocalCrewRunner` + scheduler-free injection | #484 |
| **23.3 — land verified work as a `crew/<id>` git branch** | #489 |

The overseer loop is live: with `NEWT_TEAM` set, `newt code` can plan → propose a
roster → dispatch crews per step → review the diff → **land verified work on a
branch** (merge with the embedded `git` tool). **2050 tests green.**

## 🔀 Authority convergence (fold the `attest` trilogy in)

The overseer's human gates — "approve the plan / roster" — **are** §7.5's `attest`
decision surface `{allow, attest, deny}`: knowledge#40 §7.5 (principle) · newt-agent#472
(plan) · **agent-bridle#24 (MVP merged)**. Approving a roster *grants a crew authority*
= a policy mutation; §7.5's keystone: **attenuate freely, amplify needs the human root
via `attest`**. So crew authority builds **on** the merged `attest` `Gate`, not env
vars; and `attest` shares #472's root-of-trust bootstrap with provenance (#490).

## 🔜 Next — finish LOCAL workflows (revised by the convergence)

1. **23.1 — per-member caveats ✅ done (#494).** `run_crew`/`run_team` take `&Caveats`
   and refuse out-of-`fs_write` edits at apply (attenuation). Caveats travel with the
   work. _Remaining within 23.1:_ route the shortfall through the bridle Gate's
   `NeedsDischarge` (folds into 23.2).
2. **23.2 — `attest`-gate the team enable (REFRAMED) ← NEXT.** Enabling crew/team tools
   *enlarges authority* → a live human gesture, not `NEWT_TEAM` (kept as a dev escape).
   The `/team` enable + roster-approval become `attest` decisions through agent-bridle#24's
   `Gate` (structure now vs the stub/`Prompt`; real teeth after BOOT). Plus `[team]`
   config + `[crews.*]` selection.
3. **23.4 — empirical priors.** Feed the rig's profiles (#80) into `compose_roster`
   (heuristic today). Independent — anytime.

## 🔑 BOOT — root-of-trust bootstrap (new shared prerequisite, #472)

Passkey **seals** the ed25519 `UserKey` (GitHub anchors identity only). Built **once**;
unblocks BOTH real `attest` enforcement AND provenance. Gated by #472's **four blocking
fixes** (revocation no-op · no proof-of-possession · unsigned fail-open grant · dead
push-gate) + a real WebAuthn verifier + the server-side `pre-receive` hook (the teeth;
the client `Gate` isn't the boundary).

## 🛰️ After BOOT + teeth

- **Provenance plane (#490)** — sign config / commands / prompts; seam = 23.3's
  `commit_to_branch` → sign it with the mesh key.
- **`MeshCrewRunner` (#488)** — remote resident: `CrewTask → CrewResult` over
  agent-mesh; caveats attenuate per hop; remote *amplify* needs `attest`, push-back
  rides the `pre-receive` teeth. A `CrewRunner` swap, not a rewrite. _Do not start
  until BOOT + teeth._

## 🧪 Parallel / independent

- **#80** — re-run the ground-truth rig on a fresh model family (now drivable
  out-of-band via the MCP coder surface + the panel). Produces the priors for 23.4.
- **#73 TUI wiring** — surface `VerifyTier` + `turn_verdict_banner` in the live
  newt-tui turn loop (the newt-core slice landed in #470).

## Guardrails (don't regress)

- Honest gates — verification is the harness's, never the model's self-report.
- Attenuation only — every authority hop `meet`s, never widens.
- Propose-then-act — human approves the plan and the roster.
- newt-tui stays scheduler-free — the binary injects `&dyn CrewRunner` down.
- `just check` + `cov-ci` green; never `--no-verify`.

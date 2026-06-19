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

The overseer loop is live: with `NEWT_TEAM` set, `newt code` can plan → propose a
roster → dispatch crews per step → review the diff. **2048 tests green.**

## 🔜 Next — finish LOCAL workflows before going remote

1. **23.1 — Per-crew-member caveat threading.** `run_crew`/`run_team` pass
   `meet`-attenuated caveats to each crew member. Today's bound is worktree
   isolation + the fail-closed write check in `LocalCrewRunner`; this makes the
   per-member authority real. _(newt-scheduler crew.rs/team.rs + the member tool calls.)_
2. **23.2 — `[team]` config + runtime `/team`.** Replace the `NEWT_TEAM` env gate
   with a `[team] enabled` config section + a `/team` slash command that toggles the
   tool advertisement live; let the `crew` tool select a saved `[crews.<name>]`.
3. **23.3 — Accept / merge-back.** On the overseer's approval, apply the reviewed
   crew diff from the worktree to the live tree (today the diff is review-only).
4. **23.4 — Empirical priors.** Feed the rig's model-family profiles (#80) into
   `compose_roster` so role picks are measured, not name-heuristic.

## 🛰️ After local lands — remote crew

5. **`MeshCrewRunner`** — the remote sibling of `LocalCrewRunner` (own follow-up
   issue). A resident Newt/Wyvern parked on a project links to the swarm and
   receives `CrewTask → CrewResult` over agent-mesh; caveats attenuate per hop.
   _Do not start until 1–4 are solid._

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

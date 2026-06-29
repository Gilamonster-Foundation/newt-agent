//! The crew/team tool seam (#479) — the agent-callable orchestration surface.
//!
//! `newt-scheduler` (the `run_crew`/`run_team`/`compose_roster` machinery) depends
//! on `newt-core`, so `newt-core` can NOT depend on it (circular). The agent loop
//! reaches the orchestrator through the same **trait-injection** seam as
//! [`GitTool`](super::git_tool::GitTool): newt-core defines [`CrewRunner`],
//! `newt-cli` implements it (`LocalCrewRunner`, backed by `run_crew`/`run_team` +
//! a `WorktreeWorkspace`), and the binary injects the impl into
//! [`ChatCtx`](super::ChatCtx) per turn. The tools are advertised only when an
//! impl is injected — the `/team` toggle (presence gate).
//!
//! **OCAP:** a model fielding a crew is the recursion / Confused-Deputy case. The
//! [`CrewRunner`] impl runs every spawned crew under `caveats.meet(crew_clamp)`,
//! so a crew's authority can never **exceed** the session ceiling — it is `≤` the
//! session on every axis (the overseer cannot escalate by dispatching a crew) and
//! is further bounded by `crew_clamp`. The clamp is the operator's / per-subtask
//! tightening point (`[crew] clamp`; it **defaults to `Caveats::top()`**, so the
//! meet is the identity today and the bound is exactly "`≤ session`" until an
//! operator narrows it — #749 step 8). The `/team` toggle is the operator's on/off;
//! the `meet` is the structural bound. See `docs/design/crew-swarm-overseer.md`.

use crate::caveats::Caveats;
use async_trait::async_trait;
use serde_json::Value;

/// The injected crew/team capability. Object-safe and shareable (the loop holds
/// `&dyn CrewRunner`; `Send + Sync` because the borrow lives across `.await`,
/// exactly like [`GitTool`](super::git_tool::GitTool)).
///
/// `op` is one of `compose_roster` | `crew` | `team`; `args` is the tool-call
/// argument object. The implementation **bounds** `caveats` by the meet with a
/// crew clamp (`session ⊓ clamp`, so the crew runs at `≤ session`) and fails
/// closed on the write / exec / attest gates, then returns either a rendered,
/// model-readable result string (`Ok`) — a roster proposal, or a crew's diff +
/// verify status for the overseer to review — or an error string the tool layer
/// surfaces verbatim (`Err`).
///
/// This is the **universal crew primitive**: `LocalCrewRunner` (newt-cli) runs the
/// crew here; a future `MeshCrewRunner` ships the task over agent-mesh; and a
/// **wyvern resident** implements it server-side to receive crew/plan tasks
/// (wyvern-agent#42). Same contract — `(op, args, caveats) → rendered result` —
/// with `caveats` travelling (met with each hop's clamp, so authority is
/// monotone-non-increasing across the wire — never amplified) so authority
/// crosses the wire with the work. `async` because dispatch runs inference, not
/// just local I/O.
#[async_trait]
pub trait CrewRunner: Send + Sync {
    async fn dispatch(&self, op: &str, args: &Value, caveats: &Caveats) -> Result<String, String>;
}

/// `compose_roster` — survey the live environment and **propose** a roster for the
/// human to approve. Advertised only when a [`CrewRunner`] is injected.
pub fn compose_roster_tool_definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "compose_roster",
            "description": "Survey the live models/backends and PROPOSE a crew roster \
                            (which model fills planner/navigator/triage, and whether to \
                            run as a crew, a team, or a decorrelated panel) for the human \
                            to approve. Returns the proposal with a rationale per pick. \
                            Does NOT run anything — propose, then the human approves.",
            "parameters": {
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["crew", "team", "panel"],
                        "description": "crew = roles divide one task; team = a lead \
                                        decomposes a goal then a crew runs each subtask; \
                                        panel = N decorrelated voices on the same task."
                    },
                    "goal": {
                        "type": "string",
                        "description": "Optional: the goal/plan the roster is being composed for (context)."
                    }
                },
                "required": ["mode"]
            }
        }
    })
}

/// `crew` — dispatch a crew/roster on a single task (or a whole plan via a team),
/// returning the **diff + verify status** for the overseer to review.
pub fn crew_tool_definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "crew",
            "description": "Dispatch a crew on a task UNDER YOUR OVERSIGHT: it edits an \
                            isolated workspace, runs the verification, and returns the diff \
                            + status for you to review and accept or re-dispatch. Compose a \
                            roster first (compose_roster) or name a saved crew. Crews run \
                            under your authority met with the crew clamp — they can never \
                            exceed your authority (their permissions are <= yours).",
            "parameters": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The single task (one plan step) for the crew to land."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["crew", "team"],
                        "description": "crew (default) runs one task; team decomposes the \
                                        task as a goal and runs a crew per subtask."
                    },
                    "crew": {
                        "type": "string",
                        "description": "Optional: the name of a saved [crews.<name>] roster to field."
                    },
                    "verify": {
                        "type": "string",
                        "description": "Optional: the shell command that verifies this task is done."
                    }
                },
                "required": ["task"]
            }
        }
    })
}

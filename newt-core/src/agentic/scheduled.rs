//! Scheduled context — the `scheduled` context feature (Step 26.6b, #586).
//!
//! A "compiled per-step view": instead of leaning on an ever-growing chat
//! buffer, the model keeps an ORDERED plan of steps (each Todo / Active / Done)
//! and a compiled `<plan>` checklist is injected at the head of every turn. The
//! model maintains it with `update_plan` (send the full ordered list each time,
//! each step tagged pending/in_progress/completed), so it stays oriented around
//! its plan's progress rather than re-deriving it from a long transcript.
//!
//! Pure in-memory + deterministic (a `Vec`, no clock/uuid), so the whole feature
//! unit-tests with zero network/fs. Mirrors `scratchpad.rs`: a `&self`
//! interior-mutability trait + an in-memory impl. The plan is task-specific →
//! cleared on `/new`.
//!
//! Out of scope (a future enhancement): actually RESETTING the rolling window
//! around the compiled view. v1 injects the `<plan>` view alongside the normal
//! window rather than replacing it — honest + non-invasive.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// A plan step's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Todo,
    Active,
    Done,
}

/// One step in the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub description: String,
    pub status: StepStatus,
}

/// A serializable full-state snapshot of the plan ledger (#715). Persisted onto
/// the conversation row so an interrupt + auto-resume can re-hydrate the
/// in-memory ledger EXACTLY — the ordered steps AND each step's done/active
/// status — instead of rebuilding it empty (which loses the `<plan>` block and
/// `plan_get` after a resume). The active step is encoded by its
/// [`StepStatus::Active`] marker, so the snapshot carries no separate index to
/// drift out of sync.
///
/// Unlike [`StepLedger::set_plan`] (which takes bare step strings and RESETS
/// every status — the first step Active, the rest Todo), a snapshot/restore
/// round-trip preserves which steps are Done and which is Active.
///
/// Additive + **working memory, not provenance**: it rides the conversation row
/// (never a turn), is kept OUT of the §6 content chain, and an empty snapshot is
/// skipped on serialize so it never bloats the on-disk file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSnapshot {
    /// The ordered steps with each one's status. `#[serde(default)]` so the
    /// historically-true empty backfill (`{}`) on an older db parses cleanly.
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl PlanSnapshot {
    /// Whether the snapshot holds no steps (the OFF/empty case — used by the
    /// record's `skip_serializing_if` and the resume banner).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
    /// Number of steps in the snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }
}

/// Max steps a plan may hold (a guard; extra steps are dropped on set).
pub(crate) const MAX_STEPS: usize = 30;
/// Per-step description cap in the `<plan>` view.
pub(crate) const STEP_DESC_CAP: usize = 200;
/// Whole-block char cap so a long plan can never blow the send budget.
pub(crate) const PLAN_TOTAL_CAP: usize = 3_000;

/// A session store for the ordered plan (Step 26.6b). `&self` methods (interior
/// mutability) so one shared `&dyn StepLedger` serves both the per-turn `<plan>`
/// injection and the update_plan tool. Task-specific → cleared on `/new`.
pub trait StepLedger: Send + Sync {
    /// Replace the plan with a fresh ordered list — the first step becomes
    /// Active, the rest Todo. Blank descriptions are dropped; capped at
    /// [`MAX_STEPS`]. Returns the number of steps actually set.
    fn set_plan(&self, steps: &[String]) -> usize;
    /// Mark the Active step Done and activate the next Todo. Returns the new
    /// active step's description, or `None` when the plan is complete/empty.
    fn advance(&self) -> Option<String>;
    /// A snapshot of the steps (for the `<plan>` block).
    fn steps(&self) -> Vec<Step>;
    /// Total steps (for `/context stats`).
    fn count(&self) -> u64;
    /// Completed steps (for `/context stats`).
    fn done_count(&self) -> u64;
    /// Drop the plan (`/new`).
    fn clear(&self);
    /// A full-state [`PlanSnapshot`] for persistence/resume (#715): the ordered
    /// steps with each one's status, so a later [`Self::restore`] reinstates the
    /// ledger EXACTLY (unlike [`Self::set_plan`], which resets every status).
    fn snapshot(&self) -> PlanSnapshot;
    /// Restore the ledger from a [`PlanSnapshot`] (#715) — replaces the steps
    /// and their statuses verbatim (a full clear + replace, the inverse of
    /// [`Self::snapshot`]). An empty snapshot leaves the ledger empty.
    fn restore(&self, snap: &PlanSnapshot);
}

/// In-memory, session-scoped [`StepLedger`] — pure (no fs). A `Vec` in plan
/// order; deterministic (no clock/uuid) for stable tests.
#[derive(Default)]
pub struct SessionStepLedger {
    steps: Mutex<Vec<Step>>,
}

impl StepLedger for SessionStepLedger {
    fn set_plan(&self, steps: &[String]) -> usize {
        let mut built: Vec<Step> = steps
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .take(MAX_STEPS)
            .map(|s| Step {
                description: s.to_string(),
                status: StepStatus::Todo,
            })
            .collect();
        if let Some(first) = built.first_mut() {
            first.status = StepStatus::Active;
        }
        let n = built.len();
        *self.steps.lock().unwrap() = built;
        n
    }

    fn advance(&self) -> Option<String> {
        let mut steps = self.steps.lock().unwrap();
        // Finish the current Active step (if any).
        if let Some(active) = steps.iter_mut().find(|s| s.status == StepStatus::Active) {
            active.status = StepStatus::Done;
        }
        // Activate the next Todo, if one remains.
        if let Some(next) = steps.iter_mut().find(|s| s.status == StepStatus::Todo) {
            next.status = StepStatus::Active;
            Some(next.description.clone())
        } else {
            None
        }
    }

    fn steps(&self) -> Vec<Step> {
        self.steps.lock().unwrap().clone()
    }
    fn count(&self) -> u64 {
        self.steps.lock().unwrap().len() as u64
    }
    fn done_count(&self) -> u64 {
        self.steps
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.status == StepStatus::Done)
            .count() as u64
    }
    fn clear(&self) {
        self.steps.lock().unwrap().clear();
    }
    fn snapshot(&self) -> PlanSnapshot {
        PlanSnapshot {
            steps: self.steps.lock().unwrap().clone(),
        }
    }
    fn restore(&self, snap: &PlanSnapshot) {
        *self.steps.lock().unwrap() = snap.steps.clone();
    }
}

fn truncate(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let head: String = s.chars().take(cap).collect();
        format!("{head}[…]")
    }
}

/// Render the compiled `<plan>` checklist injected at the head of a turn (Step
/// 26.6b). `None` when the plan is empty — the OFF/empty bit-for-bit guarantee
/// (mirror `scratchpad::build_state_block`). `✓` done, `→` active, `☐` todo.
pub(crate) fn build_plan_block(ledger: &dyn StepLedger, total_cap: usize) -> Option<String> {
    let steps = ledger.steps();
    if steps.is_empty() {
        return None;
    }
    let mut body = String::from("<plan>\n");
    for (i, step) in steps.iter().enumerate() {
        let mark = match step.status {
            StepStatus::Done => "✓",
            StepStatus::Active => "→",
            StepStatus::Todo => "☐",
        };
        let piece = format!(
            "{mark} {}. {}\n",
            i + 1,
            truncate(&step.description, STEP_DESC_CAP)
        );
        if body.chars().count() + piece.chars().count() + "</plan>".len() > total_cap {
            body.push_str("[… plan truncated to fit the budget …]\n");
            break;
        }
        body.push_str(&piece);
    }
    body.push_str("</plan>");
    Some(body)
}

/// TUI-facing entry: the compiled `<plan>` block with the default cap (Step
/// 26.6b). `None` when the plan is empty.
pub fn plan_block(ledger: &dyn StepLedger) -> Option<String> {
    build_plan_block(ledger, PLAN_TOTAL_CAP)
}

/// A COMPACT per-round re-seat pointer — the active step + progress on one line.
///
/// `None` unless a MULTI-STEP plan is in progress (≥2 steps and an Active step
/// remains). This is the conditional re-seat validated by the dgx1 12-step A/B
/// (re-seat **12/12** vs baseline **8/12** under drift; see
/// `docs/research/weak-model-plan-mode-findings.md`): the round-0 `<plan>`
/// snapshot goes stale across the up-to-N tool rounds, so a weak model "loses
/// track of its plan over time". Re-showing only the active step each round keeps
/// it on course at a fraction of the token cost of re-injecting the full `<plan>`
/// (which mildly *hurt* on no-drift tasks). Gated to multi-step in-progress plans
/// so trivial/single-step turns pay nothing.
#[must_use]
pub fn plan_reseat_pointer(ledger: &dyn StepLedger) -> Option<String> {
    let steps = ledger.steps();
    if steps.len() < 2 {
        return None; // no plan, or a single-step plan that needs no re-seat
    }
    let active = steps.iter().position(|s| s.status == StepStatus::Active)?;
    let done = steps
        .iter()
        .filter(|s| s.status == StepStatus::Done)
        .count();
    Some(format!(
        "[Plan progress: {done}/{} done. ACTIVE \u{2192} step {}: {}. Keep working \
         THIS step; earlier steps are already done — do not restart or re-plan them.]",
        steps.len(),
        active + 1,
        truncate(&steps[active].description, STEP_DESC_CAP)
    ))
}

// ---------------------------------------------------------------------------
// Tool schemas (advertised only when the feature is on + a ledger is present)
// ---------------------------------------------------------------------------

/// `update_plan` tool definition (#715 PR2) — the single WRITE surface that
/// replaces `plan_set` + `plan_advance`. The Codex shape: send the full ordered
/// list each turn, each step tagged with its status (exactly one `in_progress`).
pub fn update_plan_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "update_plan",
            "description": "Create or update your plan — send the full ordered list each time \
                            with each step's status. A <plan> checklist is shown at the head of \
                            every turn; mark the step you are on \"in_progress\" and finished \
                            steps \"completed\". Prefer this before more investigation for \
                            multi-step, ambiguous, resumed, or context-compacted work. Replaces \
                            plan_set/plan_advance.",
            "parameters": {
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {
                                    "type": "string",
                                    "description": "A short imperative phrase for the step."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "The step's progress; exactly one step \
                                                    should be in_progress."
                                }
                            },
                            "required": ["step", "status"]
                        },
                        "description": "The ordered steps, each with its status."
                    }
                },
                "required": ["plan"]
            }
        }
    })
}

/// `plan_get` tool definition (#716) — a read-only read of the current plan.
pub fn plan_get_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "plan_get",
            "description": "Read the current <plan> checklist — the ordered steps and which is \
                            active. No args. Use it to recover what you were working on after a \
                            resume; if it reports no active plan, create one with update_plan \
                            instead of calling plan_get again.",
            "parameters": { "type": "object", "properties": {}, "required": [] }
        }
    })
}

/// #1193: `enter_plan_mode` — flip the session into the READ-ONLY plan phase.
pub fn enter_plan_mode_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "enter_plan_mode",
            "description": "Enter PLAN MODE: a read-only phase for figuring out WHAT to do before                             doing it. The clamp applies immediately to subsequent tool calls:                             edit_file, write_file, shell execution, and network access are denied,                             while reads and update_plan remain available. Use this when a task needs                             several steps: read the relevant code, call update_plan with the ordered                             steps, then call exit_plan_mode to start executing. No args.",
            "parameters": { "type": "object", "properties": {}, "required": [] }
        }
    })
}

/// #1193: `exit_plan_mode` — leave the model-entered plan phase.
pub fn exit_plan_mode_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "exit_plan_mode",
            "description": "Leave the model-entered PLAN PHASE. Subsequent tool calls return to this turn's validated disposition and underlying session permissions; the next outer turn returns to the human-selected operating mode. This cannot override `/mode plan` or another read-only clamp. Call this once the update_plan plan is ready. No args.",
            "parameters": { "type": "object", "properties": {}, "required": [] }
        }
    })
}

// ---------------------------------------------------------------------------
// Executors (every branch returns a tool-result String, never a loop abort)
// ---------------------------------------------------------------------------

/// Map a tool-supplied `status` string onto a [`StepStatus`]. Lenient: the
/// schema's canonical `pending`/`in_progress`/`completed` plus their obvious
/// synonyms; anything unrecognized (or missing) falls back to `Todo` so a noisy
/// status never aborts the call.
fn parse_step_status(raw: &str) -> StepStatus {
    match raw.trim().to_ascii_lowercase().as_str() {
        "completed" | "complete" | "done" => StepStatus::Done,
        "in_progress" | "in-progress" | "active" | "current" => StepStatus::Active,
        _ => StepStatus::Todo, // "pending" / "todo" / unknown
    }
}

/// Enforce the Codex invariant of EXACTLY ONE active step — leniently. If the
/// model sent no `in_progress` (or several), the first non-completed step is
/// made active and any other actives demoted to Todo. An all-completed plan is
/// left with no active step (the plan is done).
fn normalize_active(steps: &mut [Step]) {
    let active = steps
        .iter()
        .filter(|s| s.status == StepStatus::Active)
        .count();
    if active == 1 {
        return; // the well-formed case — respect the model's choice
    }
    for s in steps.iter_mut() {
        if s.status == StepStatus::Active {
            s.status = StepStatus::Todo;
        }
    }
    if let Some(first) = steps.iter_mut().find(|s| s.status != StepStatus::Done) {
        first.status = StepStatus::Active;
    }
}

/// Execute an `update_plan` call (#715 PR2) — the single WRITE surface. Parses
/// the `{plan:[{step,status}]}` array into a [`PlanSnapshot`] and reinstates the
/// ledger via [`StepLedger::restore`] (the same full-state path persistence uses,
/// so statuses land verbatim). Returns the rendered `<plan>` block so the model
/// sees the result; read it back later with `plan_get`.
pub(crate) fn execute_update_plan(
    args: &serde_json::Value,
    ledger: &dyn StepLedger,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    let out = match args["plan"].as_array() {
        None => "error: update_plan requires a `plan` array of {\"step\",\"status\"} objects"
            .to_string(),
        Some(items) => {
            let mut steps: Vec<Step> = items
                .iter()
                .filter_map(|it| {
                    let desc = it.get("step").and_then(|v| v.as_str())?.trim();
                    if desc.is_empty() {
                        return None;
                    }
                    let status = it
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map_or(StepStatus::Todo, parse_step_status);
                    Some(Step {
                        description: desc.to_string(),
                        status,
                    })
                })
                .take(MAX_STEPS)
                .collect();
            if steps.is_empty() {
                "error: update_plan requires a non-empty `plan` array".to_string()
            } else {
                normalize_active(&mut steps);
                ledger.restore(&PlanSnapshot { steps });
                plan_block(ledger).unwrap_or_else(|| "plan updated".to_string())
            }
        }
    };
    out
}

/// Execute a `plan_get` call (#716) — render the current `<plan>` checklist
/// read-only (no ledger mutation), so a resumed turn can recover "what was I
/// working on". Empty plan → a hint to start one with `update_plan`.
pub(crate) fn execute_plan_get(
    ledger: &dyn StepLedger,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    plan_block(ledger).unwrap_or_else(|| {
        "no active plan — if this is multi-step, ambiguous, resumed, or context-compacted work, \
         call update_plan next with a short 2-6 step ordered plan using statuses \
         pending/in_progress/completed; do not call plan_get again until you have created or \
         updated a plan"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_plan_filters_blanks_caps_and_activates_first() {
        let l = SessionStepLedger::default();
        let n = l.set_plan(&[
            "  read the code  ".to_string(),
            "".to_string(),
            "  ".to_string(),
            "write the fix".to_string(),
        ]);
        assert_eq!(n, 2, "blank steps dropped");
        let steps = l.steps();
        assert_eq!(steps[0].description, "read the code", "trimmed");
        assert_eq!(steps[0].status, StepStatus::Active, "first is active");
        assert_eq!(steps[1].status, StepStatus::Todo);
        // cap at MAX_STEPS
        let many: Vec<String> = (0..MAX_STEPS + 10).map(|i| format!("step {i}")).collect();
        assert_eq!(l.set_plan(&many), MAX_STEPS, "capped at MAX_STEPS");
        // clear empties the plan (/new path)
        l.clear();
        assert_eq!(l.count(), 0);
        assert!(l.steps().is_empty());
    }

    #[test]
    fn advance_walks_active_to_done_and_reports_complete() {
        let l = SessionStepLedger::default();
        l.set_plan(&["a".to_string(), "b".to_string()]);
        assert_eq!(l.done_count(), 0);
        // advance: a → Done, b → Active, returns "b"
        assert_eq!(l.advance().as_deref(), Some("b"));
        let steps = l.steps();
        assert_eq!(steps[0].status, StepStatus::Done);
        assert_eq!(steps[1].status, StepStatus::Active);
        assert_eq!(l.done_count(), 1);
        // advance again: b → Done, no Todo left → None (complete)
        assert_eq!(l.advance(), None);
        assert_eq!(l.done_count(), 2);
        // advancing a complete plan stays None, no panic
        assert_eq!(l.advance(), None);
        // advancing an empty plan → None
        let empty = SessionStepLedger::default();
        assert_eq!(empty.advance(), None);
    }

    #[test]
    fn snapshot_restore_round_trips_full_state_unlike_set_plan() {
        // #715: a snapshot captures the FULL state (steps + per-step status), so
        // restore reinstates which steps are Done / Active — the resume-safety
        // property set_plan lacks (it resets every status).
        let l = SessionStepLedger::default();
        l.set_plan(&["a".to_string(), "b".to_string(), "c".to_string()]);
        l.advance(); // a → Done, b → Active
        let snap = l.snapshot();
        assert_eq!(snap.len(), 3);
        assert!(!snap.is_empty());
        assert_eq!(snap.steps[0].status, StepStatus::Done);
        assert_eq!(snap.steps[1].status, StepStatus::Active);
        assert_eq!(snap.steps[2].status, StepStatus::Todo);

        // It survives a serde round-trip (the persistence path).
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: PlanSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, snap, "snapshot must survive serde verbatim");
        // The `{}` backfill (an older db / OFF feature) parses to an empty plan.
        assert_eq!(
            serde_json::from_str::<PlanSnapshot>("{}").unwrap(),
            PlanSnapshot::default(),
            "the `{{}}` backfill parses to an empty snapshot"
        );

        // Restore into a FRESH ledger reinstates the exact state — not a reset.
        let fresh = SessionStepLedger::default();
        fresh.restore(&decoded);
        assert_eq!(fresh.snapshot(), snap, "restore reinstates verbatim");
        assert_eq!(fresh.done_count(), 1, "the Done step survives restore");
        let block = build_plan_block(&fresh, 3000).unwrap();
        assert!(block.contains("✓ 1. a"), "{block}");
        assert!(block.contains("→ 2. b"), "{block}");
        assert!(block.contains("☐ 3. c"), "{block}");
        // Contrast: set_plan would have reset b/c to Active/Todo and lost a's Done.
        let reset = SessionStepLedger::default();
        reset.set_plan(&["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(reset.done_count(), 0, "set_plan resets — proving the gap");

        // Restoring an empty snapshot clears (a conversation boundary).
        fresh.restore(&PlanSnapshot::default());
        assert_eq!(
            fresh.count(),
            0,
            "an empty snapshot leaves the ledger empty"
        );
    }

    #[test]
    fn reseat_pointer_compact_and_gated_to_multistep_in_progress() {
        let l = SessionStepLedger::default();
        assert!(plan_reseat_pointer(&l).is_none(), "empty → none");
        l.set_plan(&["only".to_string()]);
        assert!(plan_reseat_pointer(&l).is_none(), "single step → none");
        l.set_plan(&["a".to_string(), "b".to_string(), "c".to_string()]);
        let p = plan_reseat_pointer(&l).expect("multi-step → pointer");
        assert!(p.contains("step 1: a"), "names the active step: {p}");
        assert!(p.contains("0/3 done"), "shows progress: {p}");
        assert_eq!(p.lines().count(), 1, "compact (one line): {p}");
        l.advance(); // a Done, b Active
        let ptr = plan_reseat_pointer(&l).unwrap();
        assert!(
            ptr.contains("step 2: b") && ptr.contains("1/3 done"),
            "{ptr}"
        );
        l.advance();
        l.advance(); // all done, no Active
        assert!(plan_reseat_pointer(&l).is_none(), "complete plan → none");
    }

    #[test]
    fn build_block_none_when_empty_marks_status_and_caps() {
        let l = SessionStepLedger::default();
        assert_eq!(build_plan_block(&l, 3000), None, "empty plan → no block");
        l.set_plan(&["alpha".to_string(), "beta".to_string(), "gamma".to_string()]);
        l.advance(); // alpha Done, beta Active
        let block = build_plan_block(&l, 3000).unwrap();
        assert!(block.starts_with("<plan>\n") && block.ends_with("</plan>"));
        assert!(block.contains("✓ 1. alpha"), "{block}");
        assert!(block.contains("→ 2. beta"), "{block}");
        assert!(block.contains("☐ 3. gamma"), "{block}");
        // a single over-long step description is truncated with the […] marker
        let long = SessionStepLedger::default();
        long.set_plan(&["x".repeat(STEP_DESC_CAP + 50)]);
        let lb = build_plan_block(&long, 3000).unwrap();
        assert!(
            lb.contains("[…]"),
            "per-step description truncated: {lb:.80}"
        );
        // total cap trips the marker
        let big = SessionStepLedger::default();
        let many: Vec<String> = (0..MAX_STEPS)
            .map(|i| format!("step number {i} with padding"))
            .collect();
        big.set_plan(&many);
        let capped = build_plan_block(&big, 200).unwrap();
        assert!(
            capped.chars().count() <= 200 + 60,
            "total cap bounds the block"
        );
        assert!(capped.contains("plan truncated"), "{capped}");
    }

    #[test]
    fn update_plan_sets_statuses_renders_block_and_is_lenient() {
        // #715 PR2: update_plan is the single WRITE surface ({plan:[{step,status}]}).
        let l = SessionStepLedger::default();
        // missing `plan` → coaching error, ledger untouched
        assert!(execute_update_plan(&serde_json::json!({}), &l, false, 20).starts_with("error:"));
        assert_eq!(l.count(), 0);
        // empty array → error, still untouched
        assert!(
            execute_update_plan(&serde_json::json!({"plan": []}), &l, false, 20)
                .starts_with("error:")
        );
        assert_eq!(l.count(), 0);
        // a full plan with one in_progress → statuses land in the ledger AND the
        // rendered <plan> block comes back.
        let out = execute_update_plan(
            &serde_json::json!({"plan": [
                {"step": "scope it", "status": "completed"},
                {"step": "build it", "status": "in_progress"},
                {"step": "test it",  "status": "pending"},
            ]}),
            &l,
            false,
            20,
        );
        assert!(
            out.starts_with("<plan>\n") && out.ends_with("</plan>"),
            "{out}"
        );
        assert!(out.contains("✓ 1. scope it"), "{out}");
        assert!(out.contains("→ 2. build it"), "{out}");
        assert!(out.contains("☐ 3. test it"), "{out}");
        let steps = l.steps();
        assert_eq!(steps[0].status, StepStatus::Done);
        assert_eq!(steps[1].status, StepStatus::Active);
        assert_eq!(steps[2].status, StepStatus::Todo);
        assert_eq!(
            steps
                .iter()
                .filter(|s| s.status == StepStatus::Active)
                .count(),
            1,
            "exactly one in_progress"
        );
        // lenient: NO in_progress → the first non-completed becomes active.
        let none_active = execute_update_plan(
            &serde_json::json!({"plan": [
                {"step": "a", "status": "completed"},
                {"step": "b", "status": "pending"},
                {"step": "c", "status": "pending"},
            ]}),
            &l,
            false,
            20,
        );
        assert!(
            none_active.contains("→ 2. b"),
            "first non-completed becomes active: {none_active}"
        );
        // lenient: MULTIPLE in_progress → only the first non-completed stays active.
        execute_update_plan(
            &serde_json::json!({"plan": [
                {"step": "x", "status": "in_progress"},
                {"step": "y", "status": "in_progress"},
            ]}),
            &l,
            false,
            20,
        );
        let s = l.steps();
        assert_eq!(
            s.iter().filter(|x| x.status == StepStatus::Active).count(),
            1,
            "multiple in_progress collapse to one"
        );
        assert_eq!(s[0].status, StepStatus::Active, "the first becomes active");
        // blank step descriptions are dropped (the set_plan filtering parity).
        let dropped = execute_update_plan(
            &serde_json::json!({"plan": [
                {"step": "  ", "status": "pending"},
                {"step": "real", "status": "in_progress"},
            ]}),
            &l,
            false,
            20,
        );
        assert!(dropped.contains("→ 1. real"), "{dropped}");
        assert_eq!(l.count(), 1, "blank step dropped");
    }

    #[test]
    fn tool_definitions_shape() {
        assert_eq!(
            update_plan_tool_definition()["function"]["name"],
            "update_plan"
        );
        assert_eq!(plan_get_tool_definition()["function"]["name"], "plan_get");
        // update_plan takes the `plan` array of {step,status} objects.
        assert!(
            update_plan_tool_definition()["function"]["parameters"]["properties"]["plan"]
                .is_object()
        );
        // #716: plan_get is read-only — no args.
        assert_eq!(
            plan_get_tool_definition()["function"]["parameters"]["properties"],
            serde_json::json!({})
        );
    }

    #[test]
    fn plan_get_renders_plan_or_hints_when_empty() {
        // #716: empty ledger → a hint to start a plan (no mutation).
        let l = SessionStepLedger::default();
        let empty = execute_plan_get(&l, false, 20);
        assert!(empty.starts_with("no active plan"), "{empty}");
        assert!(empty.contains("update_plan next"), "{empty}");
        assert!(
            empty.contains("do not call plan_get again"),
            "empty plan_get must steer away from polling: {empty}"
        );
        assert_eq!(l.count(), 0, "plan_get does not mutate the ledger");
        // a ledger with steps → the compiled <plan> block, read-only.
        l.set_plan(&["scope it".to_string(), "build it".to_string()]);
        let block = execute_plan_get(&l, false, 20);
        assert!(
            block.starts_with("<plan>\n") && block.ends_with("</plan>"),
            "{block}"
        );
        assert!(block.contains("→ 1. scope it"), "{block}");
        assert!(block.contains("☐ 2. build it"), "{block}");
        assert_eq!(l.count(), 2, "plan_get is read-only");
    }
}

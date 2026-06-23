//! Scheduled context — the `scheduled` context feature (Step 26.6b, #586).
//!
//! A "compiled per-step view": instead of leaning on an ever-growing chat
//! buffer, the model keeps an ORDERED plan of steps (each Todo / Active / Done)
//! and a compiled `<plan>` checklist is injected at the head of every turn. The
//! model maintains it with `plan_set` (compile/replace the plan) and
//! `plan_advance` (finish the active step, activate the next), so it stays
//! oriented around its plan's progress rather than re-deriving it from a long
//! transcript.
//!
//! Pure in-memory + deterministic (a `Vec`, no clock/uuid), so the whole feature
//! unit-tests with zero network/fs. Mirrors `scratchpad.rs`: a `&self`
//! interior-mutability trait + an in-memory impl. The plan is task-specific →
//! cleared on `/new`.
//!
//! Out of scope (a future enhancement): actually RESETTING the rolling window
//! around the compiled view. v1 injects the `<plan>` view alongside the normal
//! window rather than replacing it — honest + non-invasive.

use super::display::{print_tool_call, print_tool_output};
use std::sync::Mutex;

/// A plan step's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Todo,
    Active,
    Done,
}

/// One step in the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub description: String,
    pub status: StepStatus,
}

/// Max steps a plan may hold (a guard; extra steps are dropped on set).
pub(crate) const MAX_STEPS: usize = 30;
/// Per-step description cap in the `<plan>` view.
pub(crate) const STEP_DESC_CAP: usize = 200;
/// Whole-block char cap so a long plan can never blow the send budget.
pub(crate) const PLAN_TOTAL_CAP: usize = 3_000;

/// A session store for the ordered plan (Step 26.6b). `&self` methods (interior
/// mutability) so one shared `&dyn StepLedger` serves both the per-turn `<plan>`
/// injection and the plan_set/plan_advance tools. Task-specific → cleared on
/// `/new`.
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

// ---------------------------------------------------------------------------
// Tool schemas (advertised only when the feature is on + a ledger is present)
// ---------------------------------------------------------------------------

/// `plan_set` tool definition.
pub fn plan_set_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "plan_set",
            "description": "Compile (or recompile) your plan as an ORDERED list of steps. The \
                            first becomes the active step and a <plan> checklist is shown at \
                            the head of every turn — work the active step, then call \
                            plan_advance. Re-call plan_set to revise the whole plan.",
            "parameters": {
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "The steps in order, each a short imperative phrase."
                    }
                },
                "required": ["steps"]
            }
        }
    })
}

/// `plan_advance` tool definition.
pub fn plan_advance_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "plan_advance",
            "description": "Mark the active step DONE and activate the next one. Call it when \
                            you finish a step; the <plan> checklist updates next turn.",
            "parameters": { "type": "object", "properties": {}, "required": [] }
        }
    })
}

// ---------------------------------------------------------------------------
// Executors (every branch returns a tool-result String, never a loop abort)
// ---------------------------------------------------------------------------

/// Execute a `plan_set` call (Step 26.6b).
pub(crate) fn execute_plan_set(
    args: &serde_json::Value,
    ledger: &dyn StepLedger,
    color: bool,
    tool_output_lines: usize,
) -> String {
    print_tool_call("plan_set", "", color);
    let steps: Vec<String> = args["steps"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let out = if steps.iter().all(|s| s.trim().is_empty()) {
        "error: plan_set requires a non-empty `steps` array".to_string()
    } else {
        format!("plan set: {} steps", ledger.set_plan(&steps))
    };
    print_tool_output(&out, tool_output_lines, color);
    out
}

/// Execute a `plan_advance` call (Step 26.6b).
pub(crate) fn execute_plan_advance(
    ledger: &dyn StepLedger,
    color: bool,
    tool_output_lines: usize,
) -> String {
    print_tool_call("plan_advance", "", color);
    let out = match ledger.advance() {
        Some(next) => format!("advanced — now active: {next}"),
        None => "advanced — plan complete (no steps remaining)".to_string(),
    };
    print_tool_output(&out, tool_output_lines, color);
    out
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
    fn executors_set_advance_and_coach() {
        let l = SessionStepLedger::default();
        // set: empty array → coaching, plan untouched
        assert!(
            execute_plan_set(&serde_json::json!({"steps": []}), &l, false, 20)
                .starts_with("error:")
        );
        assert_eq!(l.count(), 0);
        // set: real steps → count reported
        assert_eq!(
            execute_plan_set(
                &serde_json::json!({"steps": ["scope it", "build it", "test it"]}),
                &l,
                false,
                20
            ),
            "plan set: 3 steps"
        );
        // advance: reports the new active step, then completion
        assert_eq!(
            execute_plan_advance(&l, false, 20),
            "advanced — now active: build it"
        );
        assert_eq!(
            execute_plan_advance(&l, false, 20),
            "advanced — now active: test it"
        );
        assert!(execute_plan_advance(&l, false, 20).contains("plan complete"));
    }

    #[test]
    fn tool_definitions_shape() {
        assert_eq!(plan_set_tool_definition()["function"]["name"], "plan_set");
        assert_eq!(
            plan_advance_tool_definition()["function"]["name"],
            "plan_advance"
        );
        assert!(
            plan_set_tool_definition()["function"]["parameters"]["properties"]["steps"].is_object()
        );
    }
}

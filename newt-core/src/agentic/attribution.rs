//! Attribution bookkeeping for the agentic turn loop.
//!
//! `AttributionEpoch` is created per tool call: it snapshots commit telemetry and
//! consumes the accumulated contributors only after a confirmed commit. The other
//! helpers here record notes, build warnings and phantom reaches into their own
//! sinks; the loop supplies the sink, the ledger and the values.

use super::is_read_only_call;
use super::lexical_normalize;
use super::GitTool;
use super::NoteNudge;
use super::NoteSink;
use crate::agentic::{compress, smart_harness, tools};

/// Record organic save_note use after repeat rejection and before dispatch.
/// Reset even if the subsequent attempt fails or is cancelled; retain the
/// optional sink/nudge gates.
pub(super) fn record_organic_note_use(
    name: &str,
    note_sink: &Option<&mut dyn NoteSink>,
    note_nudge: &mut Option<&mut NoteNudge>,
) {
    if name == "save_note" && note_sink.is_some() {
        if let Some(n) = note_nudge.as_deref_mut() {
            n.note_saved();
        }
    }
}

/// #1948: observe completed calls and append at most one clean-build warning
/// per turn. Keep this after repeat recording and before event timing.
pub(super) fn append_clean_build_warning(
    messages: &mut Vec<serde_json::Value>,
    clean_build: &mut crate::loop_watch::CleanBuildWatch,
    args: &serde_json::Value,
    harness: Option<&smart_harness::SmartHarness>,
) -> anyhow::Result<()> {
    if let Some(warning) = clean_build.observe(
        args.get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    ) {
        let text = format!("{} {warning}", compress::LOOP_GUIDANCE_PREFIX);
        if let Some(harness) = harness {
            harness.host_message(&text)?;
        }
        messages.push(serde_json::json!({"role":"user", "content":text}));
    }
    Ok(())
}

/// Record a completed call only when the event sink is present. Keep elapsed
/// sampling here, after backend bookkeeping and before phantom classification.
pub(super) fn record_completed_tool_event(
    tool_events: &mut Option<&mut Vec<crate::ToolEvent>>,
    name: &str,
    args: &serde_json::Value,
    ok: bool,
    // #2315: required, so no funnel can record a shell call without deciding
    // what its execution slot holds.
    execution: Option<crate::ExecOutcome>,
    tool_t0: std::time::Instant,
) {
    if let Some(rec) = tool_events.as_deref_mut() {
        rec.push(crate::ToolEvent {
            execution,
            ..crate::ToolEvent::from_call(
                name,
                args,
                ok,
                u64::try_from(tool_t0.elapsed().as_millis()).ok(),
            )
        });
    }
}

/// #717 / #479 (G4): record alias, hallucination, and real-tool empty-miss
/// reaches, falling back to gated-off crew/team reaches. Classify only when
/// the phantom sink is present; preserve the primary classifier's precedence.
pub(super) fn record_phantom_reach(
    phantom_reaches: &mut Option<&mut Vec<crate::PhantomReach>>,
    name: &str,
    args: &serde_json::Value,
    result: &str,
    ok: bool,
    advertise_team: bool,
) {
    if let Some(pr) = phantom_reaches.as_deref_mut() {
        if let Some(resolution) = tools::classify_phantom_reach(name, args, result, ok)
            .or_else(|| tools::classify_gated_off_reach(name, advertise_team))
        {
            pr.push(crate::PhantomReach {
                name_as_called: name.to_string(),
                resolution,
                active_context_features: Vec::new(),
            });
        }
    }
}

/// retry technique (R2 action arm): before a `write_file`/`edit_file` is dispatched,
/// capture the target's pre-write bytes into the turn ledger. Recorded at the loop's
/// dispatch site (not inside `execute_tool`) so the seam stays narrow and only the
/// two file-writing tools are ever tracked. [`WriteLedger::note_before_write`] is
/// idempotent on the turn's first write of a path, so the *pre-turn* state is what
/// is preserved. A no-op when no ledger is lent — every headless caller and any
/// profile without `retry` — so behavior is bit-for-bit unchanged there.
pub(super) fn ledger_note_write(
    write_ledger: Option<&std::cell::RefCell<crate::verify_gate::WriteLedger>>,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
) {
    let Some(led) = write_ledger else {
        return;
    };
    if name != "write_file" && name != "edit_file" {
        return;
    }
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        // Key on the lexically-normalized path so a raw model path like
        // `examples/../foo.py` matches the gate's filesystem-normalized `foo.py`
        // and the revert lookup actually hits — otherwise a non-normalized path
        // would silently evade revert (the fabrication persists, the gate is gamed).
        let abs = lexical_normalize(&std::path::Path::new(workspace).join(p));
        led.borrow_mut().note_before_write(abs);
    }
}

/// #1707/#1709: record `model` as a contributor to the pending commit
/// attribution ledger when this just-completed call `ok` and was not a
/// read-only query — reusing [`is_read_only_call`], the SAME classifier that
/// already decides `round_wrote` for read-only-round steering, rather than
/// re-deriving a second "is this a write" predicate. Called AFTER dispatch
/// (unlike [`ledger_note_write`], which snapshots BEFORE for revert
/// purposes): a denied or failed call materially changed nothing and must
/// not be credited.
pub(super) fn ledger_note_attribution(
    attribution: Option<&std::cell::RefCell<crate::attribution::AttributionLedger>>,
    model: &str,
    name: &str,
    args: &serde_json::Value,
    ok: bool,
) {
    let Some(ledger) = attribution else {
        return;
    };
    if !ok || is_read_only_call(name, args) {
        return;
    }
    ledger.borrow_mut().record(
        model,
        crate::build_info::harness_name(),
        crate::build_info::PACKAGE_VERSION,
    );
}

/// Reconcile one tool call with the host's confirmed commit signal.
///
/// The TUI drains commit telemetry only after the entire agent run returns, so
/// the counter cannot reset during this scope. Preparation, signing, output
/// text, and a final shell exit status are not publication evidence. A native
/// commit may land before a later command fails or the tool is cancelled.
///
/// Native shell calls may also write after committing. Consume the pre-call
/// contributors but conservatively retain the active model for those possible
/// effects, even after a failing tail. This may retain the active model after
/// a commit-only shell call; it never carries earlier models forward or erases
/// later tool-call contributions. The embedded tool has one operation per
/// call and does not need that conservative retention.
pub(super) struct AttributionEpoch<'a> {
    attribution: Option<&'a std::cell::RefCell<crate::attribution::AttributionLedger>>,
    git_tool: Option<&'a dyn GitTool>,
    confirmed_before: usize,
    model: &'a str,
    name: &'a str,
    finished: bool,
}

impl<'a> AttributionEpoch<'a> {
    pub(super) fn new(
        attribution: Option<&'a std::cell::RefCell<crate::attribution::AttributionLedger>>,
        git_tool: Option<&'a dyn GitTool>,
        model: &'a str,
        name: &'a str,
    ) -> Self {
        Self {
            attribution,
            git_tool,
            confirmed_before: git_tool.map_or(0, GitTool::confirmed_commit_count),
            model,
            name,
            finished: false,
        }
    }

    fn consume_confirmed(&self) -> bool {
        let confirmed = self
            .git_tool
            .is_some_and(|tool| tool.confirmed_commit_count() > self.confirmed_before);
        if confirmed {
            if let Some(ledger) = self.attribution {
                let mut ledger = ledger.borrow_mut();
                ledger.clear();
                if self.name != "git" {
                    ledger.record(
                        self.model,
                        crate::build_info::harness_name(),
                        crate::build_info::PACKAGE_VERSION,
                    );
                }
            }
        }
        confirmed
    }

    pub(super) fn finish(mut self, args: &serde_json::Value, ok: bool) {
        if !self.consume_confirmed() {
            ledger_note_attribution(self.attribution, self.model, self.name, args, ok);
        }
        self.finished = true;
    }
}

impl Drop for AttributionEpoch<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Covers a confirmed publication before an error or cancellation
            // skips normal result accounting. A confirmation arriving only
            // after this scope has gone can conservatively leave old credit;
            // it must never clear credit speculatively during preparation.
            self.consume_confirmed();
        }
    }
}

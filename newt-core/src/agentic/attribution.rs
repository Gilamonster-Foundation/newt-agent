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
use crate::markup::extension::{Monotonic, Stopwatch};
use std::sync::{Arc, Mutex};

/// Per-call timing over the existing prompt lifecycle. Session filtering keeps
/// another tab's prompt out of this call; nesting counts overlapping windows once.
pub(super) struct ToolTimer {
    clock: Arc<dyn Stopwatch + Send + Sync>,
    started: u128,
    // Open windows, first opening, completed wait (all times in nanoseconds).
    wait: Arc<Mutex<(usize, u128, u128)>>,
    _subscription: crate::lifecycle::Subscription,
}

impl ToolTimer {
    pub(super) fn start() -> Self {
        Self::with_clock(Arc::new(Monotonic::default()))
    }

    pub(super) fn with_clock(clock: Arc<dyn Stopwatch + Send + Sync>) -> Self {
        let started = clock.read_nanos();
        let wait = Arc::new(Mutex::new((0, 0, 0)));
        let state = wait.clone();
        let watch = clock.clone();
        let session = crate::lifecycle::active_session();
        let thread = std::thread::current().id();
        let subscription = crate::lifecycle::subscribe(move |envelope| {
            if envelope.session_id != session
                || (session.is_none() && std::thread::current().id() != thread)
            {
                return;
            }
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (depth, opening, total) = &mut *state;
            match envelope.event {
                crate::lifecycle::LifecycleEvent::Blocked => {
                    if *depth == 0 {
                        *opening = watch.read_nanos();
                    }
                    *depth += 1;
                }
                crate::lifecycle::LifecycleEvent::Unblocked if *depth > 0 => {
                    *depth -= 1;
                    if *depth == 0 {
                        *total += watch.read_nanos().saturating_sub(*opening);
                    }
                }
                _ => {}
            }
        });
        Self {
            clock,
            started,
            wait,
            _subscription: subscription,
        }
    }

    fn finish(self) -> (Option<u64>, Option<u64>) {
        let now = self.clock.read_nanos();
        let (depth, opening, total) = *self
            .wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let waiting = total
            + if depth > 0 {
                now.saturating_sub(opening)
            } else {
                0
            };
        let execution = now.saturating_sub(self.started).saturating_sub(waiting);
        (
            u64::try_from(execution / 1_000_000).ok(),
            u64::try_from(waiting / 1_000_000).ok(),
        )
    }
}

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
    timer: ToolTimer,
) {
    if let Some(rec) = tool_events.as_deref_mut() {
        let (duration_ms, operator_wait_ms) = timer.finish();
        rec.push(crate::ToolEvent {
            execution,
            operator_wait_ms,
            ..crate::ToolEvent::from_call(name, args, ok, duration_ms)
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
    let path = args.get("path").and_then(|v| v.as_str());
    // #3039: the same target the write tool resolves (`~/x` is home).
    if let Some(full) =
        path.and_then(|p| super::tools::resolve_tool_path(std::path::Path::new(workspace), p).ok())
    {
        // Key on the lexically-normalized path so a raw model path like
        // `examples/../foo.py` matches the gate's filesystem-normalized `foo.py`
        // and the revert lookup actually hits — otherwise a non-normalized path
        // would silently evade revert (the fabrication persists, the gate is gamed).
        let abs = lexical_normalize(&full);
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

/// The model to credit for a round: the one the backend says served it, or
/// the requested model when the reply names none. A router (for example
/// `openrouter/auto`) answers with a different model than was asked for, and
/// a commit byline must name the model that did the work.
pub(super) fn served_or_requested<'a>(served: Option<&'a str>, requested: &'a str) -> &'a str {
    served
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(requested)
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
                    if let Some(tool) = self.git_tool {
                        tool.note_contribution(self.model);
                    }
                }
            }
        }
        confirmed
    }

    pub(super) fn finish(mut self, args: &serde_json::Value, ok: bool) {
        if !self.consume_confirmed() {
            ledger_note_attribution(self.attribution, self.model, self.name, args, ok);
            if ok && !is_read_only_call(self.name, args) {
                if let Some(tool) = self.git_tool {
                    tool.note_contribution(self.model);
                }
            }
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

#[cfg(test)]
mod timing_tests {
    use super::*;
    use crate::lifecycle::{emit, LifecycleEvent};
    use crate::markup::extension::Stopwatch;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    #[derive(Default)]
    struct Clock(AtomicU64);

    impl Stopwatch for Clock {
        fn read_nanos(&self) -> u128 {
            u128::from(self.0.load(Ordering::Relaxed)) * 1_000_000
        }
    }

    #[test]
    fn delayed_operator_decision_is_separate_from_tool_execution() {
        let clock = Arc::new(Clock::default());
        let timer = ToolTimer::with_clock(clock.clone());
        clock.0.store(2, Ordering::Relaxed);
        emit(LifecycleEvent::Blocked);
        clock.0.store(10_002, Ordering::Relaxed);
        emit(LifecycleEvent::Unblocked);
        clock.0.store(10_007, Ordering::Relaxed);
        let mut events = Vec::new();
        record_completed_tool_event(
            &mut Some(&mut events),
            "run_command",
            &serde_json::json!({}),
            false,
            Some(crate::ExecOutcome::Failed),
            timer,
        );
        assert_eq!(events[0].duration_ms, Some(7));
        assert_eq!(events[0].operator_wait_ms, Some(10_000));
        assert_eq!(events[0].execution, Some(crate::ExecOutcome::Failed));
    }

    #[test]
    fn unprompted_call_records_zero_operator_wait() {
        let clock = Arc::new(Clock::default());
        let timer = ToolTimer::with_clock(clock.clone());
        clock.0.store(7, Ordering::Relaxed);
        assert_eq!(timer.finish(), (Some(7), Some(0)));
    }

    #[test]
    fn repeated_and_nested_prompts_count_only_this_sessions_wait() {
        let session = crate::lifecycle::SessionId::from_issued("timing-nested");
        let _scope = crate::lifecycle::scoped_active_session(&session);
        let clock = Arc::new(Clock::default());
        let timer = ToolTimer::with_clock(clock.clone());
        crate::lifecycle::emit_for(
            Some("another-session".into()),
            crate::lifecycle::LifecycleEvent::Blocked,
        );
        clock.0.store(5, Ordering::Relaxed);
        emit(LifecycleEvent::Blocked);
        clock.0.store(10, Ordering::Relaxed);
        emit(LifecycleEvent::Blocked);
        clock.0.store(15, Ordering::Relaxed);
        emit(LifecycleEvent::Unblocked);
        clock.0.store(20, Ordering::Relaxed);
        emit(LifecycleEvent::Unblocked);
        clock.0.store(25, Ordering::Relaxed);
        emit(LifecycleEvent::Blocked);
        clock.0.store(35, Ordering::Relaxed);
        emit(LifecycleEvent::Unblocked);
        clock.0.store(40, Ordering::Relaxed);
        assert_eq!(timer.finish(), (Some(15), Some(25)));
    }

    #[test]
    fn recording_during_an_open_prompt_includes_the_wait_so_far() {
        let clock = Arc::new(Clock::default());
        let timer = ToolTimer::with_clock(clock.clone());
        clock.0.store(2, Ordering::Relaxed);
        emit(LifecycleEvent::Blocked);
        clock.0.store(12, Ordering::Relaxed);
        assert_eq!(timer.finish(), (Some(2), Some(10)));
    }
}

//! Herdr lifecycle reporting — a small, serialized state protocol.
//!
//! When newt runs inside a [Herdr](https://herdr.dev) pane, Herdr injects
//! `HERDR_ENV=1`, `HERDR_PANE_ID`, and `HERDR_BIN_PATH` into the environment.
//! This module detects that and reports the agent's lifecycle state (`idle` /
//! `working` / `blocked`) through `<HERDR_BIN_PATH> pane report-agent`, which
//! Herdr treats as authoritative — no screen-scraping heuristics needed. When
//! any of those variables is missing the integration is unavailable and every
//! call here is a no-op, resolved once through a `OnceLock`. The binary is
//! always the exact path Herdr injected; `PATH` is never consulted, so an
//! unrelated `herdr` on the user's `PATH` can never be executed.
//!
//! # Protocol design
//!
//! Lifecycle state is a distributed protocol, not logging, so the design is
//! one serialization domain:
//!
//! - Call sites enqueue **semantic events** (`Set`, `PromptOpen`,
//!   `PromptClose`, `Shutdown`) on an mpsc channel and return immediately —
//!   the TUI never blocks on Herdr.
//! - A **single worker thread** owns the state machine and all external
//!   command execution. It applies each event, then runs the `herdr` CLI to
//!   completion (`status()`, which also reaps the child) before touching the
//!   next event. Logical transition order and emitted order therefore cannot
//!   disagree, and no per-report reaper threads exist.
//! - **No `--seq`**: sequence numbers exist to order reports that may arrive
//!   out of order. Delivery here is strictly serialized (one worker, each
//!   command awaited before the next), so `--seq` adds nothing — and omitting
//!   it removes the restart hazard where a fresh process's counter restarts
//!   below a high-water mark Herdr may remember for this pane/source. Every
//!   process generation emits identical, generation-independent reports.
//! - **Delivery vs. desire**: the machine tracks the *logical* state and the
//!   *last successfully delivered* state separately. A failed `herdr`
//!   invocation leaves the delivered mark unchanged, so the next event —
//!   including a later equivalent transition — retries the undelivered state.
//!   One attempt per event: bounded, no retry loops.
//! - **Nested prompts** are depth-counted. The pre-prompt state is captured
//!   only on depth 0 → 1 and restored only on 1 → 0; inner prompts emit
//!   nothing and cannot corrupt the restored state.
//! - **Shutdown** releases lifecycle authority with `pane release-agent`
//!   using the exact same pane/source/agent identity, then stops the worker.
//!   If the channel closes without an explicit shutdown, the worker still
//!   releases best-effort on its way out.
//!
//! Failures are silent by design: lifecycle telemetry must never affect the
//! session.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

/// The stable Herdr report source for newt. Report and release must use the
/// same identity triple (pane, source, agent) or Herdr will not correlate
/// them.
const SOURCE: &str = "custom:newt";
const AGENT: &str = "newt";

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// The Herdr connection identity: the exact injected binary plus the pane to
/// report against. `None` means the integration is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HerdrEnv {
    bin: PathBuf,
    pane: String,
}

impl HerdrEnv {
    /// Pure resolution from the three injected variables, so incompleteness
    /// is unit-testable without mutating process env.
    fn from_parts(env: Option<&str>, pane: Option<&str>, bin: Option<&str>) -> Option<Self> {
        if env != Some("1") {
            return None;
        }
        let pane = pane.filter(|p| !p.is_empty())?;
        let bin = bin.filter(|b| !b.is_empty())?;
        Some(Self {
            bin: PathBuf::from(bin),
            pane: pane.to_string(),
        })
    }

    fn from_process_env() -> Option<Self> {
        let env = std::env::var("HERDR_ENV").ok();
        let pane = std::env::var("HERDR_PANE_ID").ok();
        let bin = std::env::var("HERDR_BIN_PATH").ok();
        Self::from_parts(env.as_deref(), pane.as_deref(), bin.as_deref())
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Working,
    Blocked,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// Semantic lifecycle events. Call sites enqueue these; only the worker
/// interprets them.
#[derive(Debug)]
enum Event {
    /// The REPL is idle at its prompt / working on an accepted turn.
    Set(State),
    /// A human-blocking prompt window opened (post stdin acquisition).
    PromptOpen,
    /// That prompt window closed.
    PromptClose,
    /// Release lifecycle authority and stop the worker.
    Shutdown,
}

/// The logical lifecycle machine. Pure — no I/O — so every transition rule is
/// deterministic to test.
#[derive(Debug)]
struct Machine {
    /// What the state truthfully is right now.
    logical: State,
    /// What Herdr last accepted. `None` until the first successful delivery.
    delivered: Option<State>,
    /// Open prompt windows. Blocked is entered on 0 → 1 and left on 1 → 0.
    depth: u32,
    /// The state to restore when the outermost prompt closes.
    pre_prompt: State,
}

impl Machine {
    fn new() -> Self {
        Self {
            logical: State::Idle,
            delivered: None,
            depth: 0,
            pre_prompt: State::Idle,
        }
    }

    fn apply(&mut self, event: &Event) {
        match event {
            Event::Set(s) => {
                if self.depth == 0 {
                    self.logical = *s;
                } else {
                    // A turn-state change while a prompt is open (e.g. the
                    // turn that spawned the prompt finishing) must not clobber
                    // `blocked`; it becomes the state restored on close.
                    self.pre_prompt = *s;
                }
            }
            Event::PromptOpen => {
                self.depth += 1;
                if self.depth == 1 {
                    self.pre_prompt = self.logical;
                    self.logical = State::Blocked;
                }
            }
            Event::PromptClose => {
                if self.depth > 0 {
                    self.depth -= 1;
                    if self.depth == 0 {
                        self.logical = self.pre_prompt;
                    }
                }
            }
            Event::Shutdown => {}
        }
    }

    /// The state that still needs delivering, if any. Covers both a fresh
    /// transition and a previously failed delivery of the same state.
    fn pending(&self) -> Option<State> {
        (self.delivered != Some(self.logical)).then_some(self.logical)
    }

    fn mark_delivered(&mut self, s: State) {
        self.delivered = Some(s);
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Command execution seam. The worker owns one; tests inject a recorder.
trait Exec: Send {
    /// Run to completion. `true` means Herdr accepted the report.
    fn run(&mut self, bin: &Path, args: &[String]) -> bool;
}

/// The real executor: the exact injected binary, all stdio nulled, awaited
/// (`status()` reaps the child). Never a `PATH` lookup.
struct CommandExec;

impl Exec for CommandExec {
    fn run(&mut self, bin: &Path, args: &[String]) -> bool {
        Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

fn report_args(pane: &str, state: State) -> Vec<String> {
    vec![
        "pane".into(),
        "report-agent".into(),
        pane.into(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        AGENT.into(),
        "--state".into(),
        state.as_str().into(),
    ]
}

fn release_args(pane: &str) -> Vec<String> {
    vec![
        "pane".into(),
        "release-agent".into(),
        pane.into(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        AGENT.into(),
    ]
}

// ---------------------------------------------------------------------------
// Reporter
// ---------------------------------------------------------------------------

/// Handle to the single reporter worker. Enqueue-only from the UI side.
struct Reporter {
    tx: Sender<Event>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Reporter {
    fn spawn(env: HerdrEnv, exec: impl Exec + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<Event>();
        let worker = std::thread::Builder::new()
            .name("herdr-lifecycle".into())
            .spawn(move || worker_loop(&env, exec, &rx))
            .ok();
        Self {
            tx,
            worker: Mutex::new(worker),
        }
    }

    fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Best-effort: release lifecycle authority, then stop and join the
    /// worker. Idempotent — a second call finds no handle and does nothing.
    fn shutdown(&self) {
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            self.send(Event::Shutdown);
            let _ = handle.join();
        }
    }
}

fn worker_loop(env: &HerdrEnv, mut exec: impl Exec, rx: &mpsc::Receiver<Event>) {
    let mut machine = Machine::new();
    for event in rx {
        if matches!(event, Event::Shutdown) {
            let _ = exec.run(&env.bin, &release_args(&env.pane));
            return;
        }
        machine.apply(&event);
        if let Some(state) = machine.pending() {
            if exec.run(&env.bin, &report_args(&env.pane, state)) {
                machine.mark_delivered(state);
            }
        }
    }
    // Channel closed without an explicit shutdown: still release rather than
    // leave Herdr holding a stale authoritative state.
    let _ = exec.run(&env.bin, &release_args(&env.pane));
}

// ---------------------------------------------------------------------------
// Process-wide wiring
// ---------------------------------------------------------------------------

static REPORTER: OnceLock<Option<Reporter>> = OnceLock::new();

/// The process reporter, spun up on first use when the Herdr environment is
/// complete. Also registers the tty-arbiter prompt observer, so every
/// human-blocking prompt (permission gate, question, live-spill modal)
/// enqueues open/close with no per-call-site instrumentation.
fn reporter() -> Option<&'static Reporter> {
    REPORTER
        .get_or_init(|| {
            let env = HerdrEnv::from_process_env()?;
            let reporter = Reporter::spawn(env, CommandExec);
            let tx = reporter.tx.clone();
            newt_core::tty::set_prompt_observer(move |open| {
                let _ = tx.send(if open {
                    Event::PromptOpen
                } else {
                    Event::PromptClose
                });
            });
            Some(reporter)
        })
        .as_ref()
}

/// The REPL is idle at its prompt.
pub(crate) fn set_idle() {
    if let Some(r) = reporter() {
        r.send(Event::Set(State::Idle));
    }
}

/// A non-empty turn was accepted and is running.
pub(crate) fn set_working() {
    if let Some(r) = reporter() {
        r.send(Event::Set(State::Working));
    }
}

/// RAII scope for one chat session: creation initializes the reporter (a
/// no-op outside Herdr); drop releases lifecycle authority on every orderly
/// exit path of `run_chat`, including `?` early returns.
pub(crate) struct SessionGuard;

pub(crate) fn session_guard() -> SessionGuard {
    let _ = reporter();
    SessionGuard
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(Some(r)) = REPORTER.get() {
            r.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread::ThreadId;

    /// One recorded executor invocation: binary, args, executing thread.
    type Call = (PathBuf, Vec<String>, ThreadId);

    /// Injected executor: records every call (with the executing thread) and
    /// answers from a script of per-call successes (missing entries succeed).
    #[derive(Clone, Default)]
    struct Recorder {
        calls: Arc<Mutex<Vec<Call>>>,
        script: Arc<Mutex<Vec<bool>>>,
        next: Arc<AtomicUsize>,
    }

    impl Recorder {
        fn failing_first(n: usize) -> Self {
            let r = Self::default();
            *r.script.lock().unwrap() = vec![false; n];
            r
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn states(&self) -> Vec<String> {
            self.calls()
                .iter()
                .map(|(_, args, _)| match args[1].as_str() {
                    "report-agent" => args[8].clone(),
                    other => other.to_string(),
                })
                .collect()
        }
    }

    impl Exec for Recorder {
        fn run(&mut self, bin: &Path, args: &[String]) -> bool {
            self.calls.lock().unwrap().push((
                bin.to_path_buf(),
                args.to_vec(),
                std::thread::current().id(),
            ));
            let i = self.next.fetch_add(1, Ordering::SeqCst);
            self.script.lock().unwrap().get(i).copied().unwrap_or(true)
        }
    }

    fn test_env() -> HerdrEnv {
        HerdrEnv::from_parts(Some("1"), Some("w1:p2"), Some("/trusted/herdr")).unwrap()
    }

    /// Run a reporter over `events`, shut it down, and return the recorder.
    fn drive(recorder: &Recorder, events: Vec<Event>) {
        let reporter = Reporter::spawn(test_env(), recorder.clone());
        for e in events {
            reporter.send(e);
        }
        reporter.shutdown();
    }

    // (1) Lifecycle is unavailable when the Herdr environment is absent or
    // incomplete — any missing/empty variable disables it.
    #[test]
    fn env_absent_or_incomplete_disables_integration() {
        assert_eq!(HerdrEnv::from_parts(None, None, None), None);
        assert_eq!(
            HerdrEnv::from_parts(Some("1"), Some("w1:p2"), None),
            None,
            "missing HERDR_BIN_PATH must disable, never fall back to PATH"
        );
        assert_eq!(
            HerdrEnv::from_parts(Some("1"), Some("w1:p2"), Some("")),
            None
        );
        assert_eq!(
            HerdrEnv::from_parts(Some("1"), None, Some("/t/herdr")),
            None
        );
        assert_eq!(
            HerdrEnv::from_parts(Some("1"), Some(""), Some("/t/herdr")),
            None
        );
        assert_eq!(
            HerdrEnv::from_parts(Some("0"), Some("w1:p2"), Some("/t/herdr")),
            None
        );
        assert!(HerdrEnv::from_parts(Some("1"), Some("w1:p2"), Some("/t/herdr")).is_some());
    }

    // (2) Every executed command uses exactly the injected HERDR_BIN_PATH.
    #[test]
    fn injected_bin_path_is_used_for_every_command() {
        let rec = Recorder::default();
        drive(&rec, vec![Event::Set(State::Working)]);
        let calls = rec.calls();
        assert!(!calls.is_empty());
        for (bin, _, _) in &calls {
            assert_eq!(bin, Path::new("/trusted/herdr"));
        }
    }

    // (2, real process) HERDR_BIN_PATH wins over a same-named binary on PATH:
    // `CommandExec` invokes the absolute injected path, so a planted
    // /evil/herdr on PATH is never executed.
    #[cfg(unix)]
    #[test]
    fn command_exec_ignores_path_lookup() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let _env = crate::test_env_guard::env_write_guard();
        let base = std::env::temp_dir().join(format!("newt-herdr-binpath-{}", std::process::id()));
        let trusted_dir = base.join("trusted");
        let evil_dir = base.join("evil");
        std::fs::create_dir_all(&trusted_dir).unwrap();
        std::fs::create_dir_all(&evil_dir).unwrap();
        let marker = base.join("marker");
        for (dir, tag) in [(&trusted_dir, "trusted"), (&evil_dir, "evil")] {
            let script = dir.join("herdr");
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(f, "#!/bin/sh\necho {tag} > {}", marker.display()).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let saved_path = std::env::var_os("PATH");
        std::env::set_var("PATH", &evil_dir);
        let ok = CommandExec.run(
            &trusted_dir.join("herdr"),
            &report_args("w1:p2", State::Idle),
        );
        match saved_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }

        assert!(ok);
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            "trusted",
            "PATH's herdr must never run; only HERDR_BIN_PATH's binary may"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // (3) Dedup: an unchanged logical state is reported once.
    #[test]
    fn equal_states_are_deduplicated() {
        let rec = Recorder::default();
        drive(
            &rec,
            vec![
                Event::Set(State::Working),
                Event::Set(State::Working),
                Event::Set(State::Working),
            ],
        );
        assert_eq!(rec.states(), ["working", "release-agent"]);
    }

    // (4) Ordered working → blocked → working through a prompt cycle.
    #[test]
    fn prompt_cycle_reports_in_order() {
        let rec = Recorder::default();
        drive(
            &rec,
            vec![
                Event::Set(State::Working),
                Event::PromptOpen,
                Event::PromptClose,
            ],
        );
        assert_eq!(
            rec.states(),
            ["working", "blocked", "working", "release-agent"]
        );
    }

    // (5) Nested prompts: blocked is entered once on 0→1, inner open/close
    // emit nothing, and the state restored on 1→0 is the pre-OUTER state.
    #[test]
    fn nested_prompts_restore_the_pre_outer_state() {
        let rec = Recorder::default();
        drive(
            &rec,
            vec![
                Event::Set(State::Working),
                Event::PromptOpen,
                Event::PromptOpen,
                Event::PromptClose,
                Event::PromptClose,
            ],
        );
        assert_eq!(
            rec.states(),
            ["working", "blocked", "working", "release-agent"]
        );
    }

    // (5b) Pure machine check: an inner prompt cannot overwrite the saved
    // pre-prompt state, and a Set during a prompt lands in pre_prompt.
    #[test]
    fn machine_depth_semantics() {
        let mut m = Machine::new();
        m.apply(&Event::Set(State::Working));
        m.apply(&Event::PromptOpen);
        assert_eq!(
            (m.logical, m.depth, m.pre_prompt),
            (State::Blocked, 1, State::Working)
        );
        m.apply(&Event::PromptOpen);
        assert_eq!(
            (m.logical, m.depth, m.pre_prompt),
            (State::Blocked, 2, State::Working)
        );
        // A turn-state change mid-prompt updates what will be restored.
        m.apply(&Event::Set(State::Idle));
        assert_eq!(m.logical, State::Blocked);
        m.apply(&Event::PromptClose);
        assert_eq!((m.logical, m.depth), (State::Blocked, 1));
        m.apply(&Event::PromptClose);
        assert_eq!((m.logical, m.depth), (State::Idle, 0));
        // Unbalanced close is ignored, never underflows.
        m.apply(&Event::PromptClose);
        assert_eq!((m.logical, m.depth), (State::Idle, 0));
    }

    // (6) Single serialization domain: every external command runs on the one
    // worker thread, in exactly the semantic order — concurrent enqueuers
    // cannot reorder emission relative to the applied transitions.
    #[test]
    fn all_delivery_happens_on_one_worker_thread_in_order() {
        let rec = Recorder::default();
        let mut events = vec![Event::Set(State::Working)];
        for _ in 0..10 {
            events.push(Event::PromptOpen);
            events.push(Event::PromptClose);
        }
        drive(&rec, events);
        let calls = rec.calls();
        let worker = calls[0].2;
        assert_ne!(worker, std::thread::current().id());
        assert!(calls.iter().all(|(_, _, t)| *t == worker));
        let mut expected = vec!["working".to_string()];
        for _ in 0..10 {
            expected.push("blocked".into());
            expected.push("working".into());
        }
        expected.push("release-agent".into());
        assert_eq!(rec.states(), expected);
    }

    // (7) A failed delivery does not poison the state: the delivered mark is
    // not advanced, so a later equivalent transition redelivers it.
    #[test]
    fn failed_delivery_is_retried_on_a_later_equivalent_event() {
        let rec = Recorder::failing_first(1);
        drive(
            &rec,
            vec![Event::Set(State::Working), Event::Set(State::Working)],
        );
        // Attempted, failed, then retried with the SAME state and delivered.
        assert_eq!(rec.states(), ["working", "working", "release-agent"]);
    }

    // (8) Restart in the same pane/source: reports carry no sequence numbers
    // at all, so a new process generation can never be rejected by a stale
    // high-water mark — generation B's reports are byte-identical to A's.
    #[test]
    fn restart_generation_is_sequence_free_and_identical() {
        let rec_a = Recorder::default();
        drive(&rec_a, vec![Event::Set(State::Working)]);
        // "Process restart": a brand-new reporter, same pane and source.
        let rec_b = Recorder::default();
        drive(&rec_b, vec![Event::Set(State::Working)]);
        let report_a = &rec_a.calls()[0].1;
        let report_b = &rec_b.calls()[0].1;
        assert_eq!(report_a, report_b);
        for (_, args, _) in rec_a.calls().iter().chain(rec_b.calls().iter()) {
            assert!(!args.iter().any(|a| a == "--seq"));
        }
    }

    // (9) Shutdown releases lifecycle authority, exactly once, as the final
    // command; a second shutdown is a no-op.
    #[test]
    fn shutdown_releases_agent_last_and_is_idempotent() {
        let rec = Recorder::default();
        let reporter = Reporter::spawn(test_env(), rec.clone());
        reporter.send(Event::Set(State::Working));
        reporter.shutdown();
        reporter.shutdown();
        let states = rec.states();
        assert_eq!(states, ["working", "release-agent"]);
    }

    // (10) Release uses exactly the same pane/source/agent identity as the
    // state reports.
    #[test]
    fn release_identity_matches_report_identity() {
        fn identity(args: &[String]) -> (String, String, String) {
            let get = |flag: &str| {
                let i = args.iter().position(|a| a == flag).unwrap();
                args[i + 1].clone()
            };
            (args[2].clone(), get("--source"), get("--agent"))
        }
        let report = report_args("w1:p2", State::Working);
        let release = release_args("w1:p2");
        assert_eq!(identity(&report), identity(&release));
        assert_eq!(
            identity(&release),
            ("w1:p2".into(), "custom:newt".into(), "newt".into())
        );
    }

    // Channel death without an explicit shutdown still releases authority.
    #[test]
    fn dropped_channel_still_releases() {
        let rec = Recorder::default();
        let reporter = Reporter::spawn(test_env(), rec.clone());
        reporter.send(Event::Set(State::Working));
        let handle = reporter.worker.lock().unwrap().take().unwrap();
        drop(reporter); // drops the sender; worker drains then releases
        handle.join().unwrap();
        assert_eq!(rec.states(), ["working", "release-agent"]);
    }
}

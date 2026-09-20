use super::*;
use crate::mcp::Mcp;
use crate::{close_out_message, help_lines, permissions_command_lines, ActivePosture};
use newt_core::caveats::{Caveats, CountBound, Scope};
use newt_core::{CaveatsExt as _, DenialKind, PermissionGate as _, PermissionRequest};
use std::cell::Cell;
use std::rc::Rc;

fn base_caveats(ws: &str) -> Caveats {
    Caveats {
        fs_read: Scope::only([ws.to_string()]),
        fs_write: Scope::only([ws.to_string()]),
        exec: Scope::only(["cargo".to_string()]),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

fn exec_request(target: &str) -> PermissionRequest {
    PermissionRequest {
        tool: "run_command".to_string(),
        kind: DenialKind::Exec,
        target: target.to_string(),
        reason: format!("exec of \"{target}\" is not within the granted authority"),
    }
}

/// A4/W6 (part 2): with `web_store` set, the gate PUBLISHES the decision and
/// consumes the operator's WEB verdict — it never reads the TTY. A concurrent
/// answerer stands in for the web POST; allow-once → the gate returns `Allow`.
/// This grounds the store's publish/answer/take methods against the gate's
/// own poll loop (the map from `Verdict` to the reused `PromptChoice` arms).
#[test]
fn web_decisions_publish_and_consume_a_web_verdict_without_the_tty() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("s", None).unwrap();

    // Stand in for the web POST: wait for the gate to publish, then answer.
    let answerer_store = store.clone();
    let answer_conv = conv.clone();
    let answerer = std::thread::spawn(move || {
        for _ in 0..500 {
            if let Ok(Some(p)) = answerer_store.pending_interaction_offer(&answer_conv) {
                answerer_store
                    .answer_interaction_offer(
                        &answer_conv,
                        &p.instance_id,
                        PromptChoice::AllowOnce,
                        Audience::Web,
                    )
                    .unwrap();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the gate never published an interaction offer");
    });

    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let mut gate = PromptPermissionGate {
        state: &mut state,
        base: Caveats::default(),
        key_path: None,
        conversation_id: conv.clone(),
        log_path: None,
        denials_path: None,
        config_path: None,
        preset_clamp: None,
        delegation: None,
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: Duration::from_secs(2),
        cancel: None,
        exit: None,
        // Proof the TTY is bypassed when web decisions are on.
        ask_surface: None,
        #[cfg(feature = "rich-tui")]
        open_panel: None,
        ask_human: |_w: &PromptWindow, _d: &SurfaceInteraction| {
            panic!("the TTY must not be read when web decisions are enabled")
        },
    };
    let decision = gate.ask(&[exec_request("bash")]);
    answerer.join().unwrap();
    assert!(
        matches!(decision, newt_core::PermissionDecision::Allow(_)),
        "a web allow-once verdict must produce Allow"
    );
}

#[test]
fn web_decision_timeout_resolves_and_denies_without_hanging() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("s", None).unwrap();
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let mut gate = PromptPermissionGate {
        state: &mut state,
        base: Caveats::default(),
        key_path: None,
        conversation_id: conv.clone(),
        log_path: None,
        denials_path: None,
        config_path: None,
        preset_clamp: None,
        delegation: None,
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: Duration::from_millis(50),
        cancel: None,
        exit: None,
        ask_surface: None,
        #[cfg(feature = "rich-tui")]
        open_panel: None,
        ask_human: |_w: &PromptWindow, _d: &SurfaceInteraction| {
            panic!("the TTY must not be read when web decisions are enabled")
        },
    };
    let started = Instant::now();
    let decision = gate.ask(&[exec_request("bash")]);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(decision, newt_core::PermissionDecision::Deny));
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].scope, "web-timeout");
    assert!(store.pending_interaction_offer(&conv).unwrap().is_none());
}

#[test]
fn web_publish_failure_records_web_unavailable_scope() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let mut state = PermissionPromptState {
        web_store: Some(store),
        ..Default::default()
    };
    let mut gate = PromptPermissionGate {
        state: &mut state,
        base: Caveats::default(),
        key_path: None,
        conversation_id: "does-not-exist".to_string(),
        log_path: None,
        denials_path: None,
        config_path: None,
        preset_clamp: None,
        delegation: None,
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: Duration::from_millis(50),
        cancel: None,
        exit: None,
        ask_surface: None,
        #[cfg(feature = "rich-tui")]
        open_panel: None,
        ask_human: |_w: &PromptWindow, _d: &SurfaceInteraction| {
            panic!("the TTY must not be read when web decisions are enabled");
        },
    };
    let decision = gate.ask(&[exec_request("bash")]);
    assert!(matches!(decision, newt_core::PermissionDecision::Deny));
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].scope, "web-unavailable");
}

// ---- defect 1: recoverable web-wait control reader --------------------
//
// These drive `run_web_wait` directly with a SCRIPTED control reader, a fake
// stepping clock, and a no-op sleep, so the recovery behaviour is fully
// mocked (no real terminal or wall clock). They ground the invariant that a
// transient reader error never permanently strands the operator, while
// preserving the exactly-once TTY-vs-web CAS and the fail-closed deadline.

use std::collections::VecDeque;
use std::io;

/// A control reader that replays a scripted sequence of poll results, then
/// idles (`Ok(None)`). `io::Result` lets a test inject transient/broken errors.
struct ScriptedReader(VecDeque<io::Result<Option<ModalLine>>>);
impl newt_core::tty::ControlReader for ScriptedReader {
    fn poll(&mut self, _timeout: Duration) -> io::Result<Option<ModalLine>> {
        self.0.pop_front().unwrap_or(Ok(None))
    }
}

fn broken() -> io::Error {
    io::Error::other("reader broke")
}

/// A clock that advances a fixed `step` on each call — deterministic time
/// without sleeping, so the deadline path terminates in bounded iterations.
fn stepping_clock(step: Duration) -> impl Fn() -> Instant {
    let base = Instant::now();
    let n = std::cell::Cell::new(0u32);
    move || {
        let t = base + step * n.get();
        n.set(n.get().saturating_add(1));
        t
    }
}

/// Publish a low-danger exec question and return its `request_id`.
pub(super) fn publish_low_danger(store: &newt_core::ConversationStore, conv: &str) -> String {
    publish_low_danger_for(store, conv, &[Audience::Web])
}

/// The same offer, open to both surfaces — what C4b's gate publishes.
pub(super) fn publish_open_to_both(store: &newt_core::ConversationStore, conv: &str) -> String {
    publish_low_danger_for(store, conv, &[Audience::Terminal, Audience::Web])
}

/// The definition `publish_low_danger` publishes, for the wait loop to decode
/// typed answers against.
pub(super) fn low_danger_definition() -> InteractionDefinition {
    permission_definition(
        &exec_request("bash"),
        &danger::DangerTable::builtin(),
        Audience::Web,
    )
}

fn publish_low_danger_for(
    store: &newt_core::ConversationStore,
    conv: &str,
    audiences: &[Audience],
) -> String {
    let req = exec_request("bash");
    let definition = permission_definition(&req, &danger::DangerTable::builtin(), Audience::Web);
    store
        .publish_interaction_offer(
            conv,
            &definition,
            newt_core::interaction_offer::OfferDanger::Low,
            audiences,
        )
        .unwrap()
}

pub(super) fn store_and_conv() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    newt_core::ConversationStore,
    String,
) {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("s", None).unwrap();
    (root, ws, store, conv)
}

/// Build a web gate with an explicit timeout and optional cancel/exit flags.
macro_rules! web_gate {
    ($state:expr, $conv:expr, $timeout:expr, $cancel:expr, $exit:expr) => {
        PromptPermissionGate {
            state: $state,
            base: Caveats::default(),
            key_path: None,
            conversation_id: $conv,
            log_path: None,
            denials_path: None,
            config_path: None,
            preset_clamp: None,
            delegation: None,
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: $timeout,
            cancel: $cancel,
            exit: $exit,
            ask_surface: None,
            #[cfg(feature = "rich-tui")]
            open_panel: None,
            ask_human: |_w: &PromptWindow, _d: &SurfaceInteraction| {
                panic!("run_web_wait must not read the TTY answer path")
            },
        }
    };
}

// #1959: constructs a real `PromptWindow` via `Terminal::suspend_for_prompt`,
// which bumps the SAME process-global counter
// `headless_and_piped_sessions_never_construct_a_prompt_window` asserts is
// untouched — serialized against it so the two can never race.
#[serial_test::serial(prompt_stdin)]
#[test]
fn transient_reader_error_recovers_and_esc_resolves_through_the_tty_path() {
    // Reader #1 errors (non-Interrupted); after re-arm, reader #2 yields Esc.
    // The local abort wins the CAS and the gate returns Back.
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    let mut readers: VecDeque<ScriptedReader> = VecDeque::from([
        ScriptedReader(VecDeque::from([Err(broken())])),
        ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))])),
    ]);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let (choice, scope) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |_m| {},
        },
    );
    assert_eq!(choice, PromptChoice::Back);
    assert_eq!(scope, "control");
    // The local abort resolved the request (nothing left pending).
    assert!(store.pending_interaction_offer(&conv).unwrap().is_none());
}

// #1959: see the comment on the previous test — same shared-counter race.
#[serial_test::serial(prompt_stdin)]
#[test]
fn transient_reader_error_does_not_deny_when_a_web_verdict_arrives() {
    // Reader errors, but a web ALLOW is already recorded: the temporary reader
    // failure must not force a denial — the web verdict is honored.
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    store
        .answer_interaction_offer(&conv, &request_id, PromptChoice::AllowOnce, Audience::Web)
        .unwrap();
    let mut readers: VecDeque<ScriptedReader> =
        VecDeque::from([ScriptedReader(VecDeque::from([Err(broken())]))]);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let (choice, _scope) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |_m| {},
        },
    );
    assert_eq!(
        choice,
        PromptChoice::AllowOnce,
        "web allow must survive a reader error"
    );
}

// #1959: see the comment on `transient_reader_error_recovers_and_esc_resolves_through_the_tty_path`.
#[serial_test::serial(prompt_stdin)]
#[test]
fn reader_failing_until_deadline_denies_without_busy_spin() {
    // The reader can never be re-armed; the loop must NOT busy-spin (it paces
    // via sleep) and must resolve as the fail-closed timeout denial.
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_millis(500),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let sleeps = std::cell::Cell::new(0u32);
    let (choice, scope) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || Err::<Box<dyn newt_core::tty::ControlReader>, _>(broken()),
            now: &stepping_clock(Duration::from_millis(20)),
            sleep: &mut |_d| sleeps.set(sleeps.get() + 1),
            notify: &mut |_m| {},
        },
    );
    assert_eq!(choice, PromptChoice::Deny);
    assert_eq!(scope, "web-timeout");
    // Paced (slept at least once) and bounded (nowhere near a busy-spin).
    assert!(sleeps.get() >= 1, "must pace via sleep, not spin");
    assert!(
        sleeps.get() < 10_000,
        "bounded iterations: {}",
        sleeps.get()
    );
}

// #1959: see the comment on `transient_reader_error_recovers_and_esc_resolves_through_the_tty_path`.
#[serial_test::serial(prompt_stdin)]
#[test]
fn web_verdict_and_local_control_resolve_exactly_once() {
    // (a) Web verdict already recorded → a concurrent local Back consumes THAT
    //     verdict instead of overwriting it.
    let (_r1, _w1, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    store
        .answer_interaction_offer(&conv, &request_id, PromptChoice::AllowOnce, Audience::Web)
        .unwrap();
    let mut readers: VecDeque<ScriptedReader> =
        VecDeque::from([ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))]))]);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |_m| {},
        },
    );
    assert_eq!(
        choice,
        PromptChoice::AllowOnce,
        "web verdict already won; local consumes it"
    );

    // (b) Local Back wins first → a later web answer cannot authorize.
    let (_r2, _w2, store2, conv2) = store_and_conv();
    let request_id2 = publish_low_danger(&store2, &conv2);
    let mut readers2: VecDeque<ScriptedReader> =
        VecDeque::from([ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))]))]);
    let mut state2 = PermissionPromptState {
        web_store: Some(store2.clone()),
        ..Default::default()
    };
    let gate2 = web_gate!(
        &mut state2,
        conv2.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win2 = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let (choice2, _s2) = gate2.run_web_wait(
        &store2,
        &request_id2,
        &low_danger_definition(),
        &win2,
        &mut WebWaitIo {
            reacquire: &mut || {
                readers2
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |_m| {},
        },
    );
    assert_eq!(choice2, PromptChoice::Back, "local abort won the race");
    // The request is resolved: a later web POST finds nothing to answer.
    assert!(store2.pending_interaction_offer(&conv2).unwrap().is_none());
    let late = store2
        .answer_interaction_offer(&conv2, &request_id2, PromptChoice::AllowOnce, Audience::Web)
        .unwrap();
    assert!(
        !matches!(late, newt_core::store::AnswerOutcome::Answered),
        "a late web answer must not authorize an already-resolved request: {late:?}"
    );
}

// #1959: see the comment on `transient_reader_error_recovers_and_esc_resolves_through_the_tty_path`.
#[serial_test::serial(prompt_stdin)]
#[test]
fn ctrl_c_after_a_recoverable_reader_error_sets_cancel_and_exit() {
    // A reader error, then re-arm, then Ctrl-C/Ctrl-D → run_web_wait returns
    // Exit and (as ask() then applies) both the cancel AND exit flags set.
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    let mut readers: VecDeque<ScriptedReader> = VecDeque::from([
        ScriptedReader(VecDeque::from([Err(broken())])),
        ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Exit))])),
    ]);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let exit = std::sync::atomic::AtomicBool::new(false);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        Some(&cancel),
        Some(&exit)
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |_m| {},
        },
    );
    assert_eq!(choice, PromptChoice::Exit);
    // ask() applies the control on Back|Exit; Exit sets both signals.
    gate.apply_control(choice);
    assert!(
        cancel.load(std::sync::atomic::Ordering::Relaxed),
        "cancel must be set"
    );
    assert!(
        exit.load(std::sync::atomic::Ordering::Relaxed),
        "exit must be set"
    );
}

// #1959: see the comment on `transient_reader_error_recovers_and_esc_resolves_through_the_tty_path`.
#[serial_test::serial(prompt_stdin)]
#[test]
fn repeated_interrupted_keeps_the_reader_and_paces_without_spinning() {
    // EINTR returns immediately; the SAME reader is retried (not dropped) and
    // the loop paces via sleep rather than busy-spinning. After several
    // Interrupted errors the same reader yields Esc, which still resolves.
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    let mut readers: VecDeque<ScriptedReader> = VecDeque::from([ScriptedReader(VecDeque::from([
        Err(io::Error::from(io::ErrorKind::Interrupted)),
        Err(io::Error::from(io::ErrorKind::Interrupted)),
        Err(io::Error::from(io::ErrorKind::Interrupted)),
        Ok(Some(ModalLine::Back)),
    ]))]);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let reacquired = std::cell::Cell::new(0u32);
    let sleeps = std::cell::Cell::new(0u32);
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                reacquired.set(reacquired.get() + 1);
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| sleeps.set(sleeps.get() + 1),
            notify: &mut |_m| {},
        },
    );
    assert_eq!(
        choice,
        PromptChoice::Back,
        "same reader survives EINTR and yields Esc"
    );
    assert_eq!(
        reacquired.get(),
        1,
        "an Interrupted error must NOT drop/recreate the reader"
    );
    // Paced (slept between EINTR retries) and bounded (no busy-spin).
    assert!(
        sleeps.get() >= 3,
        "must pace between EINTR retries: {}",
        sleeps.get()
    );
    assert!(sleeps.get() < 10_000, "bounded: {}", sleeps.get());
}

// #1959: see the comment on `transient_reader_error_recovers_and_esc_resolves_through_the_tty_path`.
#[serial_test::serial(prompt_stdin)]
#[test]
fn an_initial_unsupported_still_retries_and_recovers() {
    // A terminal-loss race at the FIRST acquisition (Unsupported) must NOT
    // permanently disable controls — the gate is only built for an
    // interactive session, so this is a race, not a headless session. Keep
    // retrying, re-arm when the terminal returns, and Esc still resolves.
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    let mut outcomes: VecDeque<io::Result<ScriptedReader>> = VecDeque::from([
        Err(io::Error::from(io::ErrorKind::Unsupported)), // INITIAL acquisition fails
        Ok(ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))]))), // terminal returns
    ]);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                outcomes
                    .pop_front()
                    .unwrap_or_else(|| Err(broken()))
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |_m| {},
        },
    );
    assert_eq!(
        choice,
        PromptChoice::Back,
        "an initial Unsupported must not permanently disable controls"
    );
}

// #1959: see the comment on `transient_reader_error_recovers_and_esc_resolves_through_the_tty_path`.
#[serial_test::serial(prompt_stdin)]
#[test]
fn a_post_live_unsupported_keeps_retrying_and_recovers() {
    // A gate built for an interactive session that momentarily loses its
    // terminal (reacquire → Unsupported) keeps retrying (bounded) and re-arms
    // when the terminal returns, then Esc still resolves. Guards against
    // recreating the original permanent-disable defect shape.
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    let mut outcomes: VecDeque<io::Result<ScriptedReader>> = VecDeque::from([
        Ok(ScriptedReader(VecDeque::from([Err(broken())]))), // Live, then breaks
        Err(io::Error::from(io::ErrorKind::Unsupported)),    // terminal "gone"
        Err(io::Error::from(io::ErrorKind::Unsupported)),    // still gone
        Ok(ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Back))]))), // back
    ]);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                outcomes
                    .pop_front()
                    .unwrap_or_else(|| Err(broken()))
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |_m| {},
        },
    );
    assert_eq!(
        choice,
        PromptChoice::Back,
        "a transient Unsupported must not permanently disable controls"
    );
}

// ---- defect: authorization-prompt policy is separate from human presence -
// The gate is built whenever the session has a usable TTY; permission
// prompting is a separate policy (`authorization_prompts_enabled`). Disabling
// it must deny authorization WITHOUT prompting, and must NOT erase the
// operator from `request_user_input` (proven in newt-core's
// `request_user_input_reaches_the_operator_even_when_permissions_are_denied`).

#[test]
fn authorization_prompts_disabled_denies_without_opening_a_prompt() {
    // TTY + permissions DISABLED: ask() denies and never consults the human
    // (the empty script would panic on any prompt).
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0usize));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![],
        prompts.clone(),
    );
    gate.authorization_prompts_enabled = false;
    let decision = gate.ask(&[exec_request("bash")]);
    assert!(matches!(decision, newt_core::PermissionDecision::Deny));
    assert_eq!(prompts.get(), 0, "disabled prompts must not open a prompt");
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].scope, "authorization-prompts-disabled");
}

#[test]
fn authorization_prompts_enabled_consults_the_operator() {
    // TTY + permissions ENABLED: ask() DOES prompt (scripted allow-once).
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0usize));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![PromptChoice::AllowOnce],
        prompts.clone(),
    );
    let decision = gate.ask(&[exec_request("bash")]);
    assert!(matches!(decision, newt_core::PermissionDecision::Allow(_)));
    assert_eq!(
        prompts.get(),
        1,
        "enabled prompts consult the operator once"
    );
}

#[test]
fn allow_permanent_records_session_scope_when_net_persist_fails() {
    let root = tempfile::TempDir::new().unwrap();
    let config = root.path().join("blocked-config-dir");
    std::fs::create_dir_all(&config).unwrap();
    let base = base_caveats("/ws");
    let net_req = newt_core::PermissionRequest {
        tool: "web_fetch".to_string(),
        kind: DenialKind::Net,
        target: "github.com".to_string(),
        reason: "net does not permit 'github.com'".to_string(),
    };

    let mut state = PermissionPromptState::default();
    {
        let mut gate = PromptPermissionGate {
            state: &mut state,
            base,
            key_path: None,
            conversation_id: "conv-config-fail".to_string(),
            log_path: None,
            denials_path: None,
            config_path: Some(config.clone()),
            preset_clamp: None,
            delegation: None,
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_surface: None,
            #[cfg(feature = "rich-tui")]
            open_panel: None,
            ask_human: move |_w: &PromptWindow, _d: &SurfaceInteraction| {
                PromptChoice::AllowPermanent
            },
        };
        assert!(matches!(
            gate.ask(std::slice::from_ref(&net_req)),
            newt_core::PermissionDecision::Allow(_)
        ));
    }
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(
        state.decisions[0].scope, "permanent-persist-failed",
        "failed net persistence should not be logged as durable"
    );
}

/// A gate whose "human" is a script of choices; counts every prompt.
pub(super) fn scripted_gate<'a>(
    state: &'a mut PermissionPromptState,
    base: Caveats,
    key_path: Option<std::path::PathBuf>,
    log_path: Option<std::path::PathBuf>,
    script: Vec<PromptChoice>,
    prompts: Rc<Cell<usize>>,
) -> PromptPermissionGate<'a, impl FnMut(&PromptWindow, &SurfaceInteraction) -> PromptChoice> {
    let mut script = script.into_iter();
    PromptPermissionGate {
        state,
        base,
        key_path,
        conversation_id: "conv-test".to_string(),
        log_path,
        denials_path: None,
        config_path: None,
        preset_clamp: None,
        delegation: None,
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: Duration::from_secs(2),
        cancel: None,
        exit: None,
        ask_surface: None,
        #[cfg(feature = "rich-tui")]
        open_panel: None,
        ask_human: move |_w: &PromptWindow, _definition: &SurfaceInteraction| {
            prompts.set(prompts.get() + 1);
            script.next().expect("script exhausted — unexpected prompt")
        },
    }
}

#[test]
fn mcp_net_prompt_routes_choices_and_controls_through_the_terminal_owner() {
    for (outcome, allowed, remembered, cancelled, exited) in [
        (
            HumanQuestionOutcome::Answer("a".into()),
            true,
            false,
            false,
            false,
        ),
        (
            HumanQuestionOutcome::Answer("s".into()),
            true,
            true,
            false,
            false,
        ),
        (
            HumanQuestionOutcome::Answer("A".into()),
            true,
            true,
            false,
            false,
        ),
        (
            HumanQuestionOutcome::Answer("d".into()),
            false,
            false,
            false,
            false,
        ),
        (
            HumanQuestionOutcome::Answer(String::new()),
            true,
            false,
            false,
            false,
        ),
        (
            HumanQuestionOutcome::Answer("unknown".into()),
            false,
            false,
            false,
            false,
        ),
        (HumanQuestionOutcome::Cancelled, false, false, true, false),
        (
            HumanQuestionOutcome::ExitRequested,
            false,
            false,
            true,
            true,
        ),
        (
            HumanQuestionOutcome::InputClosed,
            false,
            false,
            false,
            false,
        ),
        (
            HumanQuestionOutcome::InputFailed,
            false,
            false,
            false,
            false,
        ),
        (
            HumanQuestionOutcome::Unavailable,
            false,
            false,
            false,
            false,
        ),
    ] {
        for base in [base_caveats("/ws"), Caveats::top()] {
            let directory = tempfile::TempDir::new().unwrap();
            let config = directory.path().join("config.toml");
            let mut state = PermissionPromptState::default();
            let direct_prompts = Rc::new(Cell::new(0));
            let surface_prompts = Cell::new(0);
            let blocked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = blocked.clone();
            let me = std::thread::current().id();
            let _events = newt_core::lifecycle::subscribe(move |event| {
                if std::thread::current().id() == me
                    && matches!(event.event, newt_core::lifecycle::LifecycleEvent::Blocked)
                {
                    seen.fetch_add(1, Ordering::Relaxed);
                }
            });
            let cancel = AtomicBool::new(false);
            let exit = AtomicBool::new(false);
            let request = PermissionRequest {
                tool: "mcp connect".into(),
                kind: DenialKind::Net,
                target: "mcp.example.test".into(),
                reason: "connect the configured MCP server".into(),
            };
            let ask = |interaction: &SurfaceInteraction| {
                surface_prompts.set(surface_prompts.get() + 1);
                assert!(interaction.is_blocking());
                assert!(interaction.wants_attention());
                assert_eq!(
                    *interaction,
                    permission_interaction(
                        &request,
                        &danger::DangerTable::builtin(),
                        PromptChoice::AllowOnce,
                    ),
                    "the terminal receives the exact definition used to authorize the answer"
                );
                outcome.clone()
            };
            let mut gate = scripted_gate(
                &mut state,
                base,
                None,
                None,
                vec![PromptChoice::Deny],
                direct_prompts.clone(),
            );
            gate.ask_surface = Some(&ask);
            gate.config_path = Some(config.clone());
            gate.cancel = Some(&cancel);
            gate.exit = Some(&exit);
            let grant = gate.ask_mcp_net_grant(&request);
            assert_eq!(
                blocked.load(Ordering::Relaxed),
                0,
                "the session must not acquire a PromptWindow before asking its terminal owner"
            );
            assert_eq!(direct_prompts.get(), 0);
            assert_eq!(surface_prompts.get(), 1);
            assert_eq!(grant.is_some(), allowed, "{outcome:?}");
            if let Some((caveats, hosts, retained)) = grant {
                assert!(caveats.permits_net(&request.target));
                assert!(hosts.contains(&request.target));
                assert_eq!(retained, remembered);
            }
            assert_eq!(cancel.load(Ordering::Relaxed), cancelled);
            assert_eq!(exit.load(Ordering::Relaxed), exited);
            if outcome == HumanQuestionOutcome::Answer("A".into()) {
                let permissions = crate::migration_notices::read(|report| {
                    newt_core::Config::load(&config, report)
                })
                .unwrap()
                .tui
                .unwrap()
                .permissions;
                assert_eq!(permissions.net, vec![request.target]);
            } else {
                assert!(!config.exists(), "only a permanent answer writes config");
            }
        }
    }
}

#[test]
fn mcp_net_prompt_configured_blank_answer_preserves_grant_lifetime() {
    for (configured, allowed, remembered, persisted) in [
        (None, true, false, false),
        (Some(PromptChoice::AllowSession), true, true, false),
        (Some(PromptChoice::AllowPermanent), true, true, true),
        (Some(PromptChoice::Deny), false, false, false),
        (Some(PromptChoice::Back), false, false, false),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut state = PermissionPromptState {
            mcp_net_prompt_default: configured,
            ..Default::default()
        };
        let ask = |_interaction: &SurfaceInteraction| HumanQuestionOutcome::Answer(String::new());
        let mut gate = scripted_gate(
            &mut state,
            Caveats::top(),
            None,
            None,
            vec![],
            Rc::new(Cell::new(0)),
        );
        gate.ask_surface = Some(&ask);
        gate.config_path = Some(config.clone());
        let grant = gate.ask_mcp_net_grant(&PermissionRequest {
            tool: "mcp connect".into(),
            kind: DenialKind::Net,
            target: "mcp.example.test".into(),
            reason: "connect the configured server".into(),
        });
        assert_eq!(grant.is_some(), allowed);
        if let Some((_, _, retained)) = grant {
            assert_eq!(retained, remembered);
        }
        assert_eq!(config.exists(), persisted);
    }
}

#[test]
fn terminal_blank_answer_allows_once_and_never_closure() {
    for outcome in [
        HumanQuestionOutcome::Answer(String::new()),
        HumanQuestionOutcome::InputClosed,
        HumanQuestionOutcome::Cancelled,
    ] {
        let mut state = PermissionPromptState::default();
        let ask = |_interaction: &SurfaceInteraction| outcome.clone();
        let mut gate = scripted_gate(
            &mut state,
            Caveats::top(),
            None,
            None,
            vec![],
            Rc::new(Cell::new(0)),
        );
        gate.ask_surface = Some(&ask);
        let request = PermissionRequest {
            tool: "remote__search".into(),
            kind: DenialKind::RemoteTool,
            target: "remote__search".into(),
            reason: "operator approval required".into(),
        };
        let result = gate.ask(&[request]);
        assert_eq!(
            matches!(result, newt_core::PermissionDecision::Allow(_)),
            matches!(outcome, HumanQuestionOutcome::Answer(_)),
            "{outcome:?}"
        );
        assert!(
            state.session_grants.is_empty(),
            "Enter cannot create standing authority"
        );
    }
}

#[test]
fn mcp_net_prompt_defaults_are_offered_and_generic_defaults_cannot_add_standing_authority() {
    let request = PermissionRequest {
        tool: "mcp connect".into(),
        kind: DenialKind::Net,
        target: "mcp.example.test".into(),
        reason: "connect the configured server".into(),
    };
    let danger = danger::DangerTable::builtin();
    for configured in [
        PromptChoice::AllowOnce,
        PromptChoice::AllowSession,
        PromptChoice::AllowPermanent,
        PromptChoice::Deny,
        PromptChoice::DenyAlways,
        PromptChoice::DenyPermanent,
        PromptChoice::Back,
        PromptChoice::Exit,
    ] {
        let expected = if matches!(configured, PromptChoice::Back | PromptChoice::Exit) {
            PromptChoice::Deny
        } else {
            configured
        };
        let interaction = permission_interaction(&request, &danger, configured);
        let choice = interaction.default_choice().unwrap();
        assert_eq!(choice.id.as_str(), expected.as_str());
        assert!(choice.label.ends_with(" (default)"));
        assert_eq!(
            plain::render(&interaction.definition)
                .matches("(default)")
                .count(),
            1
        );
        assert_eq!(
            decode_answer(&interaction.definition, interaction.answer_or_default("")),
            expected
        );
        assert_eq!(
            decode_answer(
                &interaction.definition,
                interaction.answer_or_default("unknown")
            ),
            PromptChoice::Deny
        );
    }
    for (tool, kind, target) in [
        ("web_fetch", DenialKind::Net, "mcp.example.test"),
        ("mcp connect", DenialKind::Exec, "bash"),
        ("run_command", DenialKind::Exec, "bash"),
    ] {
        let other = PermissionRequest {
            tool: tool.into(),
            kind,
            target: target.into(),
            reason: String::new(),
        };
        let interaction = permission_interaction(&other, &danger, PromptChoice::AllowPermanent);
        assert_eq!(interaction.default_choice().unwrap().id.as_str(), "deny");
        assert_eq!(
            decode_answer(&interaction.definition, interaction.answer_or_default("")),
            PromptChoice::Deny
        );
        assert_eq!(
            interaction.definition,
            permission_definition(&other, &danger, Audience::Terminal)
        );
    }
}

#[test]
#[cfg(feature = "rich-tui")]
#[serial_test::serial(prompt_stdin)]
fn web_permission_asks_terminal_owner_before_claiming_input() {
    let (_root, _workspace, store, conversation_id) = store_and_conv();
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let prompts = Cell::new(0);
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = blocked.clone();
    let me = std::thread::current().id();
    let _events = newt_core::lifecycle::subscribe(move |event| {
        if std::thread::current().id() == me
            && matches!(event.event, newt_core::lifecycle::LifecycleEvent::Blocked)
        {
            seen.fetch_add(1, Ordering::Relaxed);
        }
    });
    let open_panel = |rows| {
        assert!(
            rows > 2,
            "the web offer needs a bounded modal body and border"
        );
        assert_eq!(blocked.load(Ordering::Relaxed), 0);
        prompts.set(prompts.get() + 1);
        let pending = store
            .pending_interaction_offer(&conversation_id)
            .unwrap()
            .unwrap();
        store
            .answer_interaction_offer(
                &conversation_id,
                &pending.instance_id,
                PromptChoice::AllowOnce,
                Audience::Web,
            )
            .unwrap();
        // A surface may refuse the loan. Closing the offer must still
        // consume the existing web CAS winner, never invent a verdict.
        None
    };
    let mut gate = scripted_gate(
        &mut state,
        Caveats::top(),
        None,
        None,
        vec![],
        Rc::new(Cell::new(0)),
    );
    gate.conversation_id = conversation_id.clone();
    gate.web_decision_timeout = Duration::ZERO;
    gate.open_panel = Some(&open_panel);
    assert!(matches!(
        gate.ask(&[exec_request("git")]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(prompts.get(), 1);
    assert_eq!(blocked.load(Ordering::Relaxed), 0);
}

#[test]
#[cfg(feature = "rich-tui")]
#[serial_test::serial(prompt_stdin)]
fn web_permission_refused_modal_loan_does_not_take_input_or_leave_an_offer() {
    let (_root, _workspace, store, conversation_id) = store_and_conv();
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = blocked.clone();
    let me = std::thread::current().id();
    let _events = newt_core::lifecycle::subscribe(move |event| {
        if std::thread::current().id() == me
            && matches!(event.event, newt_core::lifecycle::LifecycleEvent::Blocked)
        {
            seen.fetch_add(1, Ordering::Relaxed);
        }
    });
    let open_panel = |_rows| None;
    let mut gate = scripted_gate(
        &mut state,
        Caveats::top(),
        None,
        None,
        vec![],
        Rc::new(Cell::new(0)),
    );
    gate.conversation_id = conversation_id.clone();
    gate.web_decision_timeout = Duration::ZERO;
    gate.open_panel = Some(&open_panel);
    assert!(matches!(
        gate.ask(&[exec_request("git")]),
        newt_core::PermissionDecision::Deny
    ));
    assert!(store
        .pending_interaction_offer(&conversation_id)
        .unwrap()
        .is_none());
    assert_eq!(
        blocked.load(Ordering::Relaxed),
        0,
        "a declined loan never hands stdin to the worker"
    );
}

#[test]
fn mcp_net_prompt_distinguishes_once_from_shared_session_grants() {
    for (choice, remembered, expected_prompts) in [
        (PromptChoice::AllowOnce, false, 2),
        (PromptChoice::AllowSession, true, 1),
        (PromptChoice::AllowPermanent, true, 1),
    ] {
        for base in [base_caveats("/ws"), Caveats::top()] {
            let mut state = PermissionPromptState::default();
            let prompts = Rc::new(Cell::new(0));
            let mut gate = scripted_gate(
                &mut state,
                base,
                None,
                None,
                vec![choice, choice],
                prompts.clone(),
            );
            let request = PermissionRequest {
                tool: "mcp connect".into(),
                kind: DenialKind::Net,
                target: "mcp.example.test".into(),
                reason: "connect the configured MCP server".into(),
            };
            for _ in 0..2 {
                let (granted, names, retain) = gate.ask_mcp_net_grant(&request).unwrap();
                assert!(granted.permits_net(&request.target));
                assert!(names.contains(&request.target));
                assert_eq!(retain, remembered);
            }
            assert_eq!(prompts.get(), expected_prompts);
        }
    }
}

#[test]
fn mcp_net_prompt_disabled_or_denied_never_grants() {
    for enabled in [false, true] {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            Caveats::top(),
            None,
            None,
            vec![PromptChoice::Deny],
            prompts.clone(),
        );
        gate.authorization_prompts_enabled = enabled;
        let request = PermissionRequest {
            tool: "mcp connect".into(),
            kind: DenialKind::Net,
            target: "mcp.example.test".into(),
            reason: "private MCP origin needs an exact approval".into(),
        };
        assert!(gate.ask_mcp_net_grant(&request).is_none());
        assert_eq!(prompts.get(), usize::from(enabled));
    }
}

#[test]
fn mcp_net_grant_retains_prior_session_names_under_full_access() {
    for first in [
        PromptChoice::AllowOnce,
        PromptChoice::AllowSession,
        PromptChoice::AllowPermanent,
    ] {
        let mut state = PermissionPromptState::default();
        let mut gate = scripted_gate(
            &mut state,
            Caveats::top(),
            None,
            None,
            vec![first, PromptChoice::AllowSession],
            Rc::new(Cell::new(0)),
        );
        for target in ["auth.example.test", "mcp.example.test"] {
            let request = PermissionRequest {
                tool: "mcp connect".into(),
                kind: DenialKind::Net,
                target: target.into(),
                reason: "private MCP origin needs exact approval".into(),
            };
            let (caveats, names, _) = gate.ask_mcp_net_grant(&request).unwrap();
            let policy =
                newt_mcp_client::HttpNetworkPolicy::with_explicit_hosts(&caveats.net, &names);
            assert!(policy.explicitly_grants_host(target));
            if target == "mcp.example.test" {
                assert_eq!(
                    policy.explicitly_grants_host("auth.example.test"),
                    first != PromptChoice::AllowOnce
                );
                assert!(!policy.explicitly_grants_host("unapproved.example.test"));
            }
        }
    }
}

#[test]
fn permanent_net_target_follows_config_precedence_without_writing_ambient_config() {
    let pinned = std::path::PathBuf::from("explicit.toml");
    let user = std::path::PathBuf::from("user/config.toml");
    assert_eq!(
        durable_permission_config_target(Some(pinned.clone()), true, Some(user.clone())),
        Some(pinned),
    );
    assert_eq!(
        durable_permission_config_target(None, false, Some(user.clone())),
        Some(user.clone()),
    );
    assert_eq!(
        durable_permission_config_target(None, true, Some(user)),
        None
    );
    assert_eq!(durable_permission_config_target(None, false, None), None);
}

#[test]
fn nested_controls_cancel_without_recording_a_permission_decision() {
    for (choice, exits) in [(PromptChoice::Back, false), (PromptChoice::Exit, true)] {
        let cancel = AtomicBool::new(false);
        let exit = AtomicBool::new(false);
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![choice],
            prompts,
        );
        gate.cancel = Some(&cancel);
        gate.exit = Some(&exit);
        assert!(matches!(
            gate.ask(&[exec_request("npm")]),
            newt_core::PermissionDecision::Deny
        ));
        drop(gate);
        assert!(cancel.load(Ordering::Relaxed));
        assert_eq!(exit.load(Ordering::Relaxed), exits);
        assert!(state.decisions.is_empty());
    }
}

#[test]
fn question_policy_and_markdown_cover_each_axis_and_danger_tier() {
    let danger = danger::DangerTable::builtin();
    for (kind, target, wording) in [
        (DenialKind::FsRead, "/etc/hosts", "read"),
        (DenialKind::FsWrite, "/ws/f", "write"),
        (DenialKind::Net, "docs.rs", "reach"),
        (DenialKind::RemoteTool, "remote__tool", "call"),
        (DenialKind::GitWrite, "commit", "commit/stage via git"),
    ] {
        let q = permission_definition(
            &PermissionRequest {
                tool: "tool".into(),
                kind,
                target: target.into(),
                reason: String::new(),
            },
            &danger,
            Audience::Terminal,
        );
        assert!(q.markdown.contains(&format!("{wording} `{target}`")));
    }

    let low = permission_definition(&exec_request("npm"), &danger, Audience::Terminal);
    assert!(offers(&low, PromptChoice::AllowSession));
    assert!(low.markdown.contains("outside the granted exec allowlist"));

    let model_authored = PermissionRequest {
        tool: "request_permissions".into(),
        kind: DenialKind::Exec,
        target: "bash".into(),
        reason: "list the files".into(),
    };
    let high = permission_definition(&model_authored, &danger, Audience::Terminal);
    assert!(!offers(&high, PromptChoice::AllowSession));
    let text = plain::render(&permission_definition(
        &model_authored,
        &danger,
        Audience::Terminal,
    ));
    for expected in [
        "interpreter",
        "arbitrary command execution",
        "model-authored, unverified",
        "list the files",
        "session allow refused",
    ] {
        assert!(text.contains(expected), "missing {expected:?}: {text}");
    }

    let root = permission_definition(
        &PermissionRequest {
            tool: "request_permissions".into(),
            kind: DenialKind::FsWrite,
            target: "/".into(),
            reason: String::new(),
        },
        &danger,
        Audience::Terminal,
    );
    assert!(root.markdown.contains("filesystem root"));
    assert!(!offers(&root, PromptChoice::AllowSession));

    let web_low = permission_definition(&exec_request("npm"), &danger, Audience::Web);
    assert_eq!(
        offered_actions(&web_low),
        [
            PromptChoice::AllowOnce,
            PromptChoice::AllowSession,
            PromptChoice::Deny
        ]
    );
    // D0 (#1878): the wire round trip is the DEFINITION's now — the
    // legacy `Question` is no longer what the web publishes. A0's frozen
    // `Question` wire shape is still pinned, in
    // `markup_sprawl_ratchet::the_question_wire_shape_is_frozen`.
    assert_eq!(
        serde_json::from_str::<InteractionDefinition>(&serde_json::to_string(&web_low).unwrap())
            .unwrap(),
        web_low
    );
    let web_high = permission_definition(&exec_request("bash"), &danger, Audience::Web);
    assert_eq!(
        offered_actions(&web_high),
        [PromptChoice::AllowOnce, PromptChoice::Deny]
    );
}

#[test]
fn high_danger_target_is_not_session_allowable_but_allow_once_works() {
    let base = base_caveats("/ws");

    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    {
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![PromptChoice::AllowSession],
            prompts.clone(),
        );
        assert!(
            matches!(
                gate.ask(&[exec_request("bash")]),
                newt_core::PermissionDecision::Deny
            ),
            "session-allow of an interpreter must be refused (deny)"
        );
    }
    assert!(
        !state
            .session_grants
            .contains(&(DenialKind::Exec, "bash".to_string())),
        "a refused session-allow must leave NO standing grant"
    );
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].decision, "deny");
    assert!(
        state.decisions[0].scope.contains("refused"),
        "the record must mark the high-danger refusal, got: {}",
        state.decisions[0].scope
    );

    let mut once_state = PermissionPromptState::default();
    let once_prompts = Rc::new(Cell::new(0));
    let mut once_gate = scripted_gate(
        &mut once_state,
        base,
        None,
        None,
        vec![PromptChoice::AllowOnce],
        once_prompts,
    );
    match once_gate.ask(&[exec_request("bash")]) {
        newt_core::PermissionDecision::Allow(c) => {
            assert!(
                c.permits_exec("bash"),
                "allow-once grants the target for this op"
            );
        }
        newt_core::PermissionDecision::Deny => {
            panic!("allow-once of a high-danger target must still be permitted")
        }
    }
    drop(once_gate);
    assert!(once_state.session_grants.is_empty());
}

fn ocap(verdict: newt_core::ocap_store::Verdict, toml: &str) -> newt_core::ocap_store::PolicySet {
    newt_core::ocap_store::build_store(&[(verdict, Some(toml.to_string()))]).0
}

#[test]
fn durable_ocap_approve_allows_without_prompting_and_grants_authority() {
    let mut state = PermissionPromptState {
        ocap_policy: ocap(
            newt_core::ocap_store::Verdict::Approve,
            "[[exec]]\ntarget = \"git\"\n",
        ),
        ..Default::default()
    };
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![], // any prompt would panic (script exhausted)
        prompts.clone(),
    );
    match gate.ask(&[exec_request("git")]) {
        newt_core::PermissionDecision::Allow(c) => assert!(
            c.permits_exec("git"),
            "a durable approve must fold `git` into the minted authority"
        ),
        newt_core::PermissionDecision::Deny => panic!("durable approve must allow"),
    }
    assert_eq!(prompts.get(), 0, "durable approve must NOT prompt");
    drop(gate);
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].decision, "allow");
    assert_eq!(state.decisions[0].scope, "ocap-approve");
    assert!(state.session_grants.is_empty());
}

/// Real-resource grounding for the core read/write-mode regression and the
/// scripted durable-grant test above: persist a genuinely signed policy, load
/// it through the production verifier, and inspect the gate's minted authority.
/// Changing the saved write bit without re-signing must invalidate the grant.
/// The policy and operator key exist only inside this test's temporary directory.
#[test]
fn durable_signed_readonly_fs_approval_prompts_before_granting_write_authority() {
    use newt_core::ocap_store::{evaluate_request, load_store, sign_approves, PolicyFile, Verdict};

    let root = tempfile::tempdir().unwrap();
    let config_path = root.path().join("config.toml");
    let key_path = root.path().join("identity.pem");
    let key = newt_identity::UserKey::generate();
    key.save(&key_path).unwrap();
    let mut file = PolicyFile::parse("[[fs]]\npath = \"/fixture/notes.txt\"\n").unwrap();
    let (signed, refused) = sign_approves(
        &mut file,
        |_, _| false,
        |payload| key.sign(payload).to_bytes(),
    );
    assert_eq!(signed, 1);
    assert!(refused.is_empty());
    let policy_dir = root.path().join("ocap");
    std::fs::create_dir(&policy_dir).unwrap();
    std::fs::write(
        policy_dir.join(Verdict::Approve.filename()),
        file.to_toml().unwrap(),
    )
    .unwrap();
    let (ocap_policy, warnings) = load_store(&config_path, Some(key.public().as_bytes()));
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut state = PermissionPromptState {
        ocap_policy,
        ..Default::default()
    };
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/workspace"),
        Some(key_path),
        None,
        vec![PromptChoice::Deny],
        prompts.clone(),
    );
    let read = PermissionRequest {
        tool: "read_file".into(),
        kind: DenialKind::FsRead,
        target: "/fixture/notes.txt".into(),
        reason: String::new(),
    };
    match gate.ask(std::slice::from_ref(&read)) {
        newt_core::PermissionDecision::Allow(caveats) => {
            assert!(caveats.permits_fs_read(&read.target));
            assert!(!caveats.permits_fs_write(&read.target));
        }
        newt_core::PermissionDecision::Deny => panic!("verified read approval must allow reads"),
    }
    assert_eq!(
        prompts.get(),
        0,
        "verified reads do not need another prompt"
    );
    let write = PermissionRequest {
        tool: "write_file".into(),
        kind: DenialKind::FsWrite,
        ..read
    };
    assert!(matches!(
        gate.ask(&[write]),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(
        prompts.get(),
        1,
        "the operator must decide the new write grant"
    );
    drop(gate);
    assert!(state.session_grants.is_empty());
    assert_eq!(state.decisions.len(), 2);
    assert_eq!(state.decisions[0].scope, "ocap-approve");
    assert_eq!(state.decisions[1].decision, "deny");

    // Retain the original signature while tampering with its authority-bearing
    // write bit. Reload from disk rather than mutating the verified live policy.
    file.fs[0].write = true;
    std::fs::write(
        policy_dir.join(Verdict::Approve.filename()),
        file.to_toml().unwrap(),
    )
    .unwrap();
    let (tampered, warnings) = load_store(&config_path, Some(key.public().as_bytes()));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("approve entry `/fixture/notes.txt` dropped:"),
        "{warnings:?}"
    );
    for kind in [DenialKind::FsRead, DenialKind::FsWrite] {
        assert_eq!(
            evaluate_request(&tampered, kind, "/fixture/notes.txt"),
            None,
            "a tampered signature must not authorize either access mode"
        );
    }
}

#[test]
fn durable_ocap_deny_refuses_without_prompting() {
    let mut state = PermissionPromptState {
        ocap_policy: ocap(
            newt_core::ocap_store::Verdict::Deny,
            "[[exec]]\ntarget = \"git\"\n",
        ),
        ..Default::default()
    };
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![],
        prompts.clone(),
    );
    assert!(
        matches!(
            gate.ask(&[exec_request("git")]),
            newt_core::PermissionDecision::Deny
        ),
        "a durable deny must refuse"
    );
    assert_eq!(prompts.get(), 0, "durable deny must NOT prompt");
}

#[test]
fn durable_ocap_approve_of_high_danger_still_prompts() {
    let mut state = PermissionPromptState {
        ocap_policy: ocap(
            newt_core::ocap_store::Verdict::Approve,
            "[[exec]]\ntarget = \"bash\"\n",
        ),
        ..Default::default()
    };
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![PromptChoice::Deny], // the human still gets to decide
        prompts.clone(),
    );
    assert!(
        matches!(
            gate.ask(&[exec_request("bash")]),
            newt_core::PermissionDecision::Deny
        ),
        "a durable approve must not bypass the danger prompt for an interpreter"
    );
    assert_eq!(
        prompts.get(),
        1,
        "high-danger falls through to the human even with a durable approve"
    );
}

#[test]
fn permanently_deny_persists_and_reloads_without_reprompting() {
    let dir = tempfile::TempDir::new().unwrap();
    let denials = dir.path().join("permission-denials.jsonl");
    let base = base_caveats("/ws");
    let net_req = newt_core::PermissionRequest {
        tool: "web_fetch".to_string(),
        kind: DenialKind::Net,
        target: "evil.example.com".to_string(),
        reason: "net does not permit 'evil.example.com'".to_string(),
    };

    let mut state = PermissionPromptState::default();
    {
        let mut script = vec![PromptChoice::DenyPermanent].into_iter();
        let mut gate = PromptPermissionGate {
            state: &mut state,
            base: base.clone(),
            key_path: None,
            conversation_id: "conv-904".to_string(),
            log_path: None,
            denials_path: Some(denials.clone()),
            config_path: None,
            preset_clamp: None,
            delegation: None,
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_surface: None,
            #[cfg(feature = "rich-tui")]
            open_panel: None,
            ask_human: move |_w: &PromptWindow, _d: &SurfaceInteraction| {
                script.next().expect("script exhausted")
            },
        };
        assert!(matches!(
            gate.ask(std::slice::from_ref(&net_req)),
            newt_core::PermissionDecision::Deny
        ));
    }
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].decision, "deny");
    assert_eq!(state.decisions[0].scope, "permanent");
    assert_eq!(
        newt_core::load_denials(&denials),
        vec![(DenialKind::Net, "evil.example.com".to_string())],
        "the permanent deny was written to disk"
    );

    let mut fresh = PermissionPromptState::with_persistent_denials(Some(&denials));
    {
        let mut gate = PromptPermissionGate {
            state: &mut fresh,
            base,
            key_path: None,
            conversation_id: "conv-904b".to_string(),
            log_path: None,
            denials_path: Some(denials.clone()),
            config_path: None,
            preset_clamp: None,
            delegation: None,
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_surface: None,
            #[cfg(feature = "rich-tui")]
            open_panel: None,
            ask_human: |_w: &PromptWindow, _d: &SurfaceInteraction| {
                panic!("must NOT prompt: target was permanently denied")
            },
        };
        assert!(matches!(
            gate.ask(std::slice::from_ref(&net_req)),
            newt_core::PermissionDecision::Deny
        ));
    }
    assert!(fresh.decisions.is_empty());
}

#[test]
fn permanent_allow_offered_for_net_only() {
    let danger = danger::DangerTable::builtin();
    let net = plain::render(&permission_definition(
        &PermissionRequest {
            tool: "web_fetch".to_string(),
            kind: DenialKind::Net,
            target: "github.com".to_string(),
            reason: String::new(),
        },
        &danger,
        Audience::Terminal,
    ));
    let exec = plain::render(&permission_definition(
        &exec_request("npm"),
        &danger,
        Audience::Terminal,
    ));
    assert!(
        net.contains("[A]llow permanently"),
        "net must offer it: {net}"
    );
    assert!(
        !exec.contains("[A]llow permanently"),
        "exec must NOT: {exec}"
    );
    assert!(net.contains("[P]ermanently deny") && exec.contains("[P]ermanently deny"));
}

#[test]
fn allow_permanently_grants_now_and_persists_host_to_config() {
    for (preset, wildcard, force_full_access) in [
        ("workspace_dev", false, false),
        ("workspace_dev", true, false),
        ("workspace_dev", false, true),
        ("full_access", false, false),
    ] {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        let net = if wildcard { r#"["*"]"# } else { "[]" };
        std::fs::write(
            &config,
            format!("# my config\n[tui.permissions]\npreset = \"{preset}\"\nnet = {net}\n"),
        )
        .unwrap();
        let base = if force_full_access {
            Caveats::top()
        } else {
            crate::migration_notices::read(|report| newt_core::Config::load(&config, report))
                .unwrap()
                .tui
                .unwrap()
                .permissions
                .to_caveats("/ws")
        };
        let net_req = newt_core::PermissionRequest {
            tool: "mcp connect".to_string(),
            kind: DenialKind::Net,
            target: "github.com".to_string(),
            reason: "net does not permit 'github.com'".to_string(),
        };

        let mut state = PermissionPromptState::default();
        {
            let mut script = vec![PromptChoice::AllowPermanent].into_iter();
            let mut gate = PromptPermissionGate {
                state: &mut state,
                base,
                key_path: None,
                conversation_id: "conv-904a".to_string(),
                log_path: None,
                denials_path: None,
                config_path: Some(config.clone()),
                preset_clamp: None,
                delegation: None,
                danger: danger::DangerTable::builtin(),
                color: false,
                verbose: false,
                authorization_prompts_enabled: true,
                web_decision_timeout: Duration::from_secs(2),
                cancel: None,
                exit: None,
                ask_surface: None,
                #[cfg(feature = "rich-tui")]
                open_panel: None,
                ask_human: move |_w: &PromptWindow, _d: &SurfaceInteraction| {
                    script.next().expect("script exhausted")
                },
            };
            match gate.ask(std::slice::from_ref(&net_req)) {
                newt_core::PermissionDecision::Allow(c) => {
                    assert!(c.permits_net("github.com"), "granted this session");
                }
                newt_core::PermissionDecision::Deny => {
                    panic!("permanent-allow of a net host must be granted")
                }
            }
        }
        assert!(state
            .session_grants
            .contains(&(DenialKind::Net, "github.com".to_string())));
        assert_eq!(state.decisions[0].scope, "permanent");
        let written = std::fs::read_to_string(&config).unwrap();
        assert!(written.contains("# my config"), "comment lost: {written}");
        assert!(
            written.contains("github.com"),
            "host not persisted: {written}"
        );
        let permissions =
            crate::migration_notices::read(|report| newt_core::Config::load(&config, report))
                .unwrap()
                .tui
                .unwrap()
                .permissions;
        assert!(permissions.net.contains(&"github.com".to_string()));
        let reloaded = if force_full_access {
            Caveats::top()
        } else {
            permissions.to_caveats("/ws")
        };
        let policy = newt_mcp_client::HttpNetworkPolicy::with_explicit_hosts(
            &reloaded.net,
            &permissions.net,
        );
        assert!(
            policy.explicitly_grants_host("github.com"),
            "a fresh session honors permanent MCP grants in every mode"
        );
    }
}

#[test]
fn refresh_caveats_includes_new_session_filesystem_grants_without_prompting() {
    for kind in [DenialKind::FsRead, DenialKind::FsWrite] {
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let base = base_caveats("/ws");
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![PromptChoice::AllowSession],
            prompts.clone(),
        );
        let request = PermissionRequest {
            tool: "request_permissions".into(),
            kind,
            target: "/approved/config.toml".into(),
            reason: "read or update the requested configuration".into(),
        };
        assert!(matches!(
            gate.ask(&[request]),
            newt_core::PermissionDecision::Allow(_)
        ));
        let newt_core::PermissionDecision::Allow(refreshed) = gate.refresh_caveats(&base) else {
            panic!("existing session authority must be refreshable");
        };
        assert_eq!(
            refreshed.permits_fs_read("/approved/config.toml"),
            kind == DenialKind::FsRead
        );
        assert_eq!(
            refreshed.permits_fs_write("/approved/config.toml"),
            kind == DenialKind::FsWrite
        );
        assert!(!refreshed.permits_fs_read("/approved/sibling.toml"));
        assert!(!refreshed.permits_fs_write("/approved/sibling.toml"));
        assert!(newt_core::caveats::permits_path(
            &refreshed.fs_read,
            "/ws/source.rs"
        ));
        assert_eq!(refreshed.exec, base.exec);
        assert_eq!(refreshed.net, base.net);
        assert_eq!(prompts.get(), 1, "refresh must not ask again");
        assert_eq!(gate.state.decisions.len(), 1, "refresh is not a decision");
    }
}

#[test]
fn refresh_caveats_preserves_a_narrower_caller_baseline_while_adding_session_grants() {
    let baseline = Caveats {
        fs_write: Scope::none(),
        net: Scope::only(["allowed.test".into()]),
        max_calls: CountBound::AtMost(2),
        valid_for_generation: Scope::only([7]),
        ..base_caveats("/ws")
    };
    let gate_base = Caveats {
        fs_read: Scope::only(["/ws".into(), "/gate-only".into()]),
        fs_write: Scope::only(["/ws".into(), "/gate-only".into()]),
        exec: Scope::only(["cargo".into(), "npm".into()]),
        net: Scope::only(["allowed.test".into(), "gate-only.test".into()]),
        ..Caveats::top()
    };
    assert!(baseline.leq(&gate_base));
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        gate_base,
        None,
        None,
        vec![PromptChoice::AllowSession],
        prompts.clone(),
    );
    let request = PermissionRequest {
        tool: "request_permissions".into(),
        kind: DenialKind::FsRead,
        target: "/approved/config.toml".into(),
        reason: "read the requested configuration".into(),
    };
    assert!(matches!(
        gate.ask(&[request]),
        newt_core::PermissionDecision::Allow(_)
    ));
    let newt_core::PermissionDecision::Allow(refreshed) = gate.refresh_caveats(&baseline) else {
        panic!("the standing grant must compose with the caller's narrower baseline");
    };
    assert_eq!(
        refreshed,
        Caveats {
            fs_read: Scope::only(["/ws".into(), "/approved/config.toml".into()]),
            ..baseline
        },
        "only the approved read target may widen; retain every other caller bound"
    );
    assert!(!refreshed.permits_fs_read("/approved/sibling.toml"));
    assert_eq!(prompts.get(), 1, "refresh must not ask again");
    assert_eq!(gate.state.decisions.len(), 1);
}

#[test]
fn refresh_caveats_neither_publishes_nor_consumes_pending_once_grants() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let base = base_caveats("/ws");
    let mut gate = scripted_gate(
        &mut state,
        base.clone(),
        None,
        None,
        vec![PromptChoice::AllowOnce],
        prompts.clone(),
    );
    let mut request = exec_request("/opt/test-venv/bin/python");
    request.tool = "request_permissions".into();
    assert!(matches!(
        gate.ask(std::slice::from_ref(&request)),
        newt_core::PermissionDecision::Allow(_)
    ));
    let pending = gate.state.pending_once_grants.clone();
    assert_eq!(pending.len(), 1);
    for _ in 0..2 {
        let newt_core::PermissionDecision::Allow(refreshed) = gate.refresh_caveats(&base) else {
            panic!("refresh must preserve the baseline");
        };
        assert_eq!(
            refreshed, base,
            "one-shot authority is not session authority"
        );
        assert_eq!(gate.state.pending_once_grants, pending);
        assert_eq!(gate.state.decisions.len(), 1);
    }
    request.tool = "run_command".into();
    let newt_core::PermissionDecision::Allow(once) = gate.ask(&[request]) else {
        panic!("the exact operation must still consume its pending grant");
    };
    assert!(once.permits_exec("/opt/test-venv/bin/python"));
    assert!(gate.state.pending_once_grants.is_empty());
    assert!(gate.state.session_grants.is_empty());
    assert_eq!(prompts.get(), 1);
    let newt_core::PermissionDecision::Allow(refreshed) = gate.refresh_caveats(&base) else {
        panic!("refresh must preserve the baseline after the one-shot call");
    };
    assert_eq!(refreshed, base);
}

#[test]
fn refresh_caveats_reapplies_preset_and_delegation_ceilings() {
    use crate::caveat_policy_tests::verified_delegation;
    let ceiling = Caveats {
        fs_read: Scope::only(["/ws".into(), "/approved/config.toml".into()]),
        fs_write: Scope::none(),
        exec: Scope::none(),
        ..base_caveats("/ws")
    };
    let delegation = verified_delegation(ceiling.clone());
    for delegated in [false, true] {
        let mut state = PermissionPromptState::default();
        state.session_grants.extend([
            (DenialKind::FsRead, "/approved/config.toml".into()),
            (DenialKind::FsRead, "/outside/secret.txt".into()),
            (DenialKind::FsWrite, "/approved/config.toml".into()),
            (DenialKind::Exec, "npm".into()),
        ]);
        let prompts = Rc::new(Cell::new(0));
        let base = base_caveats("/ws");
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![],
            prompts.clone(),
        );
        if delegated {
            gate.delegation = Some(&delegation);
        } else {
            gate.preset_clamp = Some(ceiling.clone());
        }
        let newt_core::PermissionDecision::Allow(refreshed) = gate.refresh_caveats(&base) else {
            panic!("refresh must retain authority inside the ceiling");
        };
        assert!(refreshed.permits_fs_read("/approved/config.toml"));
        assert!(
            refreshed.leq(&ceiling),
            "delegated={delegated}: {refreshed:?}"
        );
        assert!(!refreshed.permits_fs_read("/outside/secret.txt"));
        assert!(!refreshed.permits_fs_write("/approved/config.toml"));
        assert!(!refreshed.permits_exec("cargo"));
        assert!(!refreshed.permits_exec("npm"));
        assert_eq!(prompts.get(), 0);
        assert!(gate.state.decisions.is_empty());
    }
}

#[test]
fn refresh_caveats_preserves_denials_and_filters_conflicting_cached_grants() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let base = base_caveats("/ws");
    let mut gate = scripted_gate(
        &mut state,
        base.clone(),
        None,
        None,
        vec![PromptChoice::AllowSession, PromptChoice::DenyAlways],
        prompts.clone(),
    );
    let mut request = PermissionRequest {
        tool: "request_permissions".into(),
        kind: DenialKind::FsRead,
        target: "/approved".into(),
        reason: "inspect the requested configuration".into(),
    };
    assert!(matches!(
        gate.ask(std::slice::from_ref(&request)),
        newt_core::PermissionDecision::Allow(_)
    ));
    request.target = "/approved/denied.toml".into();
    assert!(matches!(
        gate.ask(std::slice::from_ref(&request)),
        newt_core::PermissionDecision::Deny
    ));
    let newt_core::PermissionDecision::Allow(refreshed) = gate.refresh_caveats(&base) else {
        panic!("a denial must not remove independent baseline authority");
    };
    assert_eq!(
        refreshed, base,
        "the broad recalled grant conflicts with a denial"
    );
    assert!(matches!(
        gate.ask(&[request]),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 2, "refresh cannot clear an operator refusal");
    assert_eq!(gate.state.decisions.len(), 2);
}

#[test]
fn allow_once_grants_one_call_and_reprompts_next_time() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let base = base_caveats("/ws");
    let mut gate = scripted_gate(
        &mut state,
        base.clone(),
        None,
        None,
        vec![PromptChoice::AllowOnce, PromptChoice::AllowOnce],
        prompts.clone(),
    );
    let req = [exec_request("npm")];
    match gate.ask(&req) {
        newt_core::PermissionDecision::Allow(c) => {
            assert!(c.permits_exec("npm"), "the grant covers the target");
            assert!(c.permits_exec("cargo"), "baseline grants kept");
            assert!(!c.permits_exec("rm"), "nothing else widened");
        }
        newt_core::PermissionDecision::Deny => panic!("expected allow"),
    }
    assert_eq!(prompts.get(), 1);
    assert!(matches!(
        gate.ask(&req),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(prompts.get(), 2, "allow-once re-prompts on the next call");
    drop(gate);
    assert!(state.session_grants.is_empty());
    assert_eq!(state.decisions.len(), 2);
    assert_eq!(state.decisions[0].decision, "allow");
    assert_eq!(state.decisions[0].scope, "once");
}

#[test]
fn request_permissions_allow_once_carries_to_the_run_command_retry() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let base = base_caveats("/ws");
    let mut gate = scripted_gate(
        &mut state,
        base,
        None,
        None,
        vec![PromptChoice::AllowOnce, PromptChoice::AllowOnce],
        prompts.clone(),
    );
    let ask = PermissionRequest {
        tool: "request_permissions".to_string(),
        kind: DenialKind::Exec,
        target: "/usr/bin/python3".to_string(),
        reason: "need to run the tests".to_string(),
    };
    assert!(matches!(
        gate.ask(&[ask]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(prompts.get(), 1);
    match gate.ask(&[exec_request("/usr/bin/python3")]) {
        newt_core::PermissionDecision::Allow(c) => {
            assert!(
                c.permits_exec("/usr/bin/python3"),
                "the carried grant widened the caveats so the retry runs"
            );
        }
        newt_core::PermissionDecision::Deny => panic!("carried grant should cover the retry"),
    }
    assert_eq!(
        prompts.get(),
        1,
        "no second prompt — the pending grant covered the /usr/bin/python3 retry"
    );
    assert!(matches!(
        gate.ask(&[exec_request("/usr/bin/python3")]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(
        prompts.get(),
        2,
        "the one-shot pending grant was consumed; the next op re-prompts"
    );
}

#[test]
fn exact_executable_grants_preserve_immediate_and_proactive_once_authority() {
    for proactive in [false, true] {
        let target = "/opt/test-venv/bin/python";
        let other = "/opt/other-venv/bin/python";
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let base = base_caveats("/ws");
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![
                PromptChoice::AllowOnce,
                PromptChoice::Deny,
                PromptChoice::Deny,
            ],
            prompts.clone(),
        );
        let mut request = exec_request(target);
        if proactive {
            request.tool = "request_permissions".into();
        }
        let newt_core::PermissionDecision::Allow(granted) = gate.ask(&[request]) else {
            panic!("exact executable should be grantable once");
        };
        assert_eq!(granted.exec, Scope::only(["cargo".into(), target.into()]));
        assert_eq!(granted.fs_read, base.fs_read);
        assert_eq!(granted.fs_write, base.fs_write);
        assert_eq!(granted.net, base.net);
        // A different executable with the same basename cannot consume the grant.
        assert!(matches!(
            gate.ask(&[exec_request(other)]),
            newt_core::PermissionDecision::Deny
        ));
        assert_eq!(prompts.get(), 2);
        if proactive {
            let newt_core::PermissionDecision::Allow(retry) = gate.ask(&[exec_request(target)])
            else {
                panic!("the exact pending grant must survive an unrelated request");
            };
            assert_eq!(retry.exec, granted.exec);
            assert_eq!(
                prompts.get(),
                2,
                "the exact retry consumes the pending approval"
            );
        }
        assert!(matches!(
            gate.ask(&[exec_request(target)]),
            newt_core::PermissionDecision::Deny
        ));
        assert_eq!(prompts.get(), 3, "allow once never becomes sticky");
        drop(gate);
        assert!(state.pending_once_grants.is_empty());
        assert!(state.session_grants.is_empty());
    }
}

#[test]
fn exact_executable_grants_respect_existing_basename_denials() {
    for source in ["session", "persistent", "ocap"] {
        let mut state = PermissionPromptState::default();
        let denied = (DenialKind::Exec, "python".into());
        match source {
            "session" => {
                state.session_denials.insert(denied);
            }
            "persistent" => {
                state.persistent_denials.insert(denied);
            }
            _ => {
                state.ocap_policy = ocap(
                    newt_core::ocap_store::Verdict::Deny,
                    "[[exec]]\ntarget = \"python\"\n",
                );
            }
        }
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            base_caveats("/ws"),
            None,
            None,
            vec![PromptChoice::AllowOnce],
            prompts.clone(),
        );
        assert!(matches!(
            gate.ask(&[exec_request("/opt/test-venv/bin/python")]),
            newt_core::PermissionDecision::Deny
        ));
        assert_eq!(
            prompts.get(),
            0,
            "a previous deny must not become a new prompt"
        );
    }
}

#[test]
fn session_grant_exec_requires_exact_target() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let targets = ["mytool", "/opt/bin/mytool", "/tools/bin/mytool"];
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![PromptChoice::AllowSession; targets.len()],
        prompts.clone(),
    );
    for (index, target) in targets.iter().enumerate() {
        let mut request = exec_request(target);
        request.tool = "request_permissions".into();
        let newt_core::PermissionDecision::Allow(granted) = gate.ask(&[request]) else {
            panic!("the operator approved {target}");
        };
        assert!(
            granted.permits_exec(target),
            "mint the approved exact target"
        );
        assert_eq!(prompts.get(), index + 1, "distinct targets need decisions");
        for unapproved in &targets[index + 1..] {
            assert!(!granted.permits_exec(unapproved));
        }
        let newt_core::PermissionDecision::Allow(reused) = gate.ask(&[exec_request(target)]) else {
            panic!("the identical target must reuse its session approval");
        };
        assert_eq!(reused, granted);
        assert_eq!(prompts.get(), index + 1, "exact reuse must not prompt");
    }
}

#[test]
fn bare_session_exec_grant_does_not_bypass_an_absolute_request_denial() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![PromptChoice::AllowSession, PromptChoice::Deny],
        prompts.clone(),
    );
    assert!(matches!(
        gate.ask(&[exec_request("mytool")]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert!(matches!(
        gate.ask(&[exec_request("/opt/bin/mytool")]),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 2);
}

#[test]
fn bare_pending_once_exec_grant_survives_an_absolute_request_denial() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![
            PromptChoice::AllowOnce,
            PromptChoice::Deny,
            PromptChoice::Deny,
        ],
        prompts.clone(),
    );
    let mut request = exec_request("mytool");
    request.tool = "request_permissions".into();
    assert!(matches!(
        gate.ask(&[request]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert!(matches!(
        gate.ask(&[exec_request("/opt/bin/mytool")]),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(
        prompts.get(),
        2,
        "a different target needs its own decision"
    );
    let newt_core::PermissionDecision::Allow(granted) = gate.ask(&[exec_request("mytool")]) else {
        panic!("the exact pending grant must survive a different target");
    };
    assert!(granted.permits_exec("mytool"));
    assert!(!granted.permits_exec("/opt/bin/mytool"));
    assert_eq!(prompts.get(), 2, "exact retry consumes the pending grant");
    assert!(matches!(
        gate.ask(&[exec_request("mytool")]),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 3, "the pending grant is consumed only once");
    drop(gate);
    assert!(state.pending_once_grants.is_empty());
}

#[test]
fn full_path_session_grant_does_not_cover_a_bare_name() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![PromptChoice::AllowSession, PromptChoice::AllowSession],
        prompts.clone(),
    );
    assert!(matches!(
        gate.ask(&[exec_request("/opt/bin/mytool")]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(prompts.get(), 1);
    assert!(matches!(
        gate.ask(&[exec_request("mytool")]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(
        prompts.get(),
        2,
        "full-path grant must not widen to a bare name (pin-exact)"
    );
}

#[test]
fn git_write_grant_refused_under_readonly_preset() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let clamp = newt_core::NamedPermissionPreset {
        readonly: true,
        ..Default::default()
    }
    .clamp();
    let base = base_caveats("/ws").meet(&clamp);
    let mut gate = scripted_gate(
        &mut state,
        base,
        None,
        None,
        vec![PromptChoice::AllowOnce],
        prompts.clone(),
    );
    gate.preset_clamp = Some(clamp);
    let req = PermissionRequest {
        tool: "git".to_string(),
        kind: DenialKind::GitWrite,
        target: "commit".to_string(),
        reason: "commit the work".to_string(),
    };
    assert!(
        matches!(gate.ask(&[req]), newt_core::PermissionDecision::Deny),
        "a readonly preset must refuse a git-write grant"
    );
    assert_eq!(prompts.get(), 0, "the floor refuses WITHOUT prompting");
}

#[test]
fn git_write_grant_allowed_without_a_preset() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![PromptChoice::AllowOnce],
        prompts.clone(),
    );
    let req = PermissionRequest {
        tool: "git".to_string(),
        kind: DenialKind::GitWrite,
        target: "commit".to_string(),
        reason: "commit the work".to_string(),
    };
    assert!(matches!(
        gate.ask(&[req]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(prompts.get(), 1);
}

#[test]
fn session_grant_cannot_pierce_the_preset_floor() {
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let clamp = newt_core::NamedPermissionPreset {
        readonly: true,
        ..Default::default()
    }
    .clamp();
    let base = base_caveats("/ws").meet(&clamp);
    assert!(
        !base.permits_exec("cargo"),
        "the preset clamped exec to none"
    );

    let mut gate = scripted_gate(
        &mut state,
        base.clone(),
        None,
        None,
        vec![PromptChoice::AllowOnce, PromptChoice::AllowSession],
        prompts.clone(),
    );
    gate.preset_clamp = Some(clamp.clone());
    match gate.ask(&[exec_request("rm")]) {
        newt_core::PermissionDecision::Allow(c) => {
            assert!(
                !c.permits_exec("rm"),
                "a once-grant must not pierce the preset floor: {c:?}"
            );
            assert!(!c.permits_exec("cargo"), "floor keeps exec denied");
        }
        newt_core::PermissionDecision::Deny => panic!("the gate allowed-once"),
    }
    match gate.ask(&[exec_request("rm")]) {
        newt_core::PermissionDecision::Allow(c) => {
            assert!(
                !c.permits_exec("rm"),
                "a SESSION grant must not pierce the floor either: {c:?}"
            );
        }
        newt_core::PermissionDecision::Deny => panic!("the gate allowed-session"),
    }
    drop(gate);
    assert!(state
        .session_grants
        .contains(&(DenialKind::Exec, "rm".to_string())));
}

#[test]
fn allow_session_never_reprompts_until_restart() {
    let prompts = Rc::new(Cell::new(0));
    let base = base_caveats("/ws");
    let mut state = PermissionPromptState::default();
    {
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![PromptChoice::AllowSession],
            prompts.clone(),
        );
        let req = [exec_request("npm")];
        assert!(matches!(
            gate.ask(&req),
            newt_core::PermissionDecision::Allow(_)
        ));
        assert_eq!(prompts.get(), 1);
        assert!(matches!(
            gate.ask(&req),
            newt_core::PermissionDecision::Allow(_)
        ));
    }
    {
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![],
            prompts.clone(),
        );
        match gate.ask(&[exec_request("npm")]) {
            newt_core::PermissionDecision::Allow(c) => assert!(c.permits_exec("npm")),
            newt_core::PermissionDecision::Deny => panic!("session grant must hold"),
        }
    }
    assert_eq!(prompts.get(), 1, "exactly one prompt for the whole session");
    assert_eq!(state.decisions.len(), 1, "re-uses are not re-recorded");
    let mut fresh = PermissionPromptState::default();
    let mut gate = scripted_gate(
        &mut fresh,
        base,
        None,
        None,
        vec![PromptChoice::Deny],
        prompts.clone(),
    );
    assert!(matches!(
        gate.ask(&[exec_request("npm")]),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 2, "the grant did not survive the restart");
}

#[test]
fn deny_always_short_circuits_later_asks() {
    let prompts = Rc::new(Cell::new(0));
    let mut state = PermissionPromptState::default();
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![PromptChoice::DenyAlways],
        prompts.clone(),
    );
    let req = [exec_request("rm")];
    assert!(matches!(
        gate.ask(&req),
        newt_core::PermissionDecision::Deny
    ));
    assert!(matches!(
        gate.ask(&req),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 1, "second ask auto-denied without a prompt");
    drop(gate);
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].decision, "deny");
    assert_eq!(state.decisions[0].scope, "session");
}

#[test]
fn batch_deny_and_empty_requests_deny() {
    let prompts = Rc::new(Cell::new(0));
    let mut state = PermissionPromptState::default();
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        None,
        vec![PromptChoice::AllowOnce, PromptChoice::Deny],
        prompts.clone(),
    );
    let reqs = [exec_request("npm"), exec_request("rm")];
    assert!(matches!(
        gate.ask(&reqs),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 2, "asked per target until the deny");
    assert!(matches!(gate.ask(&[]), newt_core::PermissionDecision::Deny));
    assert_eq!(prompts.get(), 2, "empty batch never prompts");
}

#[serial_test::serial(real_fs)]
#[test]
fn decisions_are_recorded_to_the_session_log() {
    let dir = tempfile::TempDir::new().unwrap();
    let log = dir.path().join("permission-log.jsonl");
    let prompts = Rc::new(Cell::new(0));
    let mut state = PermissionPromptState::default();
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        None,
        Some(log.clone()),
        vec![
            PromptChoice::AllowOnce,
            PromptChoice::AllowSession,
            PromptChoice::Deny,
        ],
        prompts.clone(),
    );
    let _ = gate.ask(&[exec_request("npm")]);
    let _ = gate.ask(&[PermissionRequest {
        tool: "web_fetch".to_string(),
        kind: DenialKind::Net,
        target: "docs.rs".to_string(),
        reason: String::new(),
    }]);
    let _ = gate.ask(&[exec_request("rm")]);
    let body = std::fs::read_to_string(&log).unwrap();
    let records: Vec<newt_core::PermissionRecord> = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    assert!(records.iter().all(|r| r.conversation_id == "conv-test"));
    assert_eq!(
        (
            records[0].tool.as_str(),
            records[0].kind.as_str(),
            records[0].target.as_str()
        ),
        ("run_command", "exec", "npm")
    );
    assert_eq!(
        (records[0].decision.as_str(), records[0].scope.as_str()),
        ("allow", "once")
    );
    assert_eq!(
        (records[1].kind.as_str(), records[1].scope.as_str()),
        ("net", "session")
    );
    assert_eq!(
        (records[2].decision.as_str(), records[2].scope.as_str()),
        ("deny", "once")
    );
    assert_eq!(state.decisions, records);
}

#[serial_test::serial(real_fs)]
#[test]
fn allow_remints_from_the_user_root_and_never_widens_the_baseline() {
    let dir = tempfile::TempDir::new().unwrap();
    let key_path = dir.path().join("identity.pem");
    let prompts = Rc::new(Cell::new(0));
    let base = base_caveats("/ws");
    let mut state = PermissionPromptState::default();
    let mut gate = scripted_gate(
        &mut state,
        base.clone(),
        Some(key_path.clone()),
        None,
        vec![PromptChoice::AllowSession],
        prompts.clone(),
    );
    let minted = match gate.ask(&[exec_request("npm")]) {
        newt_core::PermissionDecision::Allow(c) => c,
        newt_core::PermissionDecision::Deny => panic!("expected allow"),
    };
    assert!(
        key_path.exists(),
        "the user root key was used for the re-mint"
    );
    assert!(minted.permits_exec("npm"));
    assert!(minted.permits_exec("cargo"));
    assert!(!minted.permits_exec("rm"));
    drop(gate);
    assert_eq!(base, base_caveats("/ws"));
    let policy = newt_core::widen_caveats(&base, &[(DenialKind::Exec, "npm".to_string())]);
    let key = mint_operating_key(&key_path, &policy).unwrap();
    assert_eq!(newt_identity::enforced_caveats(&key).unwrap(), minted);
}

#[test]
fn delegated_grants_cannot_cross_the_parent_ceiling_or_persist_approval() {
    use crate::caveat_policy_tests::verified_delegation;
    let ceiling = Caveats {
        fs_write: Scope::none(),
        exec: Scope::none(),
        ..base_caveats("/ws")
    };
    let delegation = verified_delegation(ceiling.clone());
    let requests = [
        exec_request("npm"),
        PermissionRequest {
            tool: "read_file".into(),
            kind: DenialKind::FsRead,
            target: "/outside".into(),
            reason: String::new(),
        },
        PermissionRequest {
            tool: "write_file".into(),
            kind: DenialKind::FsWrite,
            target: "/ws".into(),
            reason: String::new(),
        },
        PermissionRequest {
            tool: "web_fetch".into(),
            kind: DenialKind::Net,
            target: "example.com".into(),
            reason: String::new(),
        },
        PermissionRequest {
            tool: "git".into(),
            kind: DenialKind::GitWrite,
            target: "commit".into(),
            reason: String::new(),
        },
        PermissionRequest {
            tool: "remote__write".into(),
            kind: DenialKind::RemoteTool,
            target: "remote__write".into(),
            reason: String::new(),
        },
    ];
    for choice in [
        PromptChoice::AllowOnce,
        PromptChoice::AllowSession,
        PromptChoice::AllowPermanent,
    ] {
        for req in &requests {
            let dir = tempfile::tempdir().unwrap();
            let config_path = dir.path().join("config.toml");
            let key_path = dir.path().join("identity.pem");
            let prompts = Rc::new(Cell::new(0));
            let mut state = PermissionPromptState::default();
            let mut gate = scripted_gate(
                &mut state,
                ceiling.clone(),
                Some(key_path.clone()),
                None,
                vec![choice],
                prompts.clone(),
            );
            gate.delegation = Some(&delegation);
            gate.config_path = Some(config_path.clone());
            if req.kind == DenialKind::Net {
                assert!(gate.ask_mcp_net_grant(req).is_none());
            }
            assert!(
                matches!(
                    gate.ask(std::slice::from_ref(req)),
                    newt_core::PermissionDecision::Deny
                ),
                "delegated {:?} cannot grant {:?}",
                choice,
                req.kind
            );
            drop(gate);
            assert_eq!(
                prompts.get(),
                0,
                "the child cannot ask to remove its parent ceiling"
            );
            assert!(
                !config_path.exists(),
                "denied authority must not be persisted"
            );
            assert!(!key_path.exists(), "the child must not re-root a grant");
            assert!(state.session_grants.is_empty());
            assert!(state.pending_once_grants.is_empty());
            assert!(state
                .decisions
                .iter()
                .all(|record| record.decision == "deny"));
        }
    }
}

#[test]
fn delegated_grants_reject_cached_approvals_outside_the_ceiling() {
    use crate::caveat_policy_tests::verified_delegation;
    let ceiling = Caveats {
        exec: Scope::none(),
        ..base_caveats("/ws")
    };
    let delegation = verified_delegation(ceiling.clone());
    for cached_once in [false, true] {
        let prompts = Rc::new(Cell::new(0));
        let mut state = PermissionPromptState::default();
        let key = (DenialKind::Exec, "npm".to_string());
        if cached_once {
            state.pending_once_grants.insert(key);
        } else {
            state.session_grants.insert(key);
        }
        let mut gate = scripted_gate(
            &mut state,
            ceiling.clone(),
            None,
            None,
            vec![],
            prompts.clone(),
        );
        gate.delegation = Some(&delegation);
        assert!(matches!(
            gate.ask(&[exec_request("npm")]),
            newt_core::PermissionDecision::Deny
        ));
        assert_eq!(prompts.get(), 0);
    }
}

/// Grounds delegated batch preflight against a real config file: a forbidden
/// later request must prevent even a lawful earlier request from opening the
/// mocked prompt or persisting an approval. The three choices also cover both
/// session and proactive allow-once caches.
#[test]
fn delegated_grants_preflight_the_whole_batch_before_approval_side_effects() {
    use crate::caveat_policy_tests::verified_delegation;
    let base = base_caveats("/ws");
    let ceiling = Caveats {
        net: Scope::only(["github.com".to_string()]),
        ..base.clone()
    };
    let delegation = verified_delegation(ceiling.clone());
    let requests = [
        PermissionRequest {
            tool: "request_permissions".into(),
            kind: DenialKind::Net,
            target: "github.com".into(),
            reason: String::new(),
        },
        PermissionRequest {
            tool: "request_permissions".into(),
            ..exec_request("npm")
        },
    ];
    assert!(!base.permits_net(&requests[0].target));
    assert!(ceiling.permits_net(&requests[0].target));
    assert!(!ceiling.permits_exec(&requests[1].target));
    for choice in [
        PromptChoice::AllowPermanent,
        PromptChoice::AllowSession,
        PromptChoice::AllowOnce,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let original = "# batch canary\n[tui.permissions]\nnet = []\n";
        std::fs::write(&config, original).unwrap();
        let prompts = Rc::new(Cell::new(0));
        let mut state = PermissionPromptState::default();
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![choice, PromptChoice::Deny],
            prompts.clone(),
        );
        gate.delegation = Some(&delegation);
        gate.config_path = Some(config.clone());
        assert_ne!(
            gate.danger.classify(requests[0].kind, &requests[0].target),
            danger::DangerTier::High,
            "the earlier request must permit every scripted approval choice"
        );
        assert!(matches!(
            gate.ask(&requests),
            newt_core::PermissionDecision::Deny
        ));
        drop(gate);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
        assert!(state.session_grants.is_empty(), "{choice:?}");
        assert!(state.pending_once_grants.is_empty(), "{choice:?}");
        assert!(state
            .decisions
            .iter()
            .all(|record| record.decision == "deny"));
        assert_eq!(prompts.get(), 0, "batch preflight must precede {choice:?}");
    }
}

/// Grounds the returned-authority clamp against a real missing root-key path.
/// A lawful request must not carry an unrelated cached grant into its receipt.
#[test]
fn delegated_grants_do_not_return_unrelated_cached_authority() {
    use crate::caveat_policy_tests::verified_delegation;
    let ceiling = Caveats {
        exec: Scope::only(["cargo".to_string(), "npm".to_string()]),
        ..base_caveats("/ws")
    };
    let delegation = verified_delegation(ceiling.clone());
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("identity.pem");
    let prompts = Rc::new(Cell::new(0));
    let mut state = PermissionPromptState::default();
    state
        .session_grants
        .insert((DenialKind::FsWrite, "/outside".to_string()));
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        Some(key_path.clone()),
        None,
        vec![PromptChoice::AllowOnce],
        prompts.clone(),
    );
    gate.delegation = Some(&delegation);
    let newt_core::PermissionDecision::Allow(granted) = gate.ask(&[exec_request("npm")]) else {
        panic!("an unrelated cached grant must not prevent a lawful approval");
    };
    assert!(granted.permits_exec("npm"));
    assert!(
        granted.leq(&ceiling),
        "the receipt must clamp all cached grants, not just the requested grant"
    );
    assert!(!granted.permits_fs_write("/outside"));
    assert_eq!(prompts.get(), 1);
    assert!(!key_path.exists(), "the child must not re-root a grant");
}

/// Grounds the durable-approval shortcut's refusal against missing key/config
/// paths. Store signature verification has its own tests; this supplies an
/// already-loaded approval through the same fixture as the ordinary gate test.
#[test]
fn delegated_grants_reject_durable_ocap_approval_outside_the_ceiling() {
    use crate::caveat_policy_tests::verified_delegation;
    let ceiling = Caveats {
        exec: Scope::none(),
        ..base_caveats("/ws")
    };
    let delegation = verified_delegation(ceiling.clone());
    let mut state = PermissionPromptState {
        ocap_policy: ocap(
            newt_core::ocap_store::Verdict::Approve,
            "[[exec]]\ntarget = \"git\"\n",
        ),
        ..Default::default()
    };
    assert_eq!(
        newt_core::ocap_store::evaluate_request(&state.ocap_policy, DenialKind::Exec, "git"),
        Some(newt_core::ocap_store::Verdict::Approve)
    );
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("identity.pem");
    let config_path = dir.path().join("config.toml");
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        ceiling,
        Some(key_path.clone()),
        None,
        vec![], // any prompt would panic (script exhausted)
        prompts.clone(),
    );
    gate.delegation = Some(&delegation);
    gate.config_path = Some(config_path.clone());
    assert_ne!(
        gate.danger.classify(DenialKind::Exec, "git"),
        danger::DangerTier::High,
        "the fixture must reach the durable-approval shortcut"
    );
    assert!(matches!(
        gate.ask(&[exec_request("git")]),
        newt_core::PermissionDecision::Deny
    ));
    drop(gate);
    assert_eq!(prompts.get(), 0);
    assert!(!key_path.exists(), "the child must not re-root a grant");
    assert!(
        !config_path.exists(),
        "denied authority must not be persisted"
    );
    assert!(state.session_grants.is_empty());
    assert!(state.pending_once_grants.is_empty());
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].decision, "deny");
}

/// Grounds bounded approval against a real missing root-key path. The lawful
/// approval must succeed without creating an operator key; a missing-file
/// assertion does not prove absence of reads from an already-existing key.
#[test]
fn delegated_grants_allow_within_the_ceiling_without_creating_a_root_key() {
    use crate::caveat_policy_tests::verified_delegation;
    let ceiling = Caveats {
        exec: Scope::only(["cargo".to_string(), "npm".to_string()]),
        ..base_caveats("/ws")
    };
    let delegation = verified_delegation(ceiling.clone());
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("identity.pem");
    let prompts = Rc::new(Cell::new(0));
    let mut state = PermissionPromptState::default();
    let mut gate = scripted_gate(
        &mut state,
        base_caveats("/ws"),
        Some(key_path.clone()),
        None,
        vec![PromptChoice::AllowOnce],
        prompts.clone(),
    );
    gate.delegation = Some(&delegation);
    let newt_core::PermissionDecision::Allow(granted) = gate.ask(&[exec_request("npm")]) else {
        panic!("a lawful grant within the inherited ceiling must still work");
    };
    assert!(granted.permits_exec("npm"));
    assert!(granted.leq(&ceiling));
    assert_eq!(prompts.get(), 1);
    assert!(
        !key_path.exists(),
        "delegated approval must not create an operator root key"
    );
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn execute_tool_with_tui_gate_allow_once_then_reprompt() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("outside.txt"), "gated contents").unwrap();
    let caveats = base_caveats("/elsewhere");
    let prompts = Rc::new(Cell::new(0));
    let mut state = PermissionPromptState::default();
    let mut gate = scripted_gate(
        &mut state,
        caveats.clone(),
        None,
        None,
        vec![PromptChoice::AllowOnce, PromptChoice::Deny],
        prompts.clone(),
    );
    let args = serde_json::json!({"path": "outside.txt"});
    let out = newt_core::agentic::execute_tool(
        "read_file",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
        None,
        None,
        None,
        None, // memory_source
        Some(&mut gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None,
        None, // code_search
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(out, "gated contents", "allow-once executed the real read");
    assert_eq!(prompts.get(), 1);
    let out = newt_core::agentic::execute_tool(
        "read_file",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
        None,
        None,
        None,
        None, // memory_source
        Some(&mut gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None,
        None, // code_search
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert!(
        out.starts_with("capability denied: fs_read does not permit 'outside.txt'"),
        "got: {out}"
    );
    assert!(out.contains("request_permissions"), "got: {out}");
    assert_eq!(prompts.get(), 2, "allow-once does not stick");
    drop(gate);
    assert_eq!(state.decisions.len(), 2);
}

/// Grounds pending-token matching and call-local re-minting in a real confined
/// copy, including an unrelated command and a subsequent executable approval.
#[cfg(target_os = "macos")]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn native_once_filesystem_pending_grants_survive_until_the_declared_retry() {
    use crate::disable_ocap_session_tests::EnvVar;

    async fn dispatch(
        name: &str,
        args: serde_json::Value,
        root: &std::path::Path,
        base: &Caveats,
        gate: &mut dyn newt_core::PermissionGate,
    ) -> String {
        newt_core::execute_tool(
            name,
            &args,
            &root.to_string_lossy(),
            false,
            20,
            base,
            &mut Mcp::empty(),
            None,
            None,
            None,
            None,
            Some(gate),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
    }

    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
    let _full = EnvVar::unset("NEWT_FULL_ACCESS");
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let private = outside.path().canonicalize().unwrap();
    let input = private.join("input.txt");
    let output = private.join("output.txt");
    std::fs::write(&input, "QUEUED_ONCE_CONTENT\n").unwrap();
    std::fs::write(&output, "BEFORE\n").unwrap();
    let baseline = Caveats {
        exec: Scope::only(["/bin/echo".into()]),
        ..newt_core::confined_exec::workspace_confined_caveats(&root)
    };
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        baseline.clone(),
        None,
        None,
        vec![
            PromptChoice::AllowOnce,
            PromptChoice::AllowOnce,
            PromptChoice::AllowOnce,
            PromptChoice::AllowOnce,
            PromptChoice::AllowOnce,
            PromptChoice::Deny,
            PromptChoice::Deny,
        ],
        prompts.clone(),
    );
    for (capability, target) in [("fs_read", &input), ("fs_write", &output)] {
        let result = dispatch(
            "request_permissions",
            serde_json::json!({"capability": capability, "target": target}),
            &root,
            &baseline,
            &mut gate,
        )
        .await;
        assert!(result.starts_with("granted:"), "{result}");
    }
    let pending = gate.state.pending_once_grants.clone();
    assert_eq!(pending.len(), 2);
    let unrelated = dispatch(
        "run_command",
        serde_json::json!({"command": "/bin/echo unrelated", "fs_read": [root.join("already-covered.txt")]}),
        &root,
        &baseline,
        &mut gate,
    )
    .await;
    assert!(
        unrelated.lines().any(|line| line == "unrelated"),
        "{unrelated}"
    );
    assert_eq!(gate.state.pending_once_grants, pending);
    let invalid = dispatch(
        "run_command",
        serde_json::json!({"command": "/bin/echo invalid", "fs_read": [input], "fs_write": [output, 7]}),
        &root, &baseline, &mut gate,
    ).await;
    assert!(
        invalid.starts_with("error: run_command fs_write"),
        "{invalid}"
    );
    assert_eq!(gate.state.pending_once_grants, pending);
    assert_eq!(prompts.get(), 2);
    let command = format!("/bin/cp '{}' '{}'", input.display(), output.display());
    let declared =
        serde_json::json!({"command": command, "fs_read": [input], "fs_write": [output]});
    let result = dispatch("run_command", declared.clone(), &root, &baseline, &mut gate).await;
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        "QUEUED_ONCE_CONTENT\n",
        "{result}"
    );
    assert!(gate.state.pending_once_grants.is_empty());
    assert!(gate.state.session_grants.is_empty());
    assert_eq!(
        prompts.get(),
        3,
        "only the missing executable prompts on retry"
    );
    assert_eq!(gate.state.decisions.last().unwrap().kind, "exec");
    let newt_core::PermissionDecision::Allow(refreshed) = gate.refresh_caveats(&baseline) else {
        panic!("baseline refresh refused")
    };
    assert_eq!(
        refreshed, baseline,
        "no one-shot authority may become standing authority"
    );
    std::fs::write(&output, "RESET\n").unwrap();
    // A refused later executable approval must leave the file untouched and
    // must not publish the consumed filesystem grants as standing authority.
    for (capability, target) in [("fs_read", &input), ("fs_write", &output)] {
        let result = dispatch(
            "request_permissions",
            serde_json::json!({"capability": capability, "target": target}),
            &root,
            &baseline,
            &mut gate,
        )
        .await;
        assert!(result.starts_with("granted:"), "{result}");
    }
    let denied_exec = dispatch("run_command", declared.clone(), &root, &baseline, &mut gate).await;
    assert!(
        denied_exec.starts_with("capability denied:"),
        "{denied_exec}"
    );
    assert_eq!(prompts.get(), 6);
    assert!(gate.state.pending_once_grants.is_empty());
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "RESET\n");
    let denied = dispatch("run_command", declared, &root, &baseline, &mut gate).await;
    assert!(denied.starts_with("capability denied:"), "{denied}");
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "RESET\n");
    assert_eq!(prompts.get(), 7, "consumed grants require a new decision");
}

#[test]
fn native_once_filesystem_baseline_survives_later_exec_and_net_approvals() {
    let baseline = Caveats {
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::AtMost(2),
        valid_for_generation: Scope::only([7]),
        ..base_caveats("/ws")
    };
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        Caveats::top(),
        None,
        None,
        vec![PromptChoice::AllowOnce; 4],
        prompts.clone(),
    );
    let mut current = baseline.clone();
    let mut expected = baseline.clone();
    for (kind, target) in [
        (DenialKind::FsRead, "/approved/input"),
        (DenialKind::FsWrite, "/approved/output"),
        (DenialKind::Exec, "/opt/tool/bin/copy"),
        (DenialKind::Net, "approved.test"),
    ] {
        let request = PermissionRequest {
            tool: "run_command".into(),
            kind,
            target: target.into(),
            reason: "one declared invocation".into(),
        };
        let newt_core::PermissionDecision::Allow(allowed) =
            gate.ask_with_caveats(&current, &[request])
        else {
            panic!("approved request denied")
        };
        expected = newt_core::widen_caveats(&expected, &[(kind, target.into())]);
        assert_eq!(allowed, expected, "only the requested axis may widen");
        current = allowed;
    }
    assert_eq!(prompts.get(), 4);
    assert!(gate.state.pending_once_grants.is_empty());
    assert!(gate.state.session_grants.is_empty());
    let newt_core::PermissionDecision::Allow(refreshed) = gate.refresh_caveats(&baseline) else {
        panic!("baseline refresh refused")
    };
    assert_eq!(refreshed, baseline);
}

#[test]
fn native_once_filesystem_known_refusals_precede_pending_consumption() {
    use crate::caveat_policy_tests::verified_delegation;

    let ceiling = Caveats {
        fs_read: Scope::only(["/ws".into(), "/approved/input".into()]),
        fs_write: Scope::none(),
        ..base_caveats("/ws")
    };
    let delegation = verified_delegation(ceiling.clone());
    for refusal in [
        "denial",
        "preset",
        "delegation",
        "preset-exec",
        "preset-net",
    ] {
        let mut state = PermissionPromptState::default();
        state.pending_once_grants.extend([
            (DenialKind::FsRead, "/approved/input".into()),
            (DenialKind::FsWrite, "/approved/output".into()),
        ]);
        if refusal == "denial" {
            state
                .session_denials
                .insert((DenialKind::FsWrite, "/approved/output".into()));
        }
        let pending = state.pending_once_grants.clone();
        let prompts = Rc::new(Cell::new(0));
        let baseline = base_caveats("/ws");
        let mut gate = scripted_gate(
            &mut state,
            baseline.clone(),
            None,
            None,
            vec![PromptChoice::AllowOnce],
            prompts.clone(),
        );
        if refusal == "preset" {
            gate.preset_clamp = Some(ceiling.clone());
        }
        if refusal == "delegation" {
            gate.delegation = Some(&delegation);
        }
        let mixed = match refusal {
            "preset-exec" => Some(DenialKind::Exec),
            "preset-net" => Some(DenialKind::Net),
            _ => None,
        };
        if mixed.is_some() {
            gate.preset_clamp = Some(Caveats {
                exec: Scope::none(),
                net: Scope::none(),
                ..Caveats::top()
            });
        }
        let mut requests: Vec<_> = pending
            .iter()
            .map(|(kind, target)| PermissionRequest {
                tool: "run_command".into(),
                kind: *kind,
                target: target.clone(),
                reason: "declared invocation".into(),
            })
            .collect();
        if let Some(kind) = mixed {
            requests.push(PermissionRequest {
                tool: "run_command".into(),
                kind,
                target: if kind == DenialKind::Exec {
                    "/opt/tool/bin/copy"
                } else {
                    "approved.test"
                }
                .into(),
                reason: "same batch as the pending filesystem requests".into(),
            });
        }
        assert!(
            matches!(
                gate.ask_with_caveats(&baseline, &requests),
                newt_core::PermissionDecision::Deny
            ),
            "{refusal}"
        );
        assert_eq!(gate.state.pending_once_grants, pending, "{refusal}");
        assert_eq!(prompts.get(), 0, "{refusal}");
    }
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn execute_tool_with_tui_gate_session_allow_holds_across_turns() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("outside.txt"), "gated contents").unwrap();
    let caveats = base_caveats("/elsewhere");
    let prompts = Rc::new(Cell::new(0));
    let mut state = PermissionPromptState::default();
    let args = serde_json::json!({"path": "outside.txt"});
    for _turn in 0..2 {
        let mut gate = scripted_gate(
            &mut state,
            caveats.clone(),
            None,
            None,
            vec![PromptChoice::AllowSession],
            prompts.clone(),
        );
        let out = newt_core::agentic::execute_tool(
            "read_file",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
            None,
            None,
            None,
            None, // memory_source
            Some(&mut gate),
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None,
            None, // code_search
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert_eq!(out, "gated contents");
    }
    assert_eq!(prompts.get(), 1, "one prompt for the whole session");
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].scope, "session");
}

#[test]
fn prompting_configured_from_flag_or_config_off_by_default() {
    // Neither flag nor config: OFF — zero behavior change.
    assert!(!permission_prompting_configured(false, None));
    let mut tui = newt_core::TuiConfig::default();
    assert!(!permission_prompting_configured(false, Some(&tui)));
    // CLI flag (env) alone, config alone, or both.
    assert!(permission_prompting_configured(true, None));
    tui.permissions.prompt = true;
    assert!(permission_prompting_configured(false, Some(&tui)));
    assert!(permission_prompting_configured(true, Some(&tui)));
}

#[test]
fn should_prompt_permissions_defaults_on_interactive_and_off_headless() {
    // #721: the new default — an interactive human prompts even with NOTHING
    // configured (the dead-end denial used to be the only outcome).
    assert!(should_prompt_permissions(false, false, true, false));
    // Explicitly configured ON, interactive: still ON.
    assert!(should_prompt_permissions(true, false, true, false));

    // Headless / eval / ACP NEVER prompt — the default-deny invariant —
    // even when explicitly configured on. (A prompt no one can answer hangs.)
    assert!(!should_prompt_permissions(true, false, true, true));
    // Non-TTY (piped / captured) is likewise default-deny.
    assert!(!should_prompt_permissions(true, false, false, false));
    assert!(!should_prompt_permissions(false, false, false, false));

    // Explicit OFF beats the interactive default AND an explicit ON.
    assert!(!should_prompt_permissions(false, true, true, false));
    assert!(!should_prompt_permissions(true, true, true, false));
}

/// Exhaust the boolean product: no headless/non-TTY case may open a prompt.
/// Re-execution grounds the negative counter assertion in its own process;
/// sibling permission tests may construct real windows in the parent.
#[tokio::test]
async fn headless_and_piped_sessions_never_construct_a_prompt_window() {
    const CHILD: &str = "NEWT_HEADLESS_PROMPT_COUNTER_CHILD";
    const COMPLETE: &str = "HEADLESS_PROMPT_COUNTER_UNCHANGED";
    if std::env::var_os(CHILD).is_none() {
        // Same bounded, cross-platform re-execution as the settings fixture.
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "permissions::permission_prompt_tests::headless_and_piped_sessions_never_construct_a_prompt_window",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(30), command.output())
            .await
            .expect("headless counter watchdog expired; child is killed on drop")
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "isolated headless counter failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed") && stdout.contains(COMPLETE));
        return;
    }
    let before = newt_core::tty::prompt_windows_constructed();

    for configured_on in [false, true] {
        for explicit_off in [false, true] {
            // HEADLESS: never prompts, whatever else is set.
            for interactive in [false, true] {
                assert!(
                    !should_prompt_permissions(configured_on, explicit_off, interactive, true),
                    "headless prompted (configured_on={configured_on} \
                         explicit_off={explicit_off} interactive={interactive})"
                );
            }
            // NON-INTERACTIVE (piped / captured): likewise never prompts.
            assert!(
                !should_prompt_permissions(configured_on, explicit_off, false, false),
                "a non-interactive session prompted (configured_on={configured_on} \
                     explicit_off={explicit_off})"
            );
        }
    }

    assert_eq!(
        newt_core::tty::prompt_windows_constructed(),
        before,
        "a default-denied session must reach its denial without the terminal \
             ever being suspended for a question"
    );
    println!("{COMPLETE}");
}

#[test]
fn permissions_command_lists_decisions_and_log_location() {
    let mut state = PermissionPromptState::default();
    // Disabled + empty: says how to enable, says there's nothing yet.
    // No active posture ⇒ no preset line; behavior is the pre-#307 listing.
    let lines = permissions_command_lines(&state, false, None, None);
    assert!(lines[0].contains("OFF"), "got: {lines:?}");
    assert!(lines
        .iter()
        .any(|l| l.contains("no prompted permission decisions")));
    // With decisions + a log path: one row per decision, log named,
    // and promotion remains an explicit human action.
    state.decisions.push(newt_core::PermissionRecord::new(
        "conv-1",
        "run_command",
        DenialKind::Exec,
        "npm",
        "allow",
        "session",
    ));
    let log = std::path::PathBuf::from("/home/u/.newt/permission-log.jsonl");
    let lines = permissions_command_lines(&state, true, Some(&log), None);
    assert!(lines
        .iter()
        .any(|l| l.contains("exec:npm") && l.contains("run_command")));
    assert!(lines.iter().any(|l| l.contains("permission-log.jsonl")));
    assert!(lines.iter().any(|l| l.contains("never authority")));
    assert!(!lines[0].contains("OFF"));
}

/// #307: an active posture is reflected at the top of `/permissions`, even
/// with prompting OFF — the clamp in force is always visible.
#[test]
fn permissions_command_reflects_the_active_posture() {
    let state = PermissionPromptState::default();
    let preset = newt_core::NamedPermissionPreset {
        // fs_read: None preserves pre-#755 behavior (reads unrestricted).
        fs_read: None,
        readonly: true,
        exec_allow: vec!["git".to_string()],
        deny: vec!["*".to_string()],
        max_calls: Some(40),
    };
    let posture = ActivePosture {
        name: "triage".to_string(),
        preset_name: "readonly-triage".to_string(),
        clamp: preset.clamp(),
        clamp_summary: preset.summary(),
        skill_body: None,
        framing: None,
    };
    let lines = permissions_command_lines(&state, false, None, Some(&posture));
    assert!(
        lines[0].contains("active permission posture: triage")
            && lines[0].contains("readonly-triage")
            && lines[0].contains("readonly"),
        "got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("WINS over --disable-ocap")),
        "the floor property is surfaced: {lines:?}"
    );
}

#[test]
fn help_lists_the_permissions_command() {
    assert!(help_lines().iter().any(|l| l.contains("/permissions")));
}

#[test]
fn help_lists_the_mode_and_posture_commands() {
    assert!(help_lines().iter().any(|l| l.contains("/mode")));
    assert!(help_lines().iter().any(|l| l.contains("/posture")));
}

#[test]
fn help_lists_the_start_and_rename_commands() {
    // #1030 lifecycle verbs must be discoverable in /help.
    assert!(help_lines().iter().any(|l| l.contains("/start")));
    assert!(help_lines().iter().any(|l| l.contains("/rename")));
}

#[test]
fn close_out_message_reflects_the_rotation_kind() {
    // Persisted outgoing: /new is bare; /start says stays-open; the finalizers
    // point at /resume (no more "won't resume next launch").
    assert_eq!(close_out_message("new", "NEW", true), "NEW");
    assert!(close_out_message("start", "NEW", true).contains("stays open"));
    assert!(close_out_message("start", "NEW", true).contains("/resume"));
    // #1165: /end LEADS with the ending, never "Started a new conversation".
    let end = close_out_message("end", "NEW", true);
    assert!(end.starts_with("Conversation ended"), "{end}");
    assert!(end.contains("/resume to reopen"), "{end}");
    assert!(
        !end.starts_with("NEW"),
        "end must not headline the new conversation: {end}"
    );
    assert!(close_out_message("restart", "NEW", true).contains("/resume to reopen"));
    // Nothing persisted (empty conversation or ephemeral session): no
    // resume promise — the plain new-conversation line for start/new/
    // restart, but /end STILL leads with the ending (#1170 UAT gap).
    assert_eq!(close_out_message("start", "NEW", false), "NEW");
    let end_empty = close_out_message("end", "NEW", false);
    assert!(end_empty.starts_with("Conversation ended"), "{end_empty}");
    assert!(
        !end_empty.contains("/resume"),
        "nothing to reopen: {end_empty}"
    );
}

/// **C4b (#1944): the terminal is a responder, not just an abort key.**
///
/// With the web attached, `run_web_wait` already waits on BOTH sources — it
/// polls the store every iteration and the control reader alongside it. The
/// reader already yields `PromptLine::Line`, and `resolve_answer` already
/// decodes it. The one thing standing between the operator and an answer was
/// the arm that threw the line away:
///
/// ```text
/// // A typed line or EOF at a web prompt is ignored; we polled.
/// Some(Ok(_)) => blocked = true,
/// ```
///
/// So the terminal operator could abandon the turn but not decide it, while a
/// browser they might not have open could.
// #1959: see the comment on `transient_reader_error_recovers_and_esc_resolves_through_the_tty_path`.
#[serial_test::serial(prompt_stdin)]
#[test]
fn a_typed_terminal_answer_decides_the_offer_instead_of_being_dropped() {
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_open_to_both(&store, &conv);
    let mut readers: VecDeque<ScriptedReader> =
        VecDeque::from([ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Line(
            "a".to_string(),
        )))]))]);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let (choice, scope) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |_m| {},
        },
    );
    assert_eq!(
        choice,
        PromptChoice::AllowOnce,
        "the operator's typed answer did not decide the offer"
    );
    assert_eq!(scope, "once");
    assert!(
        store.pending_interaction_offer(&conv).unwrap().is_none(),
        "the terminal answer did not resolve the published offer"
    );
    assert_eq!(
        store.interaction_answered_by(&conv, &request_id).unwrap(),
        Some(Audience::Terminal),
        "the audit fact must name the terminal as the responder"
    );
}

/// **C4b: a loser must lose visibly, on the terminal too.**
///
/// C3b (#1536) found that blanket-redirecting a losing no-JS POST told a web
/// operator they had won. The broker makes the terminal the other side of
/// that coin: if the web answers first, the operator who typed an answer must
/// not be left believing theirs decided it.
///
/// A losing ABORT may silently hand back the winner's action — the operator
/// asked to leave, not to decide. A losing ANSWER may not.
// #1959: see the comment on `transient_reader_error_recovers_and_esc_resolves_through_the_tty_path`.
#[serial_test::serial(prompt_stdin)]
#[test]
fn a_terminal_answer_that_loses_to_the_web_is_told_it_lost() {
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_open_to_both(&store, &conv);
    store
        .answer_interaction_offer(&conv, &request_id, PromptChoice::Deny, Audience::Web)
        .unwrap();
    let mut readers: VecDeque<ScriptedReader> =
        VecDeque::from([ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Line(
            "a".to_string(),
        )))]))]);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let mut told: Vec<String> = Vec::new();
    let (choice, _scope) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |m| told.push(m.to_string()),
        },
    );
    assert_eq!(
        choice,
        PromptChoice::Deny,
        "the losing terminal answer was applied instead of the winner's"
    );
    assert_eq!(
        told.len(),
        1,
        "the operator was not told they lost: {told:?}"
    );
    assert!(
        told[0].contains("deny") && told[0].contains("allow_once"),
        "the message must name the winner AND the operator's own answer: {}",
        told[0]
    );
}

/// The twin. Without it, a notice emitted unconditionally — on every answer,
/// winning or losing — would satisfy the test above.
// #1959: see the comment on `transient_reader_error_recovers_and_esc_resolves_through_the_tty_path`.
#[serial_test::serial(prompt_stdin)]
#[test]
fn a_terminal_answer_that_wins_is_told_nothing() {
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_open_to_both(&store, &conv);
    let mut readers: VecDeque<ScriptedReader> =
        VecDeque::from([ScriptedReader(VecDeque::from([Ok(Some(ModalLine::Line(
            "a".to_string(),
        )))]))]);
    let mut state = PermissionPromptState {
        web_store: Some(store.clone()),
        ..Default::default()
    };
    let gate = web_gate!(
        &mut state,
        conv.clone(),
        Duration::from_secs(3600),
        None,
        None
    );
    let win = Terminal::suspend_for_prompt(newt_core::tty::TerminalTaker::PermissionAuthorization);
    let mut told: Vec<String> = Vec::new();
    let (choice, _scope) = gate.run_web_wait(
        &store,
        &request_id,
        &low_danger_definition(),
        &win,
        &mut WebWaitIo {
            reacquire: &mut || {
                readers
                    .pop_front()
                    .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                    .ok_or_else(broken)
            },
            now: &stepping_clock(Duration::from_millis(50)),
            sleep: &mut |_d| {},
            notify: &mut |m| told.push(m.to_string()),
        },
    );
    assert_eq!(choice, PromptChoice::AllowOnce);
    assert!(
        told.is_empty(),
        "an operator who WON was told they lost: {told:?}"
    );
}

// Model: GPT-6 | Harness: Codex | Operator: S Hartsock | Time: 17:30 EDT | Date: 2026-09-15

// Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 22:06 EDT | Date: 2026-09-17

// Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 05:48 EDT | Date: 2026-09-18

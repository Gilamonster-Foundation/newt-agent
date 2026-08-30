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
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: Duration::from_secs(2),
        cancel: None,
        exit: None,
        // Proof the TTY is bypassed when web decisions are on.
        ask_surface: None,
        ask_human: |_w: &PromptWindow, _d: &InteractionDefinition| {
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
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: Duration::from_millis(50),
        cancel: None,
        exit: None,
        ask_surface: None,
        ask_human: |_w: &PromptWindow, _d: &InteractionDefinition| {
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
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: Duration::from_millis(50),
        cancel: None,
        exit: None,
        ask_surface: None,
        ask_human: |_w: &PromptWindow, _d: &InteractionDefinition| {
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
    let req = exec_request("bash");
    let definition = permission_definition(&req, &danger::DangerTable::builtin(), Audience::Web);
    store
        .publish_interaction_offer(
            conv,
            &definition,
            newt_core::interaction_offer::OfferDanger::Low,
            &[Audience::Web],
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
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: $timeout,
            cancel: $cancel,
            exit: $exit,
            ask_surface: None,
            ask_human: |_w: &PromptWindow, _d: &InteractionDefinition| {
                panic!("run_web_wait must not read the TTY answer path")
            },
        }
    };
}

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
    let win = Terminal::suspend_for_prompt();
    let (choice, scope) = gate.run_web_wait(
        &store,
        &request_id,
        &win,
        || {
            readers
                .pop_front()
                .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                .ok_or_else(broken)
        },
        stepping_clock(Duration::from_millis(50)),
        |_d| {},
    );
    assert_eq!(choice, PromptChoice::Back);
    assert_eq!(scope, "control");
    // The local abort resolved the request (nothing left pending).
    assert!(store.pending_interaction_offer(&conv).unwrap().is_none());
}

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
    let win = Terminal::suspend_for_prompt();
    let (choice, _scope) = gate.run_web_wait(
        &store,
        &request_id,
        &win,
        || {
            readers
                .pop_front()
                .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                .ok_or_else(broken)
        },
        stepping_clock(Duration::from_millis(50)),
        |_d| {},
    );
    assert_eq!(
        choice,
        PromptChoice::AllowOnce,
        "web allow must survive a reader error"
    );
}

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
    let win = Terminal::suspend_for_prompt();
    let sleeps = std::cell::Cell::new(0u32);
    let (choice, scope) = gate.run_web_wait(
        &store,
        &request_id,
        &win,
        || Err::<Box<dyn newt_core::tty::ControlReader>, _>(broken()),
        stepping_clock(Duration::from_millis(20)),
        |_d| sleeps.set(sleeps.get() + 1),
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
    let win = Terminal::suspend_for_prompt();
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &win,
        || {
            readers
                .pop_front()
                .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                .ok_or_else(broken)
        },
        stepping_clock(Duration::from_millis(50)),
        |_d| {},
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
    let win2 = Terminal::suspend_for_prompt();
    let (choice2, _s2) = gate2.run_web_wait(
        &store2,
        &request_id2,
        &win2,
        || {
            readers2
                .pop_front()
                .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                .ok_or_else(broken)
        },
        stepping_clock(Duration::from_millis(50)),
        |_d| {},
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
    let win = Terminal::suspend_for_prompt();
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &win,
        || {
            readers
                .pop_front()
                .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                .ok_or_else(broken)
        },
        stepping_clock(Duration::from_millis(50)),
        |_d| {},
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
    let win = Terminal::suspend_for_prompt();
    let reacquired = std::cell::Cell::new(0u32);
    let sleeps = std::cell::Cell::new(0u32);
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &win,
        || {
            reacquired.set(reacquired.get() + 1);
            readers
                .pop_front()
                .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
                .ok_or_else(broken)
        },
        stepping_clock(Duration::from_millis(50)),
        |_d| sleeps.set(sleeps.get() + 1),
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
    let win = Terminal::suspend_for_prompt();
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &win,
        || {
            outcomes
                .pop_front()
                .unwrap_or_else(|| Err(broken()))
                .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
        },
        stepping_clock(Duration::from_millis(50)),
        |_d| {},
    );
    assert_eq!(
        choice,
        PromptChoice::Back,
        "an initial Unsupported must not permanently disable controls"
    );
}

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
    let win = Terminal::suspend_for_prompt();
    let (choice, _s) = gate.run_web_wait(
        &store,
        &request_id,
        &win,
        || {
            outcomes
                .pop_front()
                .unwrap_or_else(|| Err(broken()))
                .map(|r| Box::new(r) as Box<dyn newt_core::tty::ControlReader + '_>)
        },
        stepping_clock(Duration::from_millis(50)),
        |_d| {},
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
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_surface: None,
            ask_human: move |_w: &PromptWindow, _d: &InteractionDefinition| {
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
) -> PromptPermissionGate<'a, impl FnMut(&PromptWindow, &InteractionDefinition) -> PromptChoice> {
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
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: Duration::from_secs(2),
        cancel: None,
        exit: None,
        ask_surface: None,
        ask_human: move |_w: &PromptWindow, _definition: &InteractionDefinition| {
            prompts.set(prompts.get() + 1);
            script.next().expect("script exhausted — unexpected prompt")
        },
    }
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
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_surface: None,
            ask_human: move |_w: &PromptWindow, _d: &InteractionDefinition| {
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
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_surface: None,
            ask_human: |_w: &PromptWindow, _d: &InteractionDefinition| {
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
    let dir = tempfile::TempDir::new().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "# my config\n[tui.permissions]\nnet = []\n").unwrap();
    let base = base_caveats("/ws");
    let net_req = newt_core::PermissionRequest {
        tool: "web_fetch".to_string(),
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
            danger: danger::DangerTable::builtin(),
            color: false,
            verbose: false,
            authorization_prompts_enabled: true,
            web_decision_timeout: Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_surface: None,
            ask_human: move |_w: &PromptWindow, _d: &InteractionDefinition| {
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
    let reloaded = newt_core::Config::load(&config).unwrap();
    assert!(
        reloaded
            .tui
            .unwrap()
            .permissions
            .net
            .contains(&"github.com".to_string()),
        "a fresh session reads the durable net grant"
    );
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
        target: "python3".to_string(),
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
                c.permits_exec("python3"),
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
fn session_grant_exec_matches_by_basename() {
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
        gate.ask(&[exec_request("mytool")]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(prompts.get(), 1);
    assert!(matches!(
        gate.ask(&[exec_request("/opt/bin/mytool")]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(prompts.get(), 1, "basename covers the resolved path");
    assert!(matches!(
        gate.ask(&[exec_request("othertool")]),
        newt_core::PermissionDecision::Allow(_)
    ));
    assert_eq!(prompts.get(), 2, "a different program is not covered");
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
#[serial_test::serial(prompt_stdin)]
#[test]
fn headless_and_piped_sessions_never_construct_a_prompt_window() {
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
    // promotion stays a human config edit.
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

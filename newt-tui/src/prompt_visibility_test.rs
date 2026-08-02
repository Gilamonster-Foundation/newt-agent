//! Real-PTY ground truth for the mocked arbiter and form-parser tiers.
//!
//! A production spinner must stop before the typed permission form appears;
//! while that form owns the terminal, Esc returns immediately to chat and
//! Ctrl-C/Ctrl-D immediately exit. A child process bypasses Rust test capture
//! so these assertions inspect the bytes an operator actually sees.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tests_pty::Pty;

use crate::danger;
use crate::permissions::{
    permission_question, prompt_permission_choice, PermissionPromptState, PromptPermissionGate,
};
use newt_core::caveats::{Caveats, CountBound, Scope};
use newt_core::tty::{LineCaps, Sink, Spinner};
use newt_core::{DenialKind, PermissionGate as _, PermissionRequest};

/// The child test's fully-qualified name, used to re-invoke this binary.
const CHILD_TEST: &str = "prompt_visibility_test::prompt_scenario_child";
const CHILD_TEST_WEB_CONTROLS: &str = "prompt_visibility_test::prompt_scenario_child";

/// How long the "operator" takes to answer. Long enough that a 100 ms ticker
/// gets several chances to redraw over the question — which is what it did.
const HUMAN_THINKING_TIME: Duration = Duration::from_millis(600);

fn wait_for_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll prompt child") {
            return Some(status);
        }
        if std::time::Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The session's enforced authority: no net at all, so `example.com` denies and
/// the gate prompts — the `tools.rs` `web_fetch` arm's precondition.
fn no_net_caveats() -> Caveats {
    Caveats {
        fs_read: Scope::only(["/ws".to_string()]),
        fs_write: Scope::only(["/ws".to_string()]),
        exec: Scope::only(["cargo".to_string()]),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// The request `web_fetch` builds when `!caveats.permits_net(&host)` — copied
/// from the tool arm so this test tracks the real shape.
fn web_fetch_request(host: &str) -> PermissionRequest {
    PermissionRequest {
        tool: "web_fetch".to_string(),
        kind: DenialKind::Net,
        target: host.to_string(),
        reason: format!("net does not permit '{host}'"),
    }
}

// ---------------------------------------------------------------------------
// The child: the scenario itself, run with fd 0/1 on a pty.
// ---------------------------------------------------------------------------

/// Not a test in its own right — the body of the scenario, invoked as a child
/// process by [`a_permission_prompt_is_visible_and_survives_a_live_spinner`].
/// `#[ignore]` keeps it out of a normal run; it does nothing unless the parent's
/// env marker is set.
#[test]
#[ignore = "child process of the prompt-visibility regression test"]
fn prompt_scenario_child() {
    let visibility = std::env::var_os("NEWT_PROMPT_VISIBILITY_CHILD").is_some();
    let controls = std::env::var_os("NEWT_PROMPT_CONTROLS_CHILD").is_some();
    let web_controls = std::env::var_os("NEWT_PROMPT_WEB_CONTROLS_CHILD").is_some();
    if !visibility && !controls && !web_controls {
        return;
    }

    if controls {
        let window = newt_core::tty::Terminal::suspend_for_prompt();
        let question = permission_question(
            &web_fetch_request("example.com"),
            &danger::DangerTable::builtin(),
        );
        let choice = prompt_permission_choice(&window, &question);
        drop(window);
        println!("PROMPT-CONTROL:{choice:?}");
        return;
    }

    if web_controls {
        let root = tempfile::tempdir().expect("temp conversation root");
        let workspace = tempfile::tempdir().expect("temp permission workspace");
        let store = newt_core::ConversationStore::new(root.path(), workspace.path(), 100)
            .expect("web store");
        let conv = store
            .create("conv-prompt-visibility-web", None)
            .expect("conversation");
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let request = web_fetch_request("example.com");
        let mut state = PermissionPromptState::default();
        state.web_store = Some(store);
        {
            let mut gate = PromptPermissionGate {
                state: &mut state,
                base: no_net_caveats(),
                key_path: None,
                conversation_id: conv,
                log_path: None,
                denials_path: None,
                config_path: None,
                preset_clamp: None,
                danger: danger::DangerTable::builtin(),
                color: true,
                verbose: false,
                web_decision_timeout: std::time::Duration::from_secs(2),
                cancel: Some(cancel.as_ref()),
                exit: Some(exit.as_ref()),
                ask_human: prompt_permission_choice,
            };
            let _decision = gate.ask(std::slice::from_ref(&request));
        }
        println!(
            "PROMPT-WEB-CONTROL:{:?}:{:?}:{:?}",
            cancel.load(Ordering::Acquire),
            exit.load(Ordering::Acquire),
            state.decisions.len()
        );
        return;
    }

    // A live production spinner — the one `stream_response` and the probe /
    // compression waits construct during a turn. It owns the bottom line and
    // its shared ticker redraws it every 100 ms.
    let spinner = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, true)
        .expect("the pty is a real terminal, so the spinner takes the line");
    // Let it paint a few frames, exactly as it would mid-turn.
    std::thread::sleep(Duration::from_millis(250));

    let request = web_fetch_request("example.com");
    let mut state = PermissionPromptState::default();
    {
        // Wired exactly as `chat.rs` wires the production gate.
        let mut gate = PromptPermissionGate {
            state: &mut state,
            base: no_net_caveats(),
            key_path: None,
            conversation_id: "conv-prompt-visibility".to_string(),
            log_path: None,
            denials_path: None,
            config_path: None,
            preset_clamp: None,
            danger: danger::DangerTable::builtin(),
            color: true,
            verbose: false,
            web_decision_timeout: std::time::Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_human: prompt_permission_choice,
        };
        let _decision = gate.ask(std::slice::from_ref(&request));
    }
    // Stop the animation before exiting, so the capture is not extended by
    // frames drawn after the operator already answered.
    drop(spinner);
}

/// Grounds the prompt parser's mocked control outcomes against a real PTY:
/// each key must resolve immediately, without the Enter a canonical read needs.
#[serial_test::serial(prompt_stdin)]
#[test]
fn permission_prompt_controls_are_immediate_and_distinct() {
    for (key, expected) in [("\u{1b}", "Back"), ("\u{3}", "Exit"), ("\u{4}", "Exit")] {
        let pty = Pty::open();
        let mut child = std::process::Command::new(
            std::env::current_exe().expect("the test binary re-invokes itself"),
        )
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env("NEWT_PROMPT_CONTROLS_CHILD", "1")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the pty child");

        std::thread::sleep(Duration::from_millis(200));
        pty.type_in(key);
        let status = wait_for_child(&mut child, Duration::from_secs(1));
        let screen = pty.screen();
        assert!(
            status.is_some_and(|s| s.success())
                && screen.contains(&format!("PROMPT-CONTROL:{expected}")),
            "key {key:?} did not resolve immediately as {expected}; screen={screen:?}"
        );
    }
}

#[serial_test::serial(prompt_stdin)]
#[test]
fn web_permission_prompt_controls_are_immediate_and_distinct() {
    for (key, expected_cancel, expected_exit) in [
        ("\u{1b}", true, false),
        ("\u{3}", true, true),
        ("\u{4}", true, true),
    ] {
        let pty = Pty::open();
        let mut child = std::process::Command::new(
            std::env::current_exe().expect("the test binary re-invokes itself"),
        )
        .args([
            "--exact",
            CHILD_TEST_WEB_CONTROLS,
            "--ignored",
            "--nocapture",
        ])
        .env("NEWT_PROMPT_WEB_CONTROLS_CHILD", "1")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the web-control child");

        std::thread::sleep(Duration::from_millis(200));
        pty.type_in(key);
        let status = wait_for_child(&mut child, Duration::from_secs(1));
        let screen = pty.screen();
        let expected = format!("PROMPT-WEB-CONTROL:{expected_cancel:?}:{expected_exit:?}:");
        assert!(
            status.is_some_and(|s| s.success()) && screen.contains(&expected),
            "web controls did not cancel as expected; key={key:?}; screen={screen:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The parent: allocate the pty, drive the child, judge the capture.
// ---------------------------------------------------------------------------

/// Drive the scenario child on a real pty and judge what the terminal saw.
///
/// The pty plumbing lives in `tests-pty` (#1410) — the slave becomes the
/// child's stdin+stdout, the master is what the "operator" types on and what
/// we read the screen from.
#[serial_test::serial(prompt_stdin)]
#[test]
fn a_permission_prompt_is_visible_and_survives_a_live_spinner() {
    let pty = Pty::open();

    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
    .env("NEWT_PROMPT_VISIBILITY_CHILD", "1")
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn the pty child");

    // The operator reads the question, thinks, then denies.
    std::thread::sleep(HUMAN_THINKING_TIME);
    pty.type_in("d\r");

    let status = wait_for_child(&mut child, Duration::from_secs(2));

    let screen = pty.screen();
    assert!(
        status.is_some_and(|status| status.success()),
        "the scenario child failed.\n\nscreen:\n{screen:?}"
    );

    // --- the window under test: question's first byte .. operator's keystroke
    let prompt_start = screen.find('⊘').unwrap_or_else(|| {
        panic!("the question was never written to the terminal at all.\n\nscreen:\n{screen:?}")
    });
    let echo_rel = screen[prompt_start..]
        .find("d\r\n")
        .or_else(|| screen[prompt_start..].find("d\n"))
        .unwrap_or_else(|| {
            panic!(
                "never saw the operator's keystroke echoed — the prompt did not \
                 reach a blocking read.\n\nscreen:\n{screen:?}"
            )
        });
    let window = &screen[prompt_start..prompt_start + echo_rel];

    let expected_prompt = permission_question(
        &web_fetch_request("example.com"),
        &danger::DangerTable::builtin(),
    )
    .terminal_text();

    // (1) The FULL multi-line question survives, contiguously. A single-line
    // `ESC[K` can only preserve the final menu row; the header rows would be
    // stranded in scrollback, leaving a truncated question with no menu.
    // The pty's ONLCR translates every `\n` the process wrote into `\r\n` on
    // the wire, so compare against the same normalization.
    let window_lf = window.replace("\r\n", "\n");
    assert!(
        window_lf.contains(expected_prompt.trim_end()),
        "the question did not survive intact.\n\n  expected:\n{expected_prompt:?}\n\n  \
         on screen between the question and the keystroke:\n{window:?}"
    );

    // (2) NOTHING redrew after the question started. This is the assertion that
    // fails before the fix: the spinner's ticker paints a braille frame over
    // the menu roughly ten times a second for as long as the operator reads.
    let intruders: Vec<char> = window
        .chars()
        .filter(|c| ('\u{2800}'..='\u{28FF}').contains(c))
        .collect();
    assert!(
        intruders.is_empty(),
        "an ephemeral writer painted {} spinner frame(s) {:?} OVER the question \
         while the operator was reading it — this is the reported hang: newt is \
         correctly blocked on a question that has been scribbled out.\n\n  \
         on screen:\n{window:?}",
        intruders.len(),
        intruders,
    );

    // (3) The menu is the last thing written before the human typed.
    let visible_tail = window.trim_end_matches(|c: char| c.is_whitespace());
    assert!(
        visible_tail.ends_with('>'),
        "the choice menu was not the last thing on screen when the read \
         blocked.\n\n  on screen:\n{window:?}"
    );
}

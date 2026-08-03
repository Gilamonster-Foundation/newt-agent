//! Real-PTY regression tests for permission prompt behavior.

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

const CHILD_TEST: &str = "prompt_visibility_test::prompt_scenario_child";

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

fn web_fetch_request(host: &str) -> PermissionRequest {
    PermissionRequest {
        tool: "web_fetch".to_string(),
        kind: DenialKind::Net,
        target: host.to_string(),
        reason: format!("net does not permit '{host}'"),
    }
}

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
                // Far longer than the parent's 10s wait: this test asserts a
                // control key ABORTS the web wait, so the fallback timeout must
                // never win the race. On a starved CI runner a 2s deadline
                // (wall-clock) could elapse before the child was scheduled to
                // read the typed key, resolving as a spurious web-timeout Deny
                // (`false:false`) instead of the control's cancel/exit. With 30s
                // the control always wins if read within the parent's budget; a
                // genuine "control never read" bug still fails (parent times out).
                authorization_prompts_enabled: true,
                web_decision_timeout: std::time::Duration::from_secs(30),
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

    let spinner = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, true)
        .expect("the pty is a real terminal, so the spinner takes the line");
    std::thread::sleep(Duration::from_millis(250));

    let request = web_fetch_request("example.com");
    let mut state = PermissionPromptState::default();
    {
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
            authorization_prompts_enabled: true,
            web_decision_timeout: std::time::Duration::from_secs(2),
            cancel: None,
            exit: None,
            ask_human: prompt_permission_choice,
        };
        let _decision = gate.ask(std::slice::from_ref(&request));
    }
    drop(spinner);
}

fn prompt_control_child(env: &str, expected: &str, key: &str, child: &str) {
    let pty = Pty::open();
    let mut child = std::process::Command::new(std::env::current_exe().expect("self test"))
        .args(["--exact", child, "--ignored", "--nocapture"])
        .env(env, "1")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn control child");
    std::thread::sleep(Duration::from_millis(200));
    pty.type_in(key);
    // Wait comfortably longer than the gate's `web_decision_timeout` (2s). The
    // invariant under test is that a control key resolves BEFORE that timeout —
    // an immediate control exits with "Back"/"Exit" almost instantly (this poll
    // returns as soon as the child exits), whereas a non-immediate control would
    // be overtaken by the 2s web timeout and render "Deny", failing the screen
    // assertion below. A tight 1s wait instead flaked on loaded CI runners,
    // cutting off a genuinely-immediate child before it could exit.
    let status = wait_for_child(&mut child, Duration::from_secs(10));
    let screen = pty.screen();
    assert!(
        status.is_some_and(|s| s.success()) && screen.contains(expected),
        "{key:?} {screen:?}"
    );
}

#[serial_test::serial(prompt_stdin)]
#[test]
fn permission_prompt_controls_are_immediate_and_distinct() {
    for (key, tty, web) in [
        ("\u{1b}", "Back", "true:false"),
        ("\u{3}", "Exit", "true:true"),
        ("\u{4}", "Exit", "true:true"),
    ] {
        prompt_control_child(
            "NEWT_PROMPT_CONTROLS_CHILD",
            &format!("PROMPT-CONTROL:{tty}"),
            key,
            CHILD_TEST,
        );
        prompt_control_child(
            "NEWT_PROMPT_WEB_CONTROLS_CHILD",
            &format!("PROMPT-WEB-CONTROL:{web}:"),
            key,
            CHILD_TEST,
        );
    }
}

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

    std::thread::sleep(HUMAN_THINKING_TIME);
    pty.type_in("d\r");

    let status = wait_for_child(&mut child, Duration::from_secs(2));

    let screen = pty.screen();
    assert!(
        status.is_some_and(|status| status.success()),
        "child failed; screen={screen:?}"
    );

    let prompt_start = screen
        .find('⊘')
        .unwrap_or_else(|| panic!("question missing; screen={screen:?}"));
    let echo_rel = screen[prompt_start..]
        .find("d\r\n")
        .or_else(|| screen[prompt_start..].find("d\n"))
        .unwrap_or_else(|| panic!("keystroke not echoed; screen={screen:?}"));
    let window = &screen[prompt_start..prompt_start + echo_rel];

    let expected_prompt = permission_question(
        &web_fetch_request("example.com"),
        &danger::DangerTable::builtin(),
    )
    .terminal_text();

    let window_lf = window.replace("\r\n", "\n");
    assert!(
        window_lf.contains(expected_prompt.trim_end()),
        "question not fully visible\nscreen={screen:?}"
    );

    let intruders: Vec<char> = window
        .chars()
        .filter(|c| ('\u{2800}'..='\u{28FF}').contains(c))
        .collect();
    assert!(
        intruders.is_empty(),
        "expected no redraw after prompt appears\nscreen={screen:?}\nintruders={intruders:?}"
    );

    let visible_tail = window.trim_end_matches(|c: char| c.is_whitespace());
    assert!(
        visible_tail.ends_with('>'),
        "menu missing before input\nscreen={screen:?}"
    );
}

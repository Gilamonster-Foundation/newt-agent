//! Real-PTY regression tests for permission prompt behavior.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tests_pty::Pty;

use crate::danger;
use crate::permissions::{
    permission_definition, prompt_permission_choice, PermissionPromptState, PromptPermissionGate,
};
use newt_core::caveats::{Caveats, CountBound, Scope};
use newt_core::tty::{LineCaps, Sink, Spinner, MODAL_INPUT_GLYPH};
use newt_core::{DenialKind, PermissionGate as _, PermissionRequest};

const CHILD_TEST: &str = "prompt_visibility_test::prompt_scenario_child";

const HUMAN_THINKING_TIME: Duration = Duration::from_millis(600);

// `pub(crate)`: shared with `transcript_pager_pty_test` (#1677) — the reuse
// discipline says one child-reaper for the real-PTY tier, not a copy per test.
pub(crate) fn wait_for_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    wait_for_child_nudging(child, timeout, || {})
}

/// Poll the child to exit, invoking `nudge` once per second while it hasn't.
/// Kills (and reaps) the child at `timeout`.
fn wait_for_child_nudging(
    child: &mut std::process::Child,
    timeout: Duration,
    mut nudge: impl FnMut(),
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        nudge();
        let slice_end = std::cmp::min(std::time::Instant::now() + Duration::from_secs(1), deadline);
        loop {
            if let Some(status) = child.try_wait().expect("poll prompt child") {
                return Some(status);
            }
            if std::time::Instant::now() >= slice_end {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if std::time::Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            return None;
        }
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
        let window = newt_core::tty::Terminal::suspend_for_prompt(
            newt_core::tty::TerminalTaker::RichSurfaceModal,
        );
        let definition = permission_definition(
            &web_fetch_request("example.com"),
            &danger::DangerTable::builtin(),
            newt_interaction::Audience::Terminal,
        );
        let choice = prompt_permission_choice(&window, &definition);
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
                ask_surface: None,
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
            ask_surface: None,
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
    // Type only once the child's prompt is actually on screen. A fixed pre-type
    // sleep raced the child's terminal setup on loaded CI runners: the key
    // landed in cooked mode, was echoed (`^C`) and swallowed by the line
    // discipline, and the web-decision timeout then resolved "Deny" instead of
    // the control under test.
    let armed_by = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let screen = pty.screen();
        if screen.contains("example.com") {
            break;
        }
        assert!(
            std::time::Instant::now() < armed_by,
            "child never presented its prompt; screen={screen:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The marker narrows the race but cannot close it alone: the TTY child's
    // modal reader enters raw mode BEFORE rendering the question (its window
    // only sets canonical mode), but the web child prints its banner just
    // BEFORE its first reader arm — a sliver of cooked mode where Ctrl-C/
    // Ctrl-D can still be eaten. So re-type the key each second until the
    // child exits: a swallowed key is re-sent once the reader is armed (typed
    // bytes are kernel-buffered across the raw-mode switch, never flushed),
    // and an extra byte after resolution is simply never read. The 10s cap
    // stays comfortably above the web-decision timeout so a genuinely
    // non-immediate control still loses to it and fails the screen assertion.
    let status = wait_for_child_nudging(&mut child, Duration::from_secs(10), || pty.type_in(key));
    let screen = pty.screen();
    assert!(
        status.is_some_and(|s| s.success()) && screen.contains(expected),
        "{key:?} {screen:?}"
    );
}

/// Real-PTY grounding test for the prompt CONTROL keys — Esc→Back,
/// Ctrl-C/Ctrl-D→Exit — on both the local-tty prompt and the web-wait prompt.
/// Unlike the newline-terminated `"d\r"` of the visibility test (which the
/// cooked-mode line discipline delivers deterministically), this types raw
/// control BYTES that only take effect once the child's modal reader is in raw
/// mode. Under the parallel per-PR load the child can be CPU-starved past that
/// window, so a nudged byte is echoed (`^C`) and swallowed in cooked mode —
/// making the test inherently non-deterministic in the multi-threaded unit run.
/// It therefore lives in the single-threaded real-PTY tier
/// (`.github/workflows/newt-tui-pty.yml`, weekly + release), NOT the per-PR
/// `cargo test --workspace`, exactly as the output-oracle real tier is gated.
///
/// It GROUNDS the mocked control-key unit tests (`permissions.rs` /
/// `question.rs`: `Action` parse + `PermissionAction`/`HumanQuestionOutcome`
/// mapping) — those assert the key→outcome mapping in memory; this proves a
/// real terminal actually delivers the bytes that trigger it. See the CLAUDE.md
/// testing tiers and the real-resource-migration issue #514.
#[serial_test::serial(prompt_stdin)]
#[test]
#[ignore = "real-PTY control-byte tier; runs single-threaded in newt-tui-pty.yml (weekly/release), not the parallel per-PR gate — see doc comment"]
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

    // C0a (#1856): the expectation is re-derived from the production
    // renderer, so this real-PTY grounding tracks `plain::render` rather
    // than a copy of its output.
    let expected_prompt = newt_core::markup::plain::render(&permission_definition(
        &web_fetch_request("example.com"),
        &danger::DangerTable::builtin(),
        newt_interaction::Audience::Terminal,
    ));

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

    // VISIBLE means visible (#1959). SGR sequences are zero-width, so a row
    // written as `❯ \x1b[0m` ends, to the operator, at the chevron. #1959 gave
    // the modal input row an accent, and an accent must reset after itself or
    // the text the user types inherits it — which appends exactly such a
    // sequence. Measuring the raw byte tail would make this an assertion about
    // which INVISIBLE bytes come last, which is not what it is for.
    //
    // The guard is unchanged in strength: anything the operator can actually
    // SEE after the chevron — a spinner frame, a stray glyph, a redraw — still
    // lands at the end of the visible tail and still fails.
    let visible = strip_ansi(window);
    let visible_tail = visible.trim_end_matches(|c: char| c.is_whitespace());
    // The input line ends with the modal chevron the user types behind. Assert
    // against the production glyph itself (not a hardcoded `>`) so this
    // visibility check tracks `MODAL_INPUT_GLYPH` and can't drift from it.
    assert!(
        visible_tail.ends_with(MODAL_INPUT_GLYPH.trim_end()),
        "menu missing before input\nvisible_tail={visible_tail:?}\nscreen={screen:?}"
    );
}

/// Everything in `text` an operator can see: CSI/OSC escape sequences removed,
/// printable characters kept in order.
///
/// Local to this file on purpose. `cockpit::ansi::visible_width` is the
/// nearest existing thing and would be the right one to widen, but it lives
/// under `mod cockpit`, which is `rich-tui`-gated, and this test is not —
/// reaching for it would gate a unix regression test on a feature it does not
/// otherwise need. Kept deliberately small: it recognises the two escape
/// shapes a terminal writer actually emits here.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-byte escape; its second byte is already consumed.
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod strip_ansi_tests {
    use super::strip_ansi;

    /// The exact shape #1959's accent emits, plus the redraw around it.
    #[test]
    fn an_sgr_wrapped_row_reduces_to_its_visible_characters() {
        assert_eq!(strip_ansi("\u{1b}[38;2;255;165;90m❯ \u{1b}[0m"), "❯ ");
        assert_eq!(
            strip_ansi("a\u{1b}[0m\u{1b}[1G\u{1b}[2K\u{1b}[38;2;1;2;3m❯ \u{1b}[0m"),
            "a❯ "
        );
    }

    /// **Anti-vacuous twin.** A stripper that returned the empty string, or
    /// that ate ordinary text, would satisfy the assertion it feeds. It keeps
    /// every visible character — including one painted AFTER the chevron,
    /// which is the case the visibility guard exists to catch.
    #[test]
    fn a_visible_character_after_the_chevron_survives_stripping() {
        let painted_over = "\u{1b}[38;2;1;2;3m❯ \u{1b}[0m⠋";
        let visible = strip_ansi(painted_over);
        assert_eq!(visible, "❯ ⠋");
        assert!(
            !visible.trim_end().ends_with('❯'),
            "a spinner frame after the chevron must still break the tail check"
        );
    }

    /// Plain text is untouched, and an OSC hyperlink is removed whole.
    #[test]
    fn plain_text_is_untouched_and_osc_is_removed() {
        assert_eq!(strip_ansi("[a]llow once"), "[a]llow once");
        assert_eq!(
            strip_ansi("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}"),
            "link"
        );
    }
}

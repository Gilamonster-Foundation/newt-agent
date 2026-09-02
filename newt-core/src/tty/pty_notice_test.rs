//! **The regression proof for the summarizer race** — a notice emitted from
//! outside the lease must not damage what is already on the terminal.
//!
//! # What this grounds
//!
//! Real-resource tier, per CLAUDE.md: an add-on to the mocked tier, not a
//! deviation from it. It grounds two mocked unit tests, and it exists because
//! neither of them can observe the property that actually matters.
//!
//! 1. **`arbiter::tests::erase_is_idempotent`** proves `LineLease::erase` is
//!    flag-guarded — that a second erase sets no bytes in motion *according to
//!    the flag*. `Terminal::emit_line` stakes the entire suspended-case
//!    correctness on that: under a live `PromptWindow` the registered
//!    ephemerals were already erased, so their flags are clear and the emit
//!    writes **no erase escape at all**, which is the only reason the notice
//!    lands below the question instead of wiping its row. A mock cannot show
//!    that the escape was truly absent from the wire — it can only re-read the
//!    flag it just asserted on.
//! 2. **`arbiter::tests::a_live_prompt_window_makes_painting_a_no_op`** proves
//!    the *ticker* is suppressed while a question is up. It cannot prove a
//!    **different** writer — the notice path, on another call stack — is also
//!    harmless, and that is exactly the writer that was not: `newt-tui`'s
//!    `summarizer_progress` wrote `\r\x1b[K` straight to stdout, held no lease,
//!    and could not see `suspended()`.
//!
//! # The bug this pins
//!
//! `summarizer_progress` (deleted by this migration) carried a doc comment
//! describing `LineLease::emit_line`'s contract, and then implemented it wrong
//! in two independent ways:
//!
//! - it cleared the row without clearing anyone's `painted` flag, so the next
//!   100 ms tick fired `Clear(UntilNewLine)` on the row the notice had just
//!   moved to;
//! - it could not see `suspended()`, so with a permission question on screen it
//!   erased the question's row and printed over it — the same class of
//!   invisible-prompt hang the arbiter was built to end, arriving through a
//!   second door.
//!
//! The `Legacy` scenario below is that implementation, verbatim, kept as a
//! **control**: the test asserts it damages the question and that the migrated
//! path does not. Without the control, the passing assertion would prove only
//! that some bytes were absent, not that their absence is the fix.
//!
//! # Why a PTY, and why a child process
//!
//! The spinner refuses to paint unless it can own a real line — correct
//! behavior, and precisely why this bug never surfaced in a piped test. And
//! `cargo test` installs a thread-local capture, so the scenario runs in a
//! child (this same binary, re-invoked with `--nocapture`) whose stdin and
//! stdout *are* the pty. No filesystem, no network, no service: a pty pair and
//! a re-exec.

use std::io::Write as _;
use std::time::Duration;

use tests_pty::Pty;

use super::widgets::{Level, Notice};
use super::{LineCaps, Sink, Spinner, Terminal};

/// The notice every scenario emits — a real one, copied from the summarizer's
/// fallback path so the test tracks the production text.
const NOTICE_TEXT: &str = "⚠ summarizer falling back to qwen:0.5b…";

/// The question the prompt scenarios put on screen before the notice fires.
const QUESTION: &str = "⊘ web_fetch wants example.com — [a]llow once, [d]eny > ";

/// Long enough for the 100 ms ticker to get several chances to redraw.
const DWELL: Duration = Duration::from_millis(250);

fn notice() -> Notice<'static> {
    Notice::new(Level::Warn, "", NOTICE_TEXT)
}

// ---------------------------------------------------------------------------
// The children: the scenarios themselves, run with fd 0/1 on a pty.
// ---------------------------------------------------------------------------

/// A notice fired while the spinner is live. The migrated path routes through
/// `Terminal::emit_line`, which erases the ephemeral row through its own lease
/// (clearing `painted`, so the next tick redraws *below* the notice).
#[test]
#[ignore = "child process of the notice/arbiter regression test"]
fn notice_under_spinner_child() {
    if std::env::var_os("NEWT_NOTICE_PTY_CHILD").as_deref() != Some("spinner".as_ref()) {
        return;
    }
    let spinner =
        Spinner::start_with_caps(LineCaps::Own, "compressing context…", Sink::Stdout, true)
            .expect("the pty is a real terminal, so the spinner takes the line");
    std::thread::sleep(DWELL);
    notice().emit(LineCaps::Own, Sink::Stdout, true);
    std::thread::sleep(DWELL);
    drop(spinner);
}

/// A notice fired while a `PromptWindow` is live — the question is on screen
/// and the process is about to block on a human.
#[test]
#[ignore = "child process of the notice/arbiter regression test"]
fn notice_under_prompt_window_child() {
    if std::env::var_os("NEWT_NOTICE_PTY_CHILD").as_deref() != Some("prompt".as_ref()) {
        return;
    }
    let spinner =
        Spinner::start_with_caps(LineCaps::Own, "compressing context…", Sink::Stdout, true)
            .expect("spinner");
    std::thread::sleep(DWELL);
    {
        let window = Terminal::suspend_for_prompt(crate::tty::TerminalTaker::PlainCliConfirm);
        window.ask(QUESTION).expect("the question reaches the pty");
        std::thread::sleep(Duration::from_millis(100));
        notice().emit(LineCaps::Own, Sink::Stdout, true);
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(spinner);
}

/// **The control.** `newt-tui`'s deleted `summarizer_progress`, verbatim, in the
/// same scenario as [`notice_under_prompt_window_child`]. It must damage the
/// question — that is what makes the assertion on the migrated path mean
/// something.
#[test]
#[ignore = "child process of the notice/arbiter regression test"]
fn legacy_notice_under_prompt_window_child() {
    if std::env::var_os("NEWT_NOTICE_PTY_CHILD").as_deref() != Some("legacy".as_ref()) {
        return;
    }
    let spinner =
        Spinner::start_with_caps(LineCaps::Own, "compressing context…", Sink::Stdout, true)
            .expect("spinner");
    std::thread::sleep(DWELL);
    {
        let window = Terminal::suspend_for_prompt(crate::tty::TerminalTaker::PlainCliConfirm);
        window.ask(QUESTION).expect("the question reaches the pty");
        std::thread::sleep(Duration::from_millis(100));
        // ---- the deleted implementation, exactly as it stood ----
        let mut out = std::io::stdout();
        let _ = write!(out, "\r\x1b[K\x1b[33m{NOTICE_TEXT}\x1b[0m\n");
        let _ = out.flush();
        // ---------------------------------------------------------
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(spinner);
}

/// **#1866, in vivo.** A real process in protocol mode reaching a real
/// prompt on a real terminal.
///
/// This scenario cannot be written in-process. `enter_protocol_mode` is
/// documented as "idempotent and one-way — there is no leaving it", and the
/// flag is process-global, so setting it in a unit test would veto every
/// sibling test's notice and lease for the rest of the binary. A child is not
/// a convenience here; it is the only place the real flag can be set.
///
/// The child asserts the refusals it gets. The parent asserts the two halves
/// of the epic criterion the child cannot see: that NOTHING reached the
/// terminal, and that the process did not WAIT.
#[test]
#[ignore = "child process of the notice/arbiter regression test"]
fn protocol_mode_prompt_child() {
    if std::env::var_os("NEWT_NOTICE_PTY_CHILD").as_deref() != Some("protocol".as_ref()) {
        return;
    }
    super::caps::enter_protocol_mode();

    let window = Terminal::suspend_for_prompt(crate::tty::TerminalTaker::PlainCliConfirm);

    // Deliberately NOT asserted yet. Asserting here would short-circuit the
    // child on the very mutation this test exists to catch, and the parent
    // would never reach the half that matters: with the veto removed, `ask`
    // succeeds, and the read below BLOCKS on a pty nobody will ever type into.
    // Ordering the read first is what makes the parent's `exited` assertion
    // load-bearing rather than decorative.
    let asked = window.ask(QUESTION);

    // The half that would HANG. It must come back immediately, and as an error
    // rather than the `Ok(0)` that would forge a deliberate empty answer.
    let mut buf = String::new();
    let read = window.read_line_into(&mut buf);

    assert!(
        asked.is_err(),
        "ask must refuse in protocol mode rather than report a question it \
         never put on screen"
    );
    assert!(read.is_err(), "read must refuse, not return EOF: {read:?}");
    assert!(
        buf.is_empty(),
        "nothing may be written into the caller's buffer"
    );

    // A notice is informational and is dropped silently — `Notice::emit`'s
    // documented protocol-mode behaviour, and the reason `ask` and `notice`
    // deliberately differ.
    window
        .notice(NOTICE_TEXT)
        .expect("a dropped notice is not an error");
    drop(window);

    // #1959: THE SECOND DOOR, under the same real flag.
    //
    // `suspend_for_prompt_to` routes `ask`/`notice` to an explicit terminal
    // File rather than stdout — the shape a process uses when fd 1 has been
    // redirected into an internal capture. The seal's value is ENUMERATED,
    // PROVEN doors, so a door that is only reasoned about is not proven.
    //
    // The file is `/dev/tty`, which in this child IS the pty slave the parent
    // reads. That is what makes this cost the parent nothing: its existing
    // "no prompt byte reached the terminal" assertions now cover BOTH doors,
    // and a veto that covered only the stdout path would put the question on
    // the parent's screen and fail there.
    // A dup of fd 1 rather than an open of `/dev/tty`: the parent hands the
    // slave to the child as stdin/stdout but does NOT make it the child's
    // controlling terminal (that needs `setsid` + `TIOCSCTTY`), so `/dev/tty`
    // has nothing to resolve to here. Duping fd 1 is also the truer fixture —
    // "the process kept a File for the real terminal" is exactly the shape
    // this door exists for.
    let terminal = {
        use std::os::fd::{FromRawFd, OwnedFd};
        // SAFETY: `dup` returns a fresh owned descriptor for the pty slave;
        // `OwnedFd` takes ownership and the `File` closes it on drop.
        let raw = unsafe { libc::dup(1) };
        assert!(
            raw >= 0,
            "dup(1) failed: {}",
            std::io::Error::last_os_error()
        );
        std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) })
    };
    let window =
        Terminal::suspend_for_prompt_to(terminal, crate::tty::TerminalTaker::PlainCliConfirm);
    let asked = window.ask(QUESTION);
    let mut buf = String::new();
    let read = window.read_line_into(&mut buf);
    assert!(
        asked.is_err(),
        "the File door must refuse in protocol mode too — the veto sits in \
         the shared constructor precisely so neither door can miss it"
    );
    assert!(read.is_err(), "the File door must refuse to read: {read:?}");
    assert!(
        buf.is_empty(),
        "nothing may be written into the caller's buffer"
    );
    window
        .notice(NOTICE_TEXT)
        .expect("a dropped notice is not an error");
    drop(window);
}

// ---------------------------------------------------------------------------
// The parent: allocate the pty, drive a child, read the screen.
// ---------------------------------------------------------------------------

/// Run one scenario on a fresh pty and return everything the terminal saw.
///
/// The pty plumbing lives in `tests-pty` (#1410) — the slave becomes the
/// child's stdin+stdout, the master is what we read the screen from.
fn run_scenario(scenario: &str, child_test: &str) -> String {
    let pty = Pty::open();
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", child_test, "--ignored", "--nocapture"])
    .env("NEWT_NOTICE_PTY_CHILD", scenario)
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn the pty child");
    let status = child.wait().expect("wait for the pty child");
    let screen = pty.screen();
    assert!(
        status.success(),
        "the {scenario} scenario child failed.\n\nscreen:\n{screen:?}"
    );
    screen
}

/// Braille frames (the spinner's alphabet) present in `s`.
fn frames(s: &str) -> Vec<char> {
    s.chars()
        .filter(|c| ('\u{2800}'..='\u{28FF}').contains(c))
        .collect()
}

// ---------------------------------------------------------------------------
// (a) spinner live
// ---------------------------------------------------------------------------

/// §5 row 4 (a): with a spinner live, an emitted notice produces no interleaved
/// bytes — the notice reaches scrollback whole, and the spinner goes on
/// spinning below it.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_notice_emitted_under_a_live_spinner_is_not_interleaved() {
    let screen = run_scenario(
        "spinner",
        "tty::pty_notice_test::notice_under_spinner_child",
    );

    let start = screen
        .find(NOTICE_TEXT)
        .unwrap_or_else(|| panic!("the notice never reached the terminal.\n\nscreen:\n{screen:?}"));

    // The notice is written under ONE stdout lock — colour, text, reset and the
    // newline in a single queued batch — so nothing may appear inside it. This
    // is the property no mock can observe: the unit tier can only assert that
    // `emit_line` was called.
    let end = screen[start..]
        .find('\n')
        .map(|i| start + i)
        .unwrap_or(screen.len());
    assert!(
        frames(&screen[start..end]).is_empty(),
        "a spinner frame was painted INSIDE the notice line.\n\nnotice row:\n{:?}",
        &screen[start..end]
    );

    // And the row was genuinely handed back: `emit_line` erases through the
    // lease, which clears `painted`, so the ticker redraws *below* the notice
    // rather than over it. If `painted` had been left set — the first half of
    // the summarizer race — the next tick would have fired `Clear(UntilNewLine)`
    // on the row the notice had just moved to.
    assert!(
        !frames(&screen[end..]).is_empty(),
        "the spinner never resumed after the notice, so the ephemeral row was \
         not returned.\n\nafter the notice:\n{:?}",
        &screen[end..]
    );
}

// ---------------------------------------------------------------------------
// (b) PromptWindow live — and the control that proves the assertion bites
// ---------------------------------------------------------------------------

/// The bytes the terminal saw between the end of the question and the start of
/// the notice. Everything that could damage the question lives in this window.
fn window_between_question_and_notice(screen: &str) -> String {
    let q_end = screen
        .find(QUESTION)
        .map(|i| i + QUESTION.len())
        .unwrap_or_else(|| {
            panic!("the question never reached the terminal.\n\nscreen:\n{screen:?}")
        });
    let n_start = screen[q_end..]
        .find(NOTICE_TEXT)
        .map(|i| q_end + i)
        .unwrap_or_else(|| panic!("the notice never reached the terminal.\n\nscreen:\n{screen:?}"));
    screen[q_end..n_start].to_string()
}

/// §5 row 4 (b): with a `PromptWindow` live, the notice must NOT overwrite the
/// question.
///
/// The question is unterminated (`ask` writes no newline — the cursor parks
/// after `> ` where the operator types), so it owns the current row. An erase
/// escape between the question and the notice would wipe that row and leave the
/// operator blocked on a question they cannot see. There must be none: every
/// registered ephemeral was already erased when the window was handed out, so
/// their `painted` flags are clear and `Terminal::emit_line`'s erase is a no-op
/// that writes nothing.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_notice_emitted_under_a_prompt_window_does_not_overwrite_the_question() {
    let screen = run_scenario(
        "prompt",
        "tty::pty_notice_test::notice_under_prompt_window_child",
    );
    let window = window_between_question_and_notice(&screen);

    assert!(
        !window.contains("\x1b[K"),
        "an erase escape was written between the question and the notice — the \
         question's row was wiped and the operator is blocked on a question \
         they cannot see.\n\nwindow:\n{window:?}"
    );
    assert!(
        frames(&window).is_empty(),
        "the ticker painted over the question while it was up.\n\nwindow:\n{window:?}"
    );
    // The question survives whole, after everything.
    assert!(
        screen.contains(QUESTION),
        "the question did not survive.\n\nscreen:\n{screen:?}"
    );
}

/// **The control**, and the evidence that the assertion above is not vacuous:
/// the deleted `summarizer_progress` fails it. Run against the pre-migration
/// implementation, an erase escape lands squarely between the question and the
/// notice — which is the reported class of hang.
///
/// If this test ever stops failing to find `\x1b[K`, the assertion in
/// [`a_notice_emitted_under_a_prompt_window_does_not_overwrite_the_question`]
/// has stopped measuring anything and both need revisiting.
#[serial_test::serial(tty_arbiter)]
#[test]
fn the_legacy_unleased_notice_did_overwrite_the_question() {
    let screen = run_scenario(
        "legacy",
        "tty::pty_notice_test::legacy_notice_under_prompt_window_child",
    );
    let window = window_between_question_and_notice(&screen);

    assert!(
        window.contains("\x1b[K"),
        "the control no longer reproduces the bug, so the migrated path's \
         assertion proves nothing.\n\nwindow:\n{window:?}"
    );
}
// ---------------------------------------------------------------------------
// (c) #1866 — the protocol-mode veto, proved in vivo
// ---------------------------------------------------------------------------

/// Drive a scenario with a BOUNDED wait, so "the child blocked" is a failure
/// rather than a hung suite. Returns the screen and whether it exited in time.
fn run_scenario_bounded(scenario: &str, child_test: &str, budget: Duration) -> (String, bool) {
    let pty = Pty::open();
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", child_test, "--ignored", "--nocapture"])
    .env("NEWT_NOTICE_PTY_CHILD", scenario)
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn the pty child");

    let deadline = std::time::Instant::now() + budget;
    let mut screen = String::new();
    let exited = loop {
        screen.push_str(&pty.screen());
        match child.try_wait().expect("poll the pty child") {
            Some(status) => {
                assert!(
                    status.success(),
                    "the {scenario} scenario child failed.\n\nscreen:\n{screen:?}"
                );
                break true;
            }
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    screen.push_str(&pty.screen());
    (screen, exited)
}

/// **The #1866 proof.** Epic #1803: headless/protocol modes never wait, choose
/// defaults, or emit terminal bytes. A prompt is all three, so this asserts
/// both observable halves against a real terminal.
///
/// The CONTROL is `a_notice_emitted_under_a_prompt_window_does_not_overwrite_the_question`
/// above, which drives the SAME `suspend_for_prompt` -> `ask(QUESTION)` path with
/// flag unset and asserts the question DOES reach the pty. Without it, "the
/// question is absent" would be satisfied by a build where prompts never
/// worked at all — and asserting today's behaviour is exactly the vacuity this
/// issue is about, since today's behaviour is already correct for the wrong
/// reason (no protocol-mode caller happens to reach a prompt).
/// Serial and un-ignored, like every other parent in this module: it drives
/// the process-global arbiter through a child, and #1866's whole complaint is
/// that nothing checked this per-PR.
#[serial_test::serial(tty_arbiter)]
#[test]
fn protocol_mode_emits_no_prompt_bytes_and_does_not_wait() {
    let (screen, exited) = run_scenario_bounded(
        "protocol",
        "tty::pty_notice_test::protocol_mode_prompt_child",
        Duration::from_secs(20),
    );

    assert!(
        exited,
        "the protocol-mode child BLOCKED. `read_line` waited for an operator \
         who cannot exist on a JSON-RPC wire.\n\nscreen:\n{screen:?}"
    );
    assert!(
        !screen.contains(QUESTION),
        "the question reached a machine protocol channel.\n\nscreen:\n{screen:?}"
    );
    assert!(
        !screen.contains(NOTICE_TEXT),
        "a notice reached a machine protocol channel.\n\nscreen:\n{screen:?}"
    );
}

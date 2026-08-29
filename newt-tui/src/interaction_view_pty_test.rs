//! **Real-PTY acceptance test for the interaction view's TERMINAL LIFECYCLE**
//! (C2, #1876).
//!
//! What only this tier can prove: the inline interaction frame takes a real
//! terminal into raw mode and must hand it back on **every** exit path. No
//! mock can observe that — the thing at risk is the state of an actual tty
//! device, not a value in our process. This grounds `InlineGuard`'s
//! Drop-based restoration claim and the module's pure row-model tests, which
//! by construction never touch a terminal.
//!
//! Modelled on `transcript_pager_pty_test` (#1677), including the control
//! that makes the result mean something: **`raw_during`**. A test that never
//! observed raw mode would "prove" restoration vacuously — it would pass
//! against a build where `InlineGuard::enter` did nothing at all.
//!
//! # Why the failure path is the one that matters
//!
//! A clean close restoring the terminal is the easy half; every
//! implementation gets it right. The half that strands an operator in a shell
//! that no longer echoes is the unwind — and `config_panel::run` shows the
//! shape that gets it wrong, calling `disable_raw_mode()` as a statement
//! after its loop, which a panic skips entirely. `InlineGuard` owes the
//! restore from `Drop`, and `panic_restores_the_terminal` is what says so
//! against a real device rather than by inspection.
//!
//! # The postcondition is KERNEL state, not escape bytes
//!
//! [`Pty::is_raw`] runs `tcgetattr` on the parent's own slave fd, and pty line
//! discipline belongs to the *device*, so the parent observes exactly the
//! raw/cooked state the child installed. A child could emit a plausible-looking
//! restore sequence and still leave the tty raw — and that failure is precisely
//! the one that wrecks a terminal.

use std::time::{Duration, Instant};

use tests_pty::Pty;

use crate::interaction_view::InlineGuard;
use crate::prompt_visibility_test::wait_for_child;

const CHILD_TEST: &str = "interaction_view_pty_test::interaction_view_child";

/// Generous: this tier runs under parallel load on shared runners.
const REACH_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The child half: takes the real terminal over exactly as an inline
/// interaction frame does. `NEWT_INTERACTION_PTY_CHILD` selects the lifecycle.
#[test]
#[ignore = "child process of the interaction-view PTY acceptance test"]
fn interaction_view_child() {
    let Some(mode) = std::env::var_os("NEWT_INTERACTION_PTY_CHILD") else {
        return;
    };
    // HOLD the guard until the parent has sampled the tty. Exiting immediately
    // is a race the parent loses: the restore would run before the first
    // `is_raw()` read, and the test would "pass" without ever having observed
    // raw mode — which is exactly what the `raw_during` control refuses to
    // accept. One blocking byte-read makes the window deterministic.
    let hold = || {
        use std::io::Read as _;
        let mut byte = [0u8; 1];
        let _ = std::io::stdin().read(&mut byte);
    };
    match mode.to_string_lossy().as_ref() {
        // The ordinary path: the frame closes and the scope ends.
        "clean" => {
            let _guard = InlineGuard::enter().expect("enter raw mode");
            hold();
        }
        // The failure path AFTER entry: the guard is live and the process
        // unwinds. Restoration must not depend on reaching the end of the
        // function — it is a Drop obligation.
        "panic" => {
            let _guard = InlineGuard::enter().expect("enter raw mode");
            hold();
            panic!("synthetic failure while the interaction frame is up (#1876)");
        }
        // An error RETURN out of the middle, which is how a failed
        // `terminal.draw` leaves `present`. The guard is dropped by the `?`
        // unwind of scope, not by any cleanup the author remembered to write.
        "error" => {
            fn inner() -> std::io::Result<()> {
                let _guard = InlineGuard::enter()?;
                {
                    use std::io::Read as _;
                    let mut byte = [0u8; 1];
                    let _ = std::io::stdin().read(&mut byte);
                }
                Err(std::io::Error::other("synthetic draw failure"))
            }
            assert!(inner().is_err(), "the fixture must take the error path");
        }
        // **Nested modals.** An interaction frame opened over a frame that is
        // already up — the shape `modal.rs::RawGuard` was written for
        // (#1770). The child holds TWICE so the parent can sample the tty in
        // the window BETWEEN the inner drop and the outer one, which is the
        // only moment the defect is visible: if the inner guard restores
        // globally, the outer frame is still drawn but the terminal is
        // already cooked, so its keyboard is line-buffered and echoing.
        "nested" => {
            let _outer = InlineGuard::enter().expect("enter raw mode (outer)");
            {
                let _inner = InlineGuard::enter().expect("enter raw mode (inner)");
                hold();
            } // inner drops here; the OUTER frame is still up.
            hold();
        }
        other => panic!("unknown child mode {other:?}"),
    }
}

/// Everything the parent needs to judge one lifecycle.
struct Outcome {
    raw_after: bool,
    /// Was the pty raw while the frame was up? **The control**: a test that
    /// never observed raw mode would prove restoration vacuously.
    raw_during: bool,
    screen: String,
}

fn drive(mode: &str) -> Outcome {
    let pty = Pty::open();
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
    .env("NEWT_INTERACTION_PTY_CHILD", mode)
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn the pty child");

    // There is no alternate-screen byte to poll for — this surface
    // deliberately never enters one — so the reach condition IS the kernel
    // state we care about.
    let mut screen = String::new();
    let raw_during = {
        let deadline = Instant::now() + REACH_TIMEOUT;
        loop {
            screen.push_str(&pty.screen());
            if pty.is_raw() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // Release the child's blocking read so it proceeds to its exit path.
    pty.type_in("x");

    let _status = std::thread::scope(|scope| {
        let waiter = scope.spawn(|| wait_for_child(&mut child, EXIT_TIMEOUT));
        while !waiter.is_finished() {
            screen.push_str(&pty.screen());
            std::thread::sleep(Duration::from_millis(20));
        }
        waiter.join().expect("child reaper thread")
    });
    // Drain whatever the exit path emitted.
    std::thread::sleep(Duration::from_millis(80));
    screen.push_str(&pty.screen());

    Outcome {
        raw_after: pty.is_raw(),
        raw_during,
        screen,
    }
}

/// What the parent observed across a NESTED lifecycle.
struct NestedOutcome {
    /// Raw once the outer guard was up? (The control.)
    raw_during_outer: bool,
    /// Raw in the window after the INNER guard dropped but while the OUTER
    /// frame is still live? **This is the property.**
    raw_after_inner_drop: bool,
    /// Cooked once both are gone?
    raw_after_all: bool,
}

/// Drive the nested lifecycle, sampling in the window between the two drops.
fn drive_nested() -> NestedOutcome {
    let pty = Pty::open();
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
    .env("NEWT_INTERACTION_PTY_CHILD", "nested")
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn the pty child");

    let raw_during_outer = {
        let deadline = Instant::now() + REACH_TIMEOUT;
        loop {
            if pty.is_raw() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // Release the INNER hold. The child drops the inner guard and parks on
    // the second hold with the outer frame still up.
    pty.type_in("x");
    // Give the inner Drop time to run before sampling. Sampling too early
    // would read the pre-drop state and pass vacuously.
    std::thread::sleep(Duration::from_millis(250));
    let raw_after_inner_drop = pty.is_raw();

    // Release the OUTER hold and let the child exit.
    pty.type_in("x");
    let _ = std::thread::scope(|scope| {
        let waiter = scope.spawn(|| wait_for_child(&mut child, EXIT_TIMEOUT));
        while !waiter.is_finished() {
            let _ = pty.screen();
            std::thread::sleep(Duration::from_millis(20));
        }
        waiter.join().expect("child reaper thread")
    });
    std::thread::sleep(Duration::from_millis(80));
    let _ = pty.screen();

    NestedOutcome {
        raw_during_outer,
        raw_after_inner_drop,
        raw_after_all: pty.is_raw(),
    }
}

fn assert_restored(what: &str, out: &Outcome) {
    assert!(
        out.raw_during,
        "{what}: the pty was never raw — the test proved nothing about \
         restoration, because it never observed the state being restored FROM"
    );
    assert!(
        !out.raw_after,
        "{what}: the terminal was left in RAW mode — an operator's shell would \
         be unusable. This is kernel state via tcgetattr, not an inference \
         from escape bytes; screen={:?}",
        out.screen
    );
}

/// **The alternate screen is never entered.**
///
/// `plain_scroller_tui.md` permits an alt-screen modal on RichTUI, but the
/// carve-out is conditional: *"Operator-invoked and modal. It opens on an
/// explicit command … not ambiently, and never during a turn."* An
/// interaction prompt is model-triggered and happens DURING a turn, so it
/// satisfies neither condition and must stay inline. Asserted on the bytes
/// because that is where the violation would be visible.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn the_interaction_frame_never_enters_the_alternate_screen() {
    let out = drive("clean");
    assert!(
        out.raw_during,
        "the pty was never raw; nothing was exercised"
    );
    assert!(
        !out.screen.contains("\x1b[?1049h"),
        "the interaction frame entered the alternate screen, which the \
         plain-scroller carve-out does not permit during a turn; screen={:?}",
        out.screen
    );
}

#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_clean_close_leaves_the_terminal_exactly_as_it_was_found() {
    assert_restored("clean", &drive("clean"));
}

/// **The failure path.** The one that turns a bug into a wrecked terminal,
/// and the one least likely to be noticed missing.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_panic_while_the_frame_is_up_still_restores_the_terminal() {
    assert_restored("panic", &drive("panic"));
}

/// **The error-return path.** How a failed `terminal.draw` leaves `present`:
/// the guard is dropped by scope unwind, not by cleanup the author
/// remembered to write.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn an_error_return_while_the_frame_is_up_still_restores_the_terminal() {
    assert_restored("error", &drive("error"));
}

/// **A nested frame must not hand the terminal back while the outer one is
/// still up.**
///
/// The failure this catches is not a leak but its mirror: an over-eager
/// restore. `crossterm::enable_raw_mode` keeps ONE process-global "mode prior
/// to raw" and makes a second call a no-op, so a guard built on it sees the
/// inner `enter` do nothing and the inner `drop` restore GLOBALLY. The outer
/// frame is still drawn, but the terminal is already cooked — keys
/// line-buffered until Enter and echoed by the kernel over the frame, which
/// is exactly the "prompt that looked hung" `modal.rs::RawGuard` documents
/// from #1770.
///
/// Observable only from outside, and only in the window between the two
/// drops, which is why the child holds twice.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_nested_frame_does_not_restore_the_terminal_early() {
    let out = drive_nested();
    assert!(
        out.raw_during_outer,
        "the pty was never raw — the test proved nothing, because it never \
         observed the state that must be preserved"
    );
    assert!(
        out.raw_after_inner_drop,
        "the INNER frame's drop handed the terminal back while the OUTER \
         frame was still up: raw mode was lost mid-modal, so the outer \
         frame's keyboard is line-buffered and kernel-echoed (#1770)"
    );
    assert!(
        !out.raw_after_all,
        "both frames are gone and the terminal is still RAW"
    );
}

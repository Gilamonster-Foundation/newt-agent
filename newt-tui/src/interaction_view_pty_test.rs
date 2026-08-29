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

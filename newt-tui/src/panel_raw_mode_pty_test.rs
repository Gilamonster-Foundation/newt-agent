//! **Real-PTY acceptance test for the operator panels' RAW-MODE lifecycle**
//! (#1889).
//!
//! `config_panel::run` and `backend_panel::run` take a real terminal into raw
//! mode and must hand it back on **every** exit path. Both used to call
//! `disable_raw_mode()` as a statement after the loop closure: an error return
//! reached it, and a panic unwound straight past, leaving the operator in a
//! shell with no echo. [`PanelRawGuard`] makes the restore a Drop obligation;
//! this is what says so against an actual tty.
//!
//! Modelled on `interaction_view_pty_test` (C2a, #1876) and, through it,
//! `transcript_pager_pty_test` (#1677) — including the control that makes the
//! result mean anything: **`raw_during`**. A test that never observed raw mode
//! would prove restoration vacuously; it would pass against a build where
//! `PanelRawGuard::enter` did nothing at all.
//!
//! # Why a child process is unavoidable
//!
//! A Drop-on-unwind restoration cannot be observed from inside the process
//! doing the unwinding. The parent must outlive the child and read the tty's
//! own state. Running it in-process would leave a test that owns no terminal,
//! which proves nothing.
//!
//! # The postcondition is KERNEL state, not escape bytes
//!
//! [`Pty::is_raw`] runs `tcgetattr` on the parent's own slave fd, and pty line
//! discipline belongs to the *device*, so the parent observes exactly the
//! raw/cooked state the child installed. A child could emit a plausible
//! restore sequence and still leave the tty raw — and that is precisely the
//! failure that wrecks a terminal.
//!
//! # What this does NOT prove, and what does
//!
//! It proves the GUARD restores. It cannot prove the panels USE the guard —
//! driving `run` needs an interactive key loop. A guard that is correct and
//! unused is exactly the state these files were in, so that half is pinned
//! structurally by `config_panel::tests::enter_panel_raw_mode_is_the_only_way_in`.
//! Neither test is sufficient alone.

use std::time::{Duration, Instant};

use tests_pty::Pty;

use crate::config_panel::PanelRawGuard;
use crate::prompt_visibility_test::wait_for_child;

const CHILD_TEST: &str = "panel_raw_mode_pty_test::panel_raw_mode_child";

/// Generous: this tier runs under parallel load on shared runners.
const REACH_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The child half: takes the real terminal exactly as a panel does.
/// `NEWT_PANEL_PTY_CHILD` selects the lifecycle.
#[test]
#[ignore = "child process of the panel raw-mode PTY acceptance test"]
fn panel_raw_mode_child() {
    let Some(mode) = std::env::var_os("NEWT_PANEL_PTY_CHILD") else {
        return;
    };
    // HOLD the guard until the parent has sampled the tty. Exiting immediately
    // is a race the parent loses: the restore would run before the first
    // `is_raw()` read and the test would "pass" without ever having observed
    // raw mode — which is what `raw_during` refuses to accept.
    let hold = || {
        use std::io::Read as _;
        let mut byte = [0u8; 1];
        let _ = std::io::stdin().read(&mut byte);
    };
    match mode.to_string_lossy().as_ref() {
        // The ordinary path: the panel closes and the scope ends.
        "clean" => {
            let _guard = PanelRawGuard::enter().expect("enter raw mode");
            hold();
        }
        // THE ONE THAT MATTERS. A panic with the panel up — the path the bare
        // `disable_raw_mode()` statement skipped entirely.
        "panic" => {
            let _guard = PanelRawGuard::enter().expect("enter raw mode");
            hold();
            panic!("synthetic failure while the panel is up (#1889)");
        }
        // An error RETURN out of the middle, which is how a failed
        // `terminal.draw` leaves `run`. The guard is dropped by scope unwind,
        // not by cleanup the author remembered to write.
        "error" => {
            fn inner() -> std::io::Result<()> {
                let _guard = PanelRawGuard::enter()?;
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
    /// Was the pty raw while the panel was up? **The control**: a test that
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
    .env("NEWT_PANEL_PTY_CHILD", mode)
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn the pty child");

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

#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_clean_panel_close_leaves_the_terminal_exactly_as_it_was_found() {
    assert_restored("clean", &drive("clean"));
}

/// **The failure path**, and the whole reason #1889 exists: the one that turns
/// a bug into a wrecked session, and the one least likely to be noticed
/// missing, because the happy path always restores.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_panic_while_the_panel_is_up_still_restores_the_terminal() {
    assert_restored("panic", &drive("panic"));
}

/// **The error-return path**: how a failed `terminal.draw` leaves `run`.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn an_error_return_while_the_panel_is_up_still_restores_the_terminal() {
    assert_restored("error", &drive("error"));
}

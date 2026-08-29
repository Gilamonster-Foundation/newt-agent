//! **Real-PTY acceptance test for the rich surface's TERMINAL MODES** (#1898).
//!
//! `rich_input::read_turn` takes raw mode **and** bracketed paste for a turn and
//! must hand both back on every exit path. #1411 recorded this site as safe
//! because it "avoids `?` around its event loop" — true, and it covers the error
//! path only. A panic inside `event_loop` unwound past both restores.
//! [`RawPasteGuard`] makes them a Drop obligation; this is what says so against
//! an actual tty.
//!
//! Modelled on `panel_raw_mode_pty_test` (#1889) and, through it, C2a's
//! `interaction_view_pty_test` (#1876) and `transcript_pager_pty_test` (#1677).
//!
//! # Why a child process is unavoidable
//!
//! A Drop-on-unwind restoration cannot be observed from inside the process doing
//! the unwinding. The parent must outlive the child and read the tty's own state.
//! In-process this would be a test that owns no terminal, which proves nothing.
//!
//! # TWO postconditions, and only one of them is escape bytes
//!
//! Raw mode is KERNEL state: [`Pty::is_raw`] runs `tcgetattr` on the parent's own
//! slave fd, and line discipline belongs to the *device*, so a child that emits a
//! plausible restore sequence and still leaves the tty raw is caught.
//!
//! Bracketed paste has no kernel state — it is a terminal mode set by
//! `ESC[?2004h` and cleared by `ESC[?2004l`, and the bytes are all there is to
//! observe. So it is asserted on the stream, with `paste_during` as its control:
//! without seeing the mode turned ON, seeing it turned OFF would prove nothing.
//!
//! # What this does NOT prove
//!
//! That `read_turn` uses the guard — driving the event loop needs a real
//! interactive turn. That half is
//! `rich_input::tests::raw_and_paste_are_owned_by_one_guard`, with the teardown
//! ORDER pinned by `the_guard_releases_paste_before_raw_mode`. Neither test is
//! sufficient alone.

use std::time::{Duration, Instant};

use tests_pty::Pty;

use crate::prompt_visibility_test::wait_for_child;
use crate::rich_input::RawPasteGuard;

const CHILD_TEST: &str = "rich_input_pty_test::rich_input_child";

/// The two mode strings, spelled once.
const PASTE_ON: &str = "\x1b[?2004h";
const PASTE_OFF: &str = "\x1b[?2004l";

/// Generous: this tier runs under parallel load on shared runners.
const REACH_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The child half: takes the terminal exactly as a rich turn does.
/// `NEWT_RICH_INPUT_PTY_CHILD` selects the lifecycle.
#[test]
#[ignore = "child process of the rich-input PTY acceptance test"]
fn rich_input_child() {
    let Some(mode) = std::env::var_os("NEWT_RICH_INPUT_PTY_CHILD") else {
        return;
    };
    // HOLD until the parent has sampled the tty. Exiting immediately is a race
    // the parent loses: the restore would run before the first read and the
    // test would "pass" without ever observing the state being restored FROM.
    let hold = || {
        use std::io::Read as _;
        let mut byte = [0u8; 1];
        let _ = std::io::stdin().read(&mut byte);
    };
    match mode.to_string_lossy().as_ref() {
        // The ordinary path: the turn ends and the scope closes.
        "clean" => {
            let _guard = RawPasteGuard::enter().expect("take the terminal");
            hold();
        }
        // THE ONE #1411 CLEARED THIS SITE FOR. A panic inside the event loop.
        "panic" => {
            let _guard = RawPasteGuard::enter().expect("take the terminal");
            hold();
            panic!("synthetic failure inside the rich event loop (#1898)");
        }
        // An error RETURN out of the middle — the path #1411's reasoning DID
        // cover, kept so the mutation can show which paths each shape protects.
        "error" => {
            fn inner() -> std::io::Result<()> {
                let _guard = RawPasteGuard::enter()?;
                {
                    use std::io::Read as _;
                    let mut byte = [0u8; 1];
                    let _ = std::io::stdin().read(&mut byte);
                }
                Err(std::io::Error::other("synthetic event-loop failure"))
            }
            assert!(inner().is_err(), "the fixture must take the error path");
        }
        other => panic!("unknown child mode {other:?}"),
    }
}

/// Everything the parent needs to judge one lifecycle.
struct Outcome {
    raw_after: bool,
    /// **Control**: was the pty raw while the turn was up? A test that never
    /// observed raw mode would prove restoration vacuously.
    raw_during: bool,
    /// **Control**: was bracketed paste ever turned ON? Without it, "we saw
    /// `?2004l`" would pass against a build that never enabled it.
    paste_during: bool,
    screen: String,
}

fn drive(mode: &str) -> Outcome {
    let pty = Pty::open();
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
    .env("NEWT_RICH_INPUT_PTY_CHILD", mode)
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
    let paste_during = screen.contains(PASTE_ON);

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
        paste_during,
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
        out.paste_during,
        "{what}: bracketed paste was never enabled, so finding it disabled \
         afterwards proves nothing; screen={:?}",
        out.screen
    );
    assert!(
        !out.raw_after,
        "{what}: the terminal was left in RAW mode — an operator's shell would \
         be unusable. This is kernel state via tcgetattr, not an inference from \
         escape bytes; screen={:?}",
        out.screen
    );
    assert!(
        out.screen.contains(PASTE_OFF),
        "{what}: bracketed paste was never disabled — the next thing to read \
         the terminal receives a literal ESC[200~ around any paste; screen={:?}",
        out.screen
    );
}

#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_clean_turn_hands_back_raw_mode_and_bracketed_paste() {
    assert_restored("clean", &drive("clean"));
}

/// **The path #1411 cleared this site for and did not cover.**
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_panic_in_the_event_loop_still_hands_both_back() {
    assert_restored("panic", &drive("panic"));
}

/// The error-return path — the one the old shape *did* protect. Kept so the
/// mutation can show exactly which paths each shape covers.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn an_error_return_still_hands_both_back() {
    assert_restored("error", &drive("error"));
}

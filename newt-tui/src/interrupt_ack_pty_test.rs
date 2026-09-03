//! #2010 — PTY acceptance for press-time interrupt acknowledgement: on a real
//! terminal, with a real spinner holding the line and a real turn that
//! CANNOT stop yet, every Ctrl-C — the 1st, the 2nd and the 3rd — changes
//! the rendered grid at press time.
//!
//! **What it grounds.** The spinner's label substitution and the watcher's
//! press counter are each unit-tested in memory. Neither can tell you that a
//! byte typed at a terminal becomes a different label ON SCREEN while the
//! turn is still blocked — which is exactly the defect: the second press used
//! to set a flag that was read only after the turn returned, so the 2nd and
//! the 10th press were indistinguishable from the 1st until the turn yielded
//! on its own. A test that asserts only "the flag was set" is that defect one
//! layer up.
//!
//! **Why there is no stopwatch.** The child's turn blocks until a RELEASE
//! press the parent sends only after the grid has shown every label under
//! test, so each press is typed while the previous one's acknowledgement is
//! already on screen. An acknowledgement that appeared only when the turn
//! returned would never show — the parent's wait hits its reach timeout —
//! rather than pass late. The assertion is therefore "on screen while the
//! turn is still running", which no CI load can turn into a false pass, and
//! is asserted on the rendered GRID (#1986 bar), not the byte stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tests_pty::{screen_grid, Pty};

use crate::prompt_visibility_test::wait_for_child;
use newt_core::tty::{LineCaps, Sink, Spinner};

const CHILD_TEST: &str = "interrupt_ack_pty_test::interrupt_ack_child";
/// Prefix on the child's committed report line; nothing a spinner frame or a
/// cargo-harness line can collide with.
const TAG: &str = "[interrupt-ack]";
/// The label the child's turn shows once the watcher is armed (cbreak on,
/// Ctrl-C delivered as a byte rather than a SIGINT). Typing before this is on
/// screen would land in the pty's line discipline and kill the child.
const ARMED: &str = "armed — press Ctrl-C";
/// Three presses under test, then one more that releases the turn — the
/// spinner erases its row the instant the turn returns, so the label for the
/// last press under test has to be read BEFORE the release.
const RELEASE_PRESS: u32 = 4;
const REACH_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
/// The child's own ceiling, shorter than the parent's so a stuck child
/// reports what it saw instead of being killed anonymously.
const CHILD_TURN_TIMEOUT: Duration = Duration::from_secs(30);

/// The child half: a real spinner on the inherited pty, under the real
/// keyboard watcher, around a "turn" that ignores `cancel` and blocks until
/// the release press has been counted — the turn that cannot stop yet.
#[test]
#[ignore = "child process of the interrupt-acknowledgement PTY test"]
fn interrupt_ack_child() {
    if std::env::var_os("NEWT_INTERRUPT_ACK_PTY_CHILD").is_none() {
        return;
    }
    let spinner = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
        .expect("the pty is a real terminal, so the spinner takes the line");
    let cancel = AtomicBool::new(false);
    let seen = crate::with_interrupt_watch(true, &cancel, || {
        // Only now is the terminal in cbreak: say so through the line the
        // spinner owns, never a second writer.
        spinner.set_label(ARMED);
        let deadline = Instant::now() + CHILD_TURN_TIMEOUT;
        while newt_core::tty::interrupt_presses() < RELEASE_PRESS && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        newt_core::tty::interrupt_presses()
    });
    drop(spinner);
    println!(
        "{TAG} presses={seen} cancel={}",
        cancel.load(Ordering::Relaxed)
    );
}

#[test]
fn every_interrupt_press_changes_the_grid_while_the_turn_still_runs() {
    let pty = Pty::open();
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
    .env("NEWT_INTERRUPT_ACK_PTY_CHILD", "1")
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    .stderr(pty.slave_stdio())
    .spawn()
    .expect("spawn the pty child");

    let mut transcript = String::new();
    // Wait until `marker` is on the rendered GRID — the spinner repaints its
    // row in place, so the byte stream would match a frame that has since
    // been overwritten.
    let mut wait_on_grid = |marker: &str| -> String {
        let deadline = Instant::now() + REACH_TIMEOUT;
        loop {
            transcript.push_str(&pty.screen());
            let grid = screen_grid(&transcript).join("\n");
            if grid.contains(marker) {
                return grid;
            }
            assert!(
                Instant::now() < deadline,
                "`{marker}` never RENDERED while the turn was still running.\ngrid:\n{grid}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // 0. The spinner holds the line and the watcher is armed.
    wait_on_grid(ARMED);

    // 1. The first press: acknowledged (this half already held before #2010).
    pty.type_in("\u{3}");
    wait_on_grid("interrupting…");

    // 2. The second press: THE regression. It must be visibly different
    //    from the first, on screen, while the turn is still blocked.
    pty.type_in("\u{3}");
    wait_on_grid("×2 heard");

    // 3. And the third, so "second" is not a special case.
    pty.type_in("\u{3}");
    let grid = wait_on_grid("×3 heard");

    // 4. Only now release the turn.
    pty.type_in("\u{3}");
    let status =
        wait_for_child(&mut child, EXIT_TIMEOUT).expect("the child exited within the timeout");
    transcript.push_str(&pty.screen());
    assert!(
        status.success() && transcript.contains(&format!("{TAG} presses=4 cancel=true")),
        "the child's turn did not see all three presses: {status:?}\ngrid:\n{grid}\n\
         transcript:\n{transcript}"
    );
}

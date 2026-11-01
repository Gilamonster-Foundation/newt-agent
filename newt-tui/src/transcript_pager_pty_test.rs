//! Real-PTY acceptance test for the transcript pager's TERMINAL LIFECYCLE
//! (#1677).
//!
//! What only this tier can prove: the pager takes a real terminal over — raw
//! mode plus the alternate screen — and must hand it back on **every** exit
//! path. No mock can observe that, because the thing at risk is the state of
//! an actual tty device, not a value in our process. This grounds
//! `AltScreenGuard`'s Drop-based restoration claim and the module's pure
//! row-model tests, which by construction never touch a terminal.
//!
//! The postcondition is asserted as KERNEL state, not as escape bytes:
//! [`Pty::is_raw`] runs `tcgetattr` on the parent's own slave fd, and pty line
//! discipline belongs to the *device*, so the parent observes exactly the
//! raw/cooked state the child installed. Bytes are checked too (the
//! `?1049h`/`?1049l` alternate-screen pair), but they are corroboration — a
//! child could emit `?1049l` and still leave the tty in raw mode, and that
//! failure is precisely the one that strands an operator's terminal.
//!
//! Determinism: `enable_raw_mode()` runs BEFORE `EnterAlternateScreen` in
//! [`AltScreenGuard::enter`], so observing `?1049h` on the master proves the
//! child is already raw. The parent polls for that byte instead of sleeping a
//! guessed interval. `Pty::screen()` DRAINS the master, so every read is
//! accumulated rather than replacing what came before.

use std::time::{Duration, Instant};

use tests_pty::{signal_winch, Pty};

use crate::prompt_visibility_test::wait_for_child;
use crate::transcript_pager::{run_pager, AltScreenGuard, PagerState};

const CHILD_TEST: &str = "transcript_pager_pty_test::transcript_pager_child";

/// DECSET 1049 — enter the alternate screen.
const ALT_ENTER: &str = "\x1b[?1049h";
/// DECRST 1049 — leave it. The byte an operator's terminal needs back.
const ALT_LEAVE: &str = "\x1b[?1049l";

/// How long to wait for the child to reach the alternate screen / to exit.
/// Generous: this tier runs under parallel load on shared runners.
const REACH_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);

fn synthetic_turns(n: usize) -> Vec<newt_core::ConversationTurn> {
    (0..n)
        .map(|i| newt_core::ConversationTurn {
            user: format!("prompt {i}\nsecond line of {i}"),
            assistant: format!("reply {i}\nmore reply {i}\nand more"),
            events: vec![newt_core::ToolEvent::from_call(
                format!("tool{i}"),
                &serde_json::json!({"path": "x"}),
                true,
                Some(12),
            )],
            phantom_reaches: Vec::new(),
            sources: Vec::new(),
            tokens_in: None,
            tokens_out: None,
        })
        .collect()
}

/// The child half: takes the real terminal over, exactly as `/transcript`
/// does. `NEWT_TRANSCRIPT_PTY_CHILD` selects which lifecycle is exercised.
#[test]
#[ignore = "child process of the transcript-pager PTY acceptance test"]
fn transcript_pager_child() {
    let Some(mode) = std::env::var_os("NEWT_TRANSCRIPT_PTY_CHILD") else {
        return;
    };
    match mode.to_string_lossy().as_ref() {
        // The ordinary path: run the pager until the operator quits.
        "pager" => {
            let turns = synthetic_turns(40);
            let mut state = PagerState::new("pty transcript", &turns);
            let mut surface = crate::lean_input::LeanSurface::new(None).unwrap();
            run_pager(&mut state, &mut surface, false).expect("pager runs to a clean quit");
        }
        // The failure path AFTER entry: the guard is live and the process
        // unwinds. Restoration must not depend on reaching the end of
        // `run_pager` — it is a Drop obligation, and this proves it against a
        // real terminal rather than by inspection.
        "panic" => {
            let _guard = AltScreenGuard::enter(None).expect("enter the alternate screen");
            // HOLD the guard until the parent has sampled the tty. Panicking
            // immediately is a race the parent loses: the unwind restores
            // cooked mode before the first `is_raw()` read, and the test then
            // "passes" without ever having observed raw — which is exactly
            // what the `raw_during` guard refuses to accept. One blocking
            // byte-read makes the window deterministic.
            use std::io::Read as _;
            let mut byte = [0u8; 1];
            let _ = std::io::stdin().read(&mut byte);
            panic!("synthetic failure after pager entry (#1677 restoration proof)");
        }
        // #3016: the F4 fold list over one retained spill of ten lines.
        #[cfg(feature = "live-spill")]
        "folds" => {
            use std::os::fd::AsFd as _;
            let archive = crate::completed_spill::CompletedSpillArchive::default();
            let body = (0..10).map(|i| format!("body-row-{i}")).collect::<Vec<_>>();
            archive.retain(&body.join("\n"));
            let out: std::fs::File = std::io::stdout()
                .as_fd()
                .try_clone_to_owned()
                .expect("dup the slave")
                .into();
            crate::transcript_pager::run_spill_picker(&archive.snapshot(), out)
                .expect("fold list runs to a clean quit");
        }
        // #3023: F4 before anything is retained shows the empty notice.
        #[cfg(feature = "live-spill")]
        "empty-folds" => {
            use std::os::fd::AsFd as _;
            let archive = crate::completed_spill::CompletedSpillArchive::default();
            let out: std::fs::File = std::io::stdout()
                .as_fd()
                .try_clone_to_owned()
                .expect("dup the slave")
                .into();
            crate::transcript_pager::run_spill_picker(&archive.snapshot(), out)
                .expect("the empty fold list runs to a clean quit");
        }
        other => panic!("unknown child mode {other:?}"),
    }
}

/// Everything the parent side needs to judge one lifecycle.
struct Outcome {
    /// Every byte the terminal was shown, accumulated across drains.
    screen: String,
    /// Was the pty still raw immediately after the child exited?
    raw_after: bool,
    /// Was the pty raw while the pager was up? (The control: a test that
    /// never observed raw mode would "prove" restoration vacuously.)
    raw_during: bool,
    exited_cleanly: bool,
}

fn spawn_child(pty: &Pty, mode: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().expect("the test binary re-invokes itself"))
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env("NEWT_TRANSCRIPT_PTY_CHILD", mode)
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the pty child")
}

/// Drive one child lifecycle end to end.
///
/// `keys` are typed once the child has provably reached the alternate screen;
/// `resize` additionally shrinks then grows the pty (with an explicit
/// SIGWINCH, because a `slave_stdio()` child has no controlling terminal and
/// so gets no signal from `TIOCSWINSZ` alone).
fn drive(mode: &str, keys: &str, resize: bool) -> Outcome {
    let pty = Pty::open();
    let mut child = spawn_child(&pty, mode);

    let mut screen = String::new();
    let reached = {
        let deadline = Instant::now() + REACH_TIMEOUT;
        loop {
            screen.push_str(&pty.screen());
            if screen.contains(ALT_ENTER) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    assert!(
        reached,
        "child never entered the alternate screen; screen={screen:?}"
    );

    // Raw mode precedes the alt-screen byte we just saw, so this must hold.
    let raw_during = pty.is_raw();

    if resize {
        pty.resize(12, 60);
        signal_winch(child.id());
        std::thread::sleep(Duration::from_millis(120));
        screen.push_str(&pty.screen());
        pty.resize(60, 240);
        signal_winch(child.id());
        std::thread::sleep(Duration::from_millis(120));
        screen.push_str(&pty.screen());
    }

    if !keys.is_empty() {
        pty.type_in(keys);
    }
    // A pty's output buffer is bounded. Keep draining while the shared reaper
    // waits: otherwise the pager (or libtest's child summary after the pager
    // returns) can block in write(2), and the parent mistakes that backpressure
    // for a child lifecycle failure.
    //
    // Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 09:24 EDT | Date: 2026-08-17
    let status = std::thread::scope(|scope| {
        let waiter = scope.spawn(|| wait_for_child(&mut child, EXIT_TIMEOUT));
        while !waiter.is_finished() {
            screen.push_str(&pty.screen());
            std::thread::sleep(Duration::from_millis(20));
        }
        waiter.join().expect("child reaper thread")
    });
    // Drain whatever the exit path emitted (the restore sequence lands here).
    std::thread::sleep(Duration::from_millis(80));
    screen.push_str(&pty.screen());

    Outcome {
        screen,
        raw_after: pty.is_raw(),
        raw_during,
        exited_cleanly: status.is_some_and(|s| s.success()),
    }
}

fn assert_terminal_restored(what: &str, out: &Outcome) {
    assert!(
        out.raw_during,
        "{what}: the pty was never raw — the test proved nothing about restoration"
    );
    assert!(
        out.screen.contains(ALT_LEAVE),
        "{what}: the alternate screen was never left; screen={:?}",
        out.screen
    );
    assert!(
        !out.raw_after,
        "{what}: the terminal was left in RAW mode — an operator's shell would \
         be unusable (this is kernel state, not an escape-byte inference)"
    );
}

#[serial_test::serial(transcript_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn q_leaves_the_terminal_exactly_as_it_was_found() {
    let out = drive("pager", "q", false);
    assert!(out.exited_cleanly, "child failed; screen={:?}", out.screen);
    assert_terminal_restored("q", &out);
}

#[serial_test::serial(transcript_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn esc_leaves_the_terminal_exactly_as_it_was_found() {
    let out = drive("pager", "\x1b", false);
    assert!(out.exited_cleanly, "child failed; screen={:?}", out.screen);
    assert_terminal_restored("Esc", &out);
}

#[serial_test::serial(transcript_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn ctrl_c_leaves_the_terminal_exactly_as_it_was_found() {
    // In raw mode Ctrl-C is a KEY, not SIGINT — the pager handles it as quit.
    // If it ever reverted to signal delivery, restoration would ride the
    // default SIGINT disposition (process death, no unwind, no Drop) and this
    // assertion is what would catch it.
    let out = drive("pager", "\x03", false);
    assert!(out.exited_cleanly, "child failed; screen={:?}", out.screen);
    assert_terminal_restored("Ctrl-C", &out);
}

#[serial_test::serial(transcript_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_resize_then_quit_still_restores_the_terminal() {
    let out = drive("pager", "q", true);
    assert!(out.exited_cleanly, "child failed; screen={:?}", out.screen);
    assert_terminal_restored("resize then q", &out);
}

#[serial_test::serial(transcript_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_panic_after_entry_still_restores_the_terminal() {
    // The child dies non-zero on purpose; what must survive is the terminal.
    // The keystroke releases the child's blocking read so it panics only
    // AFTER the parent has confirmed the tty is raw.
    let out = drive("panic", "x", false);
    assert!(
        !out.exited_cleanly,
        "the panic child was supposed to fail; screen={:?}",
        out.screen
    );
    assert_terminal_restored("panic after entry", &out);
}

/// #3016: F4 is a fold list. Enter opens the selected spill; Space unfolds the selected item in place (the
/// rows past the preview appear on the same screen), and `q` still hands the
/// terminal back.
#[cfg(feature = "live-spill")]
#[serial_test::serial(transcript_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn space_unfolds_a_spill_in_place_in_the_f4_view() {
    let pty = Pty::open();
    let mut child = spawn_child(&pty, "folds");

    // ratatui moves the cursor between words, so match one unbroken token.
    let drew = pty.wait_for_screen("expand]", REACH_TIMEOUT);
    assert!(
        drew,
        "the folded preview never drew; screen={:?}",
        pty.screen()
    );
    assert!(
        !pty.wait_for_screen("body-row-9", Duration::from_millis(200)),
        "the last row must stay hidden while the item is folded"
    );
    pty.type_in(" ");
    assert!(
        pty.wait_for_screen("body-row-9", REACH_TIMEOUT),
        "Space did not unfold the item in place"
    );
    assert!(
        pty.wait_for_screen("\u{25be}", REACH_TIMEOUT),
        "no unfolded marker"
    );
    pty.type_in("q");
    let status = wait_for_child(&mut child, EXIT_TIMEOUT);
    assert!(
        status.is_some_and(|s| s.success()),
        "child did not quit cleanly"
    );
    assert!(!pty.is_raw(), "the terminal was left in raw mode");
}

/// #3023 review: PageDown on the empty F4 view panicked in `FoldList::scroll`.
#[cfg(feature = "live-spill")]
#[serial_test::serial(transcript_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn paging_the_empty_f4_view_does_not_panic() {
    let pty = Pty::open();
    let mut child = spawn_child(&pty, "empty-folds");
    assert!(
        pty.wait_for_screen("retained", REACH_TIMEOUT),
        "the empty notice never drew; screen={:?}",
        pty.screen()
    );
    pty.type_in("\x1b[6~");
    pty.type_in("\x1b[5~");
    pty.type_in("q");
    let status = wait_for_child(&mut child, EXIT_TIMEOUT);
    assert!(
        status.is_some_and(|s| s.success()),
        "the empty F4 view did not survive paging and quit cleanly"
    );
    assert!(!pty.is_raw(), "the terminal was left in raw mode");
}

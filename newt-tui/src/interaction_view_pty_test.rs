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

use tests_pty::{signal_winch, Pty};

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
        // **The real renderer, not a bare guard.** `present` owns the inline
        // viewport, polls events, redraws on resize, and answers on an
        // accelerator — so resize and narrow-width are exercised against the
        // production loop rather than against a fixture that merely holds
        // raw mode.
        "present" => {
            let interaction = fixture_interaction();
            let outcome =
                crate::interaction_view::present(&interaction).expect("the frame runs and returns");
            // Printed AFTER the guard has dropped and erased the region, so
            // the parent can distinguish "answered" from "still drawing".
            println!("OUTCOME:{outcome:?}");
        }
        other => panic!("unknown child mode {other:?}"),
    }
}

/// A permission-shaped interaction with labels long enough that a narrow
/// terminal must wrap them.
fn fixture_interaction() -> newt_core::interaction_surface::SurfaceInteraction {
    use newt_interaction::{
        ChoiceOption, Control, ControlId, ControlKind, InteractionDefinition, InteractionKind,
        OptionId, Requirement, SemanticRole,
    };
    let option = |id: &str, key: &str, label: &str| ChoiceOption {
        id: OptionId::new(id).expect("valid option id"),
        role: SemanticRole::Allow,
        label: label.to_string(),
        key: key.to_string(),
        aliases: Vec::new(),
    };
    let mut definition = InteractionDefinition::new(
        InteractionKind::Choice,
        "\u{2298} run_command wants to run `bash` \u{2014} outside the granted exec allowlist.",
        vec![Control {
            id: ControlId::new("decision").expect("valid control id"),
            kind: ControlKind::Choice {
                options: vec![
                    option("allow_once", "a", "allow once"),
                    option("deny", "d", "deny (default)"),
                ],
            },
            label: String::new(),
            requirement: Requirement::Required,
        }],
    );
    definition.note = Some("Esc=back \u{b7} Ctrl-C/Ctrl-D=exit".into());
    newt_core::interaction_surface::SurfaceInteraction::blocking(definition)
}

/// Answer a Device Status Report if the child asked for one.
///
/// `ratatui`'s `Viewport::Inline` asks the terminal where the cursor is
/// (`ESC[6n`) so it knows which rows it may own, and it waits for the reply.
/// A bare pty has no emulator behind it, so nothing answers and the frame
/// never initialises — which is precisely the class of thing only a real-PTY
/// test finds. The parent holds the master fd, so answering is its job.
/// `answered` counts the replies already sent FOR THIS SEGMENT, and each
/// segment's buffer starts empty — so it must be reset per segment. A
/// cumulative counter compared against a per-segment count silently stops
/// answering: the post-resize query then goes unanswered and the child dies
/// with "The cursor position could not be read within a normal duration",
/// which is how this was found.
fn answer_cursor_report(pty: &Pty, screen: &str, answered: &mut usize) {
    // EVERY query, not just the first: a resize makes the viewport re-ask,
    // and a latch that answered once left the frame waiting forever on the
    // second.
    let asked = screen.matches("\u{1b}[6n").count();
    while *answered < asked {
        // Row 1, column 1: the frame is entered at a known-clean row because
        // `suspend_for_prompt` has erased every ephemeral writer first.
        pty.type_in("\u{1b}[1;1R");
        *answered += 1;
    }
}

/// Lines libtest itself printed, which share the pty with the frame.
///
/// The child runs under `--nocapture` so its `OUTCOME:` marker reaches the
/// parent, and libtest's own progress lines come with it. A width assertion
/// that measured those would be testing libtest's formatting, not the frame's
/// — and would fail for a reason that has nothing to do with the property.
fn is_harness_noise(line: &str) -> bool {
    let t = line.trim();
    t.is_empty()
        || t.starts_with("running ")
        || t.starts_with("test ")
        || t.starts_with("test result:")
        || t.starts_with("failures")
        || t.starts_with("OUTCOME:")
        || t.starts_with("interaction_view_pty_test::")
        || t.starts_with("note: ")
        || t.starts_with("thread '")
}

/// The screen as a GRID, by applying cursor positioning rather than stripping it.
///
/// This exists because the first cut of the width assertion was wrong in a way
/// worth recording: `ratatui` does not emit padding, it MOVES the cursor
/// (`ESC[1;3H`) and prints a fragment. Stripping escapes therefore
/// concatenates every fragment onto one line — `⊘run_commandwantstorun…` —
/// and a width check over that measures nothing about the screen. Only by
/// honouring `CUP` does "no line exceeds the terminal width" become a claim
/// about what the operator sees.
///
/// Deliberately tiny: `CUP`, `\r`, `\n`, and printable text. Anything else is
/// consumed and ignored, which is sound here because the assertions are about
/// WHERE text lands, not how it is coloured.
fn screen_grid(screen: &str) -> Vec<String> {
    let mut grid: Vec<Vec<char>> = Vec::new();
    let (mut row, mut col) = (0usize, 0usize);
    let put = |grid: &mut Vec<Vec<char>>, row: usize, col: usize, ch: char| {
        while grid.len() <= row {
            grid.push(Vec::new());
        }
        let line = &mut grid[row];
        while line.len() <= col {
            line.push(' ');
        }
        line[col] = ch;
    };
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                row += 1;
                col = 0;
            }
            '\r' => col = 0,
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut final_byte = '\0';
                    for f in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&f) {
                            final_byte = f;
                            break;
                        }
                        params.push(f);
                    }
                    if final_byte == 'H' {
                        // CUP is 1-based, and an omitted parameter is 1.
                        let mut it = params.split(';');
                        let r: usize = it.next().unwrap_or("").parse().unwrap_or(1);
                        let c2: usize = it.next().unwrap_or("").parse().unwrap_or(1);
                        row = r.saturating_sub(1);
                        col = c2.saturating_sub(1);
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(f) = chars.next() {
                        if f == '\u{7}' || (f == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            printable if !printable.is_control() => {
                put(&mut grid, row, col, printable);
                col += 1;
            }
            _ => {}
        }
    }
    grid.into_iter()
        .map(|l| l.into_iter().collect::<String>().trim_end().to_string())
        .collect()
}

/// Visible text with CSI/OSC sequences removed, one entry per screen line.
///
/// Used only where POSITION does not matter (presence of a marker). Width
/// assertions use [`screen_grid`] instead — see its doc for why.
#[allow(dead_code)]
fn visible_lines(screen: &str) -> Vec<String> {
    let mut out = String::new();
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: parameters/intermediates, then a final byte in @..~
            Some('[') => {
                chars.next();
                for f in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&f) {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or ST
            Some(']') => {
                chars.next();
                while let Some(f) = chars.next() {
                    if f == '\u{7}' || (f == '\u{1b}' && chars.peek() == Some(&'\\')) {
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out.replace('\r', "\n")
        .lines()
        .map(str::to_string)
        .collect()
}

/// Spawn the child in `mode`, with fd 0/1 on `pty`.
///
/// **The ONE spawn site in this file, deliberately.** Three drivers need a
/// child (a simple lifecycle, a nested one, and the real `present` loop) and
/// each began as its own spawn call. That is three copies of one trust
/// decision, and `docs/security/spawn-inventory.toml` counts occurrences per
/// file — so it would have been three things to justify and three places for
/// them to drift apart. One helper keeps the inventory entry at a count of
/// one and makes the justification cover exactly one shape:
///
/// (Note for whoever edits this doc: `scripts/spawn_inventory.py` matches its
/// needle over raw file text without stripping comments, so writing the
/// spawn-constructor's name in prose here counts as a second site and fails
/// the gate. Bumping the allowlist to accommodate a comment would be the
/// wrong fix — it would reserve room for a real spawn nobody reviewed.)
///
/// re-execute THIS test binary (`std::env::current_exe`) with an `--exact`
/// filter so the child runs one ignored `#[test]` with fd 0/1 on a pty. Fixed
/// argv, no shell, no attacker- or model-controlled input; the child is the
/// binary cargo just built, so it cannot be stale or foreign.
fn spawn_child(pty: &Pty, mode: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().expect("the test binary re-invokes itself"))
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env("NEWT_INTERACTION_PTY_CHILD", mode)
        .env("TERM", "xterm-256color")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the pty child")
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
    let mut child = spawn_child(&pty, mode);

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
    let mut child = spawn_child(&pty, "nested");

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

/// One `present` lifecycle, captured in PHASES.
///
/// `Pty::screen()` DRAINS the master, so capturing per phase gives the output
/// produced *in that phase* rather than a running transcript. That matters:
/// the grid has no model of erase sequences, so a cumulative capture would
/// still hold the pre-resize frame drawn at the old width and a
/// "nothing exceeds the new width" assertion would fail on history rather
/// than on what the operator can see.
struct Frames {
    /// The first frame, before any resize and before the answer key.
    first: String,
    /// Output produced after the resize (empty when there was none).
    after_resize: String,
    /// Everything from the answer key onwards, including the child's marker.
    tail: String,
    raw_during: bool,
    raw_after: bool,
}

/// Drain for `ticks` x 20ms, answering every cursor-report query that appears
/// in THIS segment.
fn collect(pty: &Pty, ticks: usize) -> String {
    let mut out = String::new();
    let mut answered = 0usize;
    for _ in 0..ticks {
        out.push_str(&pty.screen());
        answer_cursor_report(pty, &out, &mut answered);
        std::thread::sleep(Duration::from_millis(20));
    }
    out
}

/// Drive the real `present` frame on a pty of the given size, optionally
/// resizing once the frame is up, then answer with `key`.
fn drive_present(rows: u16, cols: u16, resize_to: Option<(u16, u16)>, key: &str) -> Frames {
    let pty = Pty::open();
    pty.resize(rows, cols);
    let mut child = spawn_child(&pty, "present");

    let mut first = String::new();
    let mut answered = 0usize;
    let raw_during = {
        let deadline = Instant::now() + REACH_TIMEOUT;
        loop {
            first.push_str(&pty.screen());
            answer_cursor_report(&pty, &first, &mut answered);
            if pty.is_raw() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // Let the first frame land. `collect` owns its own reply counter, and
    // `first` keeps accumulating, so hand it the running one for this segment.
    for _ in 0..30 {
        first.push_str(&pty.screen());
        answer_cursor_report(&pty, &first, &mut answered);
        std::thread::sleep(Duration::from_millis(20));
    }

    let after_resize = match resize_to {
        Some((r, c)) => {
            pty.resize(r, c);
            // A `slave_stdio()` child has no controlling terminal, so
            // TIOCSWINSZ alone delivers no signal — the explicit SIGWINCH is
            // what makes the resize observable rather than merely configured.
            signal_winch(child.id());
            // A FRESH segment, so a fresh reply counter — the viewport
            // re-queries the cursor after a resize and will hang without it.
            collect(&pty, 35)
        }
        None => String::new(),
    };

    pty.type_in(key);
    let mut tail = String::new();
    let _ = std::thread::scope(|scope| {
        let waiter = scope.spawn(|| wait_for_child(&mut child, EXIT_TIMEOUT));
        while !waiter.is_finished() {
            tail.push_str(&pty.screen());
            std::thread::sleep(Duration::from_millis(20));
        }
        waiter.join().expect("child reaper thread")
    });
    std::thread::sleep(Duration::from_millis(120));
    tail.push_str(&pty.screen());

    Frames {
        first,
        after_resize,
        tail,
        raw_during,
        raw_after: pty.is_raw(),
    }
}

/// Frame lines wider than `width`, with libtest's own output excluded.
fn overflowing(segment: &str, width: usize) -> Vec<String> {
    screen_grid(segment)
        .into_iter()
        .filter(|l| !is_harness_noise(l) && l.chars().count() > width)
        .collect()
}

/// **Resize: the frame reflows to the new width and the terminal is still
/// handed back.**
///
/// Two different failures live here, so both are asserted: a frame that
/// ignores SIGWINCH keeps drawing at the old width and corrupts the display,
/// and a frame that handles it but loses its guard leaves the terminal raw.
/// Passing one says nothing about the other.
///
/// The width claim is made on the output produced AFTER the resize, because
/// that is the only segment that describes the resized terminal.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_resize_reflows_the_frame_and_still_restores_the_terminal() {
    const NARROW: usize = 46;
    let f = drive_present(24, 100, Some((12, NARROW as u16)), "d");
    assert!(
        f.raw_during,
        "the pty was never raw — nothing was exercised"
    );
    assert!(!f.raw_after, "the terminal was left RAW after a resize");
    // The frame survived the resize rather than wedging: a frame that died on
    // SIGWINCH would never read the key.
    assert!(
        f.tail.contains("OUTCOME:"),
        "the frame did not answer after the resize; tail={:?}",
        f.tail
    );
    assert!(
        !f.after_resize.trim().is_empty(),
        "the frame drew nothing after SIGWINCH — it did not notice the resize"
    );
    let over = overflowing(&f.after_resize, NARROW);
    assert!(
        over.is_empty(),
        "after resizing to {NARROW} columns the frame still drew wider: {over:#?}"
    );
}

/// **Narrow width: content degrades legibly rather than corrupting the frame.**
///
/// Asserted on the GRID — cursor positioning applied — because `ratatui`
/// positions text rather than padding it, so a check over stripped escapes
/// would concatenate fragments and measure nothing.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_narrow_terminal_wraps_rather_than_overflowing() {
    const COLS: usize = 28;
    let f = drive_present(20, COLS as u16, None, "d");
    assert!(
        f.raw_during,
        "the pty was never raw — nothing was exercised"
    );
    assert!(!f.raw_after, "the terminal was left RAW");

    let over = overflowing(&f.first, COLS);
    assert!(
        over.is_empty(),
        "the frame overflowed a {COLS}-column terminal: {over:#?}"
    );
    // Degrading LEGIBLY means the content is still there, wrapped — not
    // truncated away. Without this, a frame that drew nothing at all would
    // satisfy "nothing overflows".
    let flat = screen_grid(&f.first).join(" ");
    assert!(
        flat.contains("run_command") || flat.contains("bash"),
        "the body vanished at {COLS} columns rather than wrapping: {flat:?}"
    );
    assert!(f.tail.contains("OUTCOME:"), "the frame did not answer");
}

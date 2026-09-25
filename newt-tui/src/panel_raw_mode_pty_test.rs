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

use tests_pty::{screen_grid, signal_winch, Pty};

use crate::panel::PanelRawGuard;
use crate::prompt_visibility_test::wait_for_child;

const CHILD_TEST: &str = "panel_raw_mode_pty_test::panel_raw_mode_child";

/// Generous: this tier runs under parallel load on shared runners.
const REACH_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);

/// #1950 fixture: the panel's inline height, and what it draws.
const PANEL_TEST_HEIGHT: u16 = 6;
const PANEL_SENTINEL: &str = "PANEL-DREW-1950";
const PANEL_SENTINEL_OK: &str = "PANEL-OPENED-1950";

/// #1977 fixture. The BODY of the panel, distinct from its border: the
/// operator's report was a visible top border with no rows under it.
const PANEL_BODY_SENTINEL: &str = "PANEL-BODY-1977";
/// The competing bottom-pinned viewport, standing in for the rich prompt.
const PROMPT_SENTINEL: &str = "PROMPT-ROWS-1977";

/// #2573 hardening fixture: the rows a competing holder keeps, and what it
/// paints in them.
const HOLDER_ROWS: u16 = 3;
const HOLDER_SENTINEL: &str = "HOLDER-ROW";

/// A bordered panel, `Esc` to close: the smallest `Screen` that draws a
/// height the parent can count from its rounded corners.
struct HeldPanel;

impl crate::panel::Screen for HeldPanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, BorderType, Paragraph};
        let body = (0..30)
            .map(|n| format!("panel-row-{n:02}"))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(body.join("\n")).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title("held"),
            ),
            frame.area(),
        );
    }

    fn key(&mut self, key: crate::panel::Key) -> crate::panel::Flow {
        match key {
            crate::panel::Key::Esc => crate::panel::Flow::Close(false),
            _ => crate::panel::Flow::Stay,
        }
    }
}

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
        // #1950: THE REPORTED FAILURE. A competing consumer of the same tty
        // while the panel opens. `Viewport::Inline` makes ratatui ask the
        // terminal where the cursor is (`ESC[6n`, answered on the INPUT
        // stream) — and the reader below takes the answer first, so the panel
        // used to fail to open at all with "The cursor position could not be
        // read within a normal duration".
        //
        // The parent DOES answer the query. That is the point: the terminal is
        // cooperative and the panel must still open, so a pass cannot be
        // explained by the query never having been asked.
        "inline_contended" => {
            let _guard = PanelRawGuard::enter().expect("enter raw mode");
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let reader = {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = crossterm::event::poll(Duration::from_millis(50));
                        let _ = crossterm::event::read();
                    }
                })
            };
            // Let the reader reach its blocking read before the panel asks.
            std::thread::sleep(Duration::from_millis(150));

            match crate::panel::make_terminal(PANEL_TEST_HEIGHT) {
                Ok(mut terminal) => {
                    // RENDERS, not merely constructs. A terminal that was
                    // built and then drew nothing would leave the operator
                    // looking at the same blank screen the bug produced.
                    terminal
                        .draw(|f| {
                            f.render_widget(
                                ratatui::widgets::Paragraph::new(PANEL_SENTINEL),
                                f.area(),
                            );
                        })
                        .expect("the panel draws");
                    println!("{PANEL_SENTINEL_OK}");
                }
                Err(e) => println!("PANEL-FAILED {e}"),
            }
            use std::io::Write as _;
            std::io::stdout().flush().ok();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            // Deliberately NOT `hold()`, and deliberately not joined. The
            // reader is blocked in `event::read`, so joining would wait for a
            // keystroke — and it would EAT the byte the parent sends to
            // release a hold, which is exactly how the first draft of this
            // fixture hung for 60s and reported the terminal left raw. The
            // parent has already sampled `is_raw` by now: the child took the
            // guard, slept 150ms, and spent a DSR round trip before drawing.
            // Falling off the end here drops the guard and restores the tty,
            // which is the property the rest of this file exists to hold.
            drop(reader);
        }
        // #1977: TWO bottom-anchored inline viewports, which is the operator's
        // live report. The competing reader eats the DSR reply, so BOTH take
        // the anchored fallback and land on the same rows — the environment
        // the defect was found in, made rather than mocked.
        "inline_overpainted" => {
            let _guard = PanelRawGuard::enter().expect("enter raw mode");
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let reader = {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = crossterm::event::poll(Duration::from_millis(50));
                        let _ = crossterm::event::read();
                    }
                })
            };
            std::thread::sleep(Duration::from_millis(150));

            // The PROMPT viewport, as `rich_input` owns it. Same height as
            // the panel and the same bottom anchor, so the two cover the SAME
            // rows — which is #1977's geometry. A shorter prompt overlaps only
            // part of the panel and the assertion below could pass on a row
            // that was never contested.
            let prompt_lease = crate::inline_viewport::lease_bottom_rows(
                PANEL_TEST_HEIGHT,
                newt_core::tty::OnCollision::Refuse,
            )
            .expect("the prompt takes the bottom rows first");
            let mut prompt = crate::inline_viewport::inline_terminal(prompt_lease)
                .expect("prompt viewport opens");
            // The content must CHANGE on each paint. ratatui diffs cells and
            // emits only what differs, so a prompt repainting identical text
            // writes nothing at all — the first cut of this fixture painted
            // six times and produced one write, contending with nothing.
            // EVERY row changes on every paint. ratatui diffs cells, so a
            // prompt repainting near-identical text rewrites only the columns
            // that differ — the first cut of this fixture painted six times
            // and emitted one changed digit, contending with nothing.
            let paint_prompt = |t: &mut crate::inline_viewport::InlineTerm, tick: usize| {
                let filled = (0..PANEL_TEST_HEIGHT)
                    .map(|r| format!("{PROMPT_SENTINEL} r{r} t{tick}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                // `clear` before drawing, because ratatui diffs against its OWN
                // buffer and this viewport's belief about the screen is stale
                // the moment another writer touches its rows. That staleness is
                // the #1977 mechanism itself; a real surface reclaims its rows
                // on any full redraw (a resize, a mode change), and this is that
                // redraw made deterministic.
                t.clear().ok();
                t.draw(|f| {
                    f.render_widget(ratatui::widgets::Paragraph::new(filled), f.area());
                })
                .ok();
            };
            paint_prompt(&mut prompt, 0);

            match crate::panel::make_terminal(PANEL_TEST_HEIGHT) {
                Ok(mut terminal) => {
                    // The panel fills EVERY row with its body marker, so the
                    // assertion cannot pass on a row the prompt happens not to
                    // cover.
                    let body = (0..PANEL_TEST_HEIGHT)
                        .map(|r| format!("{PANEL_BODY_SENTINEL} r{r}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    terminal
                        .draw(|f| {
                            f.render_widget(ratatui::widgets::Paragraph::new(body), f.area());
                        })
                        .expect("the panel draws");
                    println!("{PANEL_SENTINEL_OK}");
                    // The prompt keeps repainting while the panel is open —
                    // which is what the rich input does, and what makes the
                    // panel body invisible rather than merely misplaced.
                    for tick in 1..=5 {
                        paint_prompt(&mut prompt, tick);
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                Err(e) => println!("PANEL-FAILED {e}"),
            }
            use std::io::Write as _;
            std::io::stdout().flush().ok();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            drop(reader);
        }

        // #2573 hardening: `panel::drive(.., None)` — the path with no cockpit
        // to lend rows — while another component holds the bottom 3 rows.
        // `NEWT_PANEL_HELD_MOVE` makes the holder follow a terminal resize to
        // the new bottom, as the rich prompt does.
        "panel_held" => {
            use std::io::Write as _;
            use std::sync::{Arc, Mutex};
            let holder = crate::inline_viewport::lease_bottom_rows(
                HOLDER_ROWS,
                newt_core::tty::OnCollision::Refuse,
            )
            .expect("the holder takes the bottom rows first");
            let paint = |top: u16| {
                let mut out = std::io::stdout();
                for r in 0..HOLDER_ROWS {
                    let _ = write!(out, "\x1b[{};1H{HOLDER_SENTINEL}-{r}", top + r + 1);
                }
                let _ = out.flush();
            };
            let (_, rows) = crossterm::terminal::size().expect("size");
            paint(rows - HOLDER_ROWS);
            let holder = Arc::new(Mutex::new(holder));
            if std::env::var_os("NEWT_PANEL_HELD_MOVE").is_some() {
                let holder = holder.clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(10));
                    let (_, now) = crossterm::terminal::size().expect("size");
                    if now == rows {
                        continue;
                    }
                    let top = now.saturating_sub(HOLDER_ROWS);
                    let moved = holder.lock().unwrap().relocate(
                        newt_core::tty::Region::Rows {
                            top,
                            height: HOLDER_ROWS,
                        },
                        newt_core::tty::OnCollision::Refuse,
                    );
                    paint(top);
                    println!("HOLDER_MOVED:{moved}");
                    return;
                });
            }
            println!("HOLDER_READY");
            let result = crate::panel::drive(&mut HeldPanel, PANEL_TEST_HEIGHT, None);
            // Say how `drive` ended, whichever way it did.
            println!(
                "DRIVE_RESULT:{:?}",
                result.as_ref().map_err(ToString::to_string)
            );
            println!("HOLDER_STILL_HOLDS:{:?}", holder.lock().unwrap().region());
            hold();
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
    /// Did the child actually ASK where the cursor was, and did the parent
    /// answer? #1950's control: a panel that opened because nothing ever
    /// queried the terminal would prove nothing about a contended query.
    answered_cursor_query: bool,
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
    let mut answered_cursor_query = false;
    let raw_during = {
        let deadline = Instant::now() + REACH_TIMEOUT;
        loop {
            screen.push_str(&pty.screen());
            // Play the terminal: answer DSR (`ESC[6n`) with a cursor report,
            // the way a real emulator does. A bare pty answers nothing on its
            // own, so without this every inline surface would time out here
            // for a reason that has nothing to do with what is under test.
            if !answered_cursor_query && screen.contains("\u{1b}[6n") {
                pty.type_in("\u{1b}[3;1R");
                answered_cursor_query = true;
            }
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
            if !answered_cursor_query && screen.contains("\u{1b}[6n") {
                pty.type_in("\u{1b}[3;1R");
                answered_cursor_query = true;
            }
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
        answered_cursor_query,
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

/// **#1950: the panel opens even when another consumer takes the cursor
/// reply.**
///
/// `Viewport::Inline` makes ratatui ask the terminal where the cursor is, and
/// the reply comes back on the *input* stream — so any other reader of that
/// stream can take it first. The operator hit this on `/backends` inside a
/// multiplexer; newt's own interrupt watcher is another candidate. Before the
/// fix, `Terminal::with_options` returned
/// `"The cursor position could not be read within a normal duration"` and the
/// panel did not open at all.
///
/// **This test must fail against `17a89c91`.** It was written by making the
/// failure — a real competing reader on a real pty — rather than by asserting
/// on a mocked error, because a mocked error proves only that the rescue
/// branch compiles.
///
/// It was ALSO measured against the pre-#1924 shape: raw mode taken through
/// `crossterm::terminal::enable_raw_mode` (so crossterm's own raw flag is
/// correctly set) fails here identically. The cursor query is not a raw-mode
/// problem, and syncing that flag fixes nothing.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_panel_still_opens_when_the_cursor_reply_is_taken_by_another_reader() {
    let out = drive("inline_contended");

    // CONTROL 1: the query really was asked and really was answered. Without
    // this, a pass could mean the inline viewport never queried at all.
    assert!(
        out.answered_cursor_query,
        "the child never emitted ESC[6n, so nothing was contended and this \
         test proved nothing; screen={:?}",
        out.screen
    );
    // CONTROL 2: the guard was actually engaged, as the other tests require.
    assert!(
        out.raw_during,
        "the pty was never raw; screen={:?}",
        out.screen
    );

    assert!(
        !out.screen.contains("PANEL-FAILED"),
        "the panel refused to open — this is #1950; screen={:?}",
        out.screen
    );
    assert!(
        out.screen.contains(PANEL_SENTINEL_OK) && out.screen.contains(PANEL_SENTINEL),
        "the panel did not DRAW. Opening without drawing leaves the operator \
         the same blank screen the bug produced; screen={:?}",
        out.screen
    );
    // And it still hands the terminal back, which is what the rest of this
    // file exists to hold.
    assert!(
        !out.raw_after,
        "the terminal was left RAW; screen={:?}",
        out.screen
    );
}

/// **#1977, red-first.** Two bottom-anchored inline viewports own the same
/// rows and neither knows about the other, so the prompt's repaints erase the
/// panel's body. The operator saw a top border and nothing under it, and the
/// content only reached the screen when Esc's teardown flushed it to
/// scrollback.
///
/// **This asserts on the GRID, not the byte stream, and that distinction is
/// the whole test.** `drive` accumulates every byte the child emits, and the
/// panel's body IS emitted — that is why it appeared in scrollback. A
/// `screen.contains(BODY)` assertion would therefore pass against the very
/// defect it claims to catch. Only replaying the cursor motions and asking
/// what is left on the screen can tell "drawn" from "drawn then overpainted".
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_panel_body_survives_a_competing_bottom_anchored_viewport() {
    let out = drive("inline_overpainted");
    let grid = screen_grid(&out.screen);

    // CONTROL 1: the panel opened at all. Without this a "body missing"
    // failure could just be #1950 regressing.
    assert!(
        out.screen.contains(PANEL_SENTINEL_OK),
        "the panel never opened, so this says nothing about overpainting; \
         screen={:?}",
        out.screen
    );
    // CONTROL 2: the competing viewport really painted. If the prompt never
    // reached the screen there was no contention and a pass is meaningless.
    assert!(
        grid.iter().any(|line| line.contains(PROMPT_SENTINEL)),
        "the competing prompt never painted, so nothing contended; grid={grid:#?}"
    );
    // CONTROL 3: the body was emitted. This separates "never drawn" from
    // "drawn and then overpainted" — the second is #1977, the first is not.
    assert!(
        out.screen.contains(PANEL_BODY_SENTINEL),
        "the panel never drew its body; that is a different bug; screen={:?}",
        out.screen
    );

    // THE PROPERTY: emitted is not enough. It has to still be on the screen.
    assert!(
        grid.iter().any(|line| line.contains(PANEL_BODY_SENTINEL)),
        "the panel body was overpainted by the prompt viewport — #1977. It \
         reached the terminal (control 3) and is gone from the screen; \
         grid={grid:#?}"
    );
}

/// Drive `panel::drive(.., None)` beside a holder of the bottom 3 rows. The
/// panel starts at 6 rows on a 24-row terminal. Ctrl-Z asks for the whole
/// screen and Shift-Up for one more row; then, if `shrink_to` is set, the
/// terminal shrinks to that many rows (and the panel must then be the given
/// height) (`holder_follows`: the holder moves to
/// the new bottom first, as the rich prompt does) and Esc closes.
fn drive_held(shrink_to: Option<(u16, usize)>, holder_follows: bool) -> Held {
    let pty = Pty::open();
    pty.resize(24, 80);
    let baseline = pty.termios_snapshot();
    let mut command = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    );
    command
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env("NEWT_PANEL_PTY_CHILD", "panel_held")
        .env("TERM", "xterm-256color")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(std::process::Stdio::null());
    if holder_follows {
        command.env("NEWT_PANEL_HELD_MOVE", "1");
    }
    let mut child = command.spawn().expect("spawn the pty child");
    // The command still owns dups of the slave: drop them, or EOF never comes.
    drop(command);
    let mut transcript = String::new();
    let panel_height = |repaint: &str| {
        let rows = screen_grid(repaint);
        let top = rows.iter().position(|row| row.contains('╭'));
        let bottom = rows.iter().rposition(|row| row.contains('╰'));
        top.zip(bottom).map(|(top, bottom)| bottom - top + 1)
    };
    let result = (|| -> Result<(), String> {
        let mut step = |what: &str, act: &dyn Fn(), want: usize| -> Result<(), String> {
            transcript.push_str(&pty.screen());
            act();
            if !pty.wait_for_screen("╯", REACH_TIMEOUT) {
                return Err(format!("no repaint after: {what}"));
            }
            let repaint = pty.screen();
            transcript.push_str(&repaint);
            match panel_height(&repaint) {
                Some(height) if height == want => Ok(()),
                other => Err(format!(
                    "{what}: height {other:?}, want {want}: {repaint:?}"
                )),
            }
        };
        if !pty.wait_for_screen_after("HOLDER_READY", "╯", REACH_TIMEOUT) {
            return Err("the panel never painted".into());
        }
        // The DSR is unanswered on purpose: the panel anchors on its lease.
        step(
            "Ctrl-Z is refused, the panel keeps its rows",
            &|| pty.type_in("\x1a"),
            6,
        )?;
        step("Shift-Up grows one row", &|| pty.type_in("\x1b[1;2A"), 7)?;
        if let Some((rows, panel_rows)) = shrink_to {
            step(
                "the terminal shrinks",
                &|| {
                    pty.resize(rows, 80);
                    if holder_follows {
                        // The holder moves first; only then does the panel hear.
                        assert!(pty.wait_for_screen("HOLDER_MOVED:true", REACH_TIMEOUT));
                    }
                    signal_winch(child.id());
                },
                panel_rows,
            )?;
        }
        transcript.push_str(&pty.screen());
        pty.type_in("\x1b");
        if !pty.wait_for_screen("HOLDER_STILL_HOLDS", REACH_TIMEOUT) {
            return Err("Esc did not close the panel".into());
        }
        Ok(())
    })();
    // `drive` has returned by now, one way or another; the child says how.
    let returned = pty.wait_for_screen("HOLDER_STILL_HOLDS", Duration::from_secs(10));
    let termios_restored = returned && pty.termios_snapshot() == baseline;
    transcript.push_str(&pty.screen());
    pty.type_in("\n");
    if result.is_err() {
        let _ = child.kill();
    }
    let status = wait_for_child(&mut child, EXIT_TIMEOUT);
    transcript.push_str(&pty.screen_when_finished(status.is_some()));
    Held {
        result,
        stream: transcript,
        termios_restored,
        exit_ok: status.is_some_and(|status| status.success()),
    }
}

/// What `drive_held` saw.
struct Held {
    /// The steps the parent drove: every height, and Esc closing the panel.
    result: Result<(), String>,
    stream: String,
    /// Kernel termios equal to the baseline once `drive` returned.
    termios_restored: bool,
    exit_ok: bool,
}

/// #2573 hardening, the no-cockpit path: with the bottom 3 rows held (a
/// `Refuse` lease), Ctrl-Z cannot take the screen and Shift-Up grows only
/// into free rows. The panel stays open at what it was granted, and Esc closes
/// with `drive` returning `Ok` and termios restored.
///
/// NOT asserted: that the holder's rows are untouched. They are not — the
/// panel's `clear` erases from its own top row to the bottom of the screen,
/// holder included (see RESULT.md); pinning that as a passing test would
/// bless it.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_panel_beside_a_row_holder_stays_open_at_its_granted_height() {
    let held = drive_held(None, false);
    assert!(held.result.is_ok(), "{:?}: {:?}", held.result, held.stream);
    assert!(
        held.stream.contains("DRIVE_RESULT:Ok(false)"),
        "drive did not return Ok: {:?}",
        held.stream
    );
    assert!(
        held.termios_restored,
        "termios not restored: {:?}",
        held.stream
    );
    assert!(held.exit_ok, "{:?}", held.stream);
}

/// The same beside a holder that follows a terminal shrink 24 → 10 rows, as
/// the rich prompt does. The panel (7 rows after the grow) fits exactly above
/// the holder's new rows, so it stays open at 7 and closes `Ok`.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_panel_beside_a_row_holder_survives_a_terminal_shrink_that_still_fits() {
    let held = drive_held(Some((10, 7)), true);
    assert!(held.result.is_ok(), "{:?}: {:?}", held.result, held.stream);
    assert!(
        held.stream.contains("HOLDER_MOVED:true"),
        "{:?}",
        held.stream
    );
    assert!(
        held.stream.contains("DRIVE_RESULT:Ok(false)"),
        "drive did not return Ok: {:?}",
        held.stream
    );
    assert!(
        held.termios_restored,
        "termios not restored: {:?}",
        held.stream
    );
    assert!(held.exit_ok, "{:?}", held.stream);
}

/// A shrink that leaves fewer free rows above the holder than the panel had
/// (24 → 8, holder on rows 5..7, so 5 free) degrades the panel to what fits.
/// Before, `relet` found neither 7 rows nor the granted 7 and `drive` returned
/// `Err("another surface already owns these rows")`, closing the panel.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_shrink_that_leaves_no_room_degrades_the_panel_instead_of_closing_it() {
    let held = drive_held(Some((8, 5)), true);
    assert!(held.result.is_ok(), "{:?}: {:?}", held.result, held.stream);
    assert!(
        held.stream.contains("DRIVE_RESULT:Ok(false)"),
        "drive did not return Ok: {:?}",
        held.stream
    );
    assert!(
        held.termios_restored,
        "termios not restored: {:?}",
        held.stream
    );
    assert!(held.exit_ok, "{:?}", held.stream);
}

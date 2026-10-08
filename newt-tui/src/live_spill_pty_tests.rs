//! Real terminal witness for the rich model's existing input and erase owner,
//! and for the arbiter's placement of the live frame under the transcript.

use super::{LiveSpillRenderer, ScreenModel};
use crate::interaction_view_pty_test::erasing_screen;
use crate::panel_raw_mode_pty_test::Pump;
use crate::prompt_visibility_test::wait_for_child;
use newt_core::agentic::{CompletedSpillRenderer, FileChangePresentation};
use newt_core::{LiveToolOutput, ToolOutputStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tests_pty::Pty;

const CHILD: &str = "live_spill::tests::terminal::rich_file_change_child";
const LIVE_CHILD: &str = "live_spill::tests::terminal::live_frame_child";
const REACH: Duration = Duration::from_secs(30);

/// Keep the session leader alive while the parent verifies the terminal:
/// macOS revokes the controlling PTY when its session leader exits.
fn hold_for_parent() {
    let mut ready = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut ready, 1, REACH.as_millis() as i32) },
        1
    );
    let mut release = String::new();
    std::io::stdin().read_line(&mut release).unwrap();
    assert_eq!(release, "\n");
}

/// The one place this file re-runs the test binary as a child (the spawn
/// inventory counts sites per file): one ignored child test on the pty,
/// selected by `marker`.
fn spawn_child(pty: &Pty, child: &str, marker: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", child, "--ignored", "--nocapture"])
        .env(marker, "1")
        .env("TERM", "xterm-256color")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(pty.slave_stdio())
        .spawn()
        .unwrap()
}

#[test]
#[ignore = "child of the rich file-change terminal regression"]
fn rich_file_change_child() {
    if std::env::var_os("NEWT_RICH_FILE_CHANGE_CHILD").is_none() {
        return;
    }
    // Crossterm measures /dev/tty first; this disposable child must own the
    // same terminal that the parent resizes through the fixture's master.
    assert!(unsafe { libc::setsid() } >= 0);
    assert_eq!(unsafe { libc::ioctl(0, libc::TIOCSCTTY as _, 0) }, 0);
    let before = "old\u{1b}]52;c;ignored\u{7}\n".repeat(12);
    let after = format!("{}last_visible\n", "new source\n".repeat(11));
    let unified = format!(
        "--- state.txt\n+++ state.txt\n@@ -1,12 +1,12 @@\n{}{}",
        before
            .lines()
            .map(|line| format!("-{line}\n"))
            .collect::<String>(),
        after
            .lines()
            .map(|line| format!("+{line}\n"))
            .collect::<String>()
    );
    let changes = newtui::diff::from_unified(&unified).unwrap();
    let raw = changes.to_markdown();
    let safe = newt_core::notes_scan::neutralize_for_display(&raw).into_owned();
    let change = Arc::new(FileChangePresentation::new(
        changes,
        "state.txt".into(),
        Some(before),
        Some(after),
        raw.clone(),
        0..raw.len(),
    ));
    let renderer = LiveSpillRenderer::stdout(
        4,
        true,
        Arc::new(crate::completed_spill::CompletedSpillArchive::default()),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    crate::with_live_spill_watch(true, &cancel, false, Some(renderer.as_ref()), || {
        println!("receipt-committed");
        assert!(renderer.render_file_change(&raw, &safe, change, 80, 4) > 0);
        let deadline = Instant::now() + REACH;
        while !cancel.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            cancel.load(Ordering::Relaxed),
            "the existing turn watcher must receive the interrupt"
        );
        renderer.erase();
    });
    println!("receipt-terminal-done");
    hold_for_parent();
}

/// A live frame taller than the rows under the transcript: 18 transcript
/// lines and a header, then a frame of 8 visible rows that scrolls the
/// transcript by the deficit, finishes, and is followed by one committed
/// line. The parent reads where that line landed.
#[test]
#[ignore = "child of the live frame placement regression"]
fn live_frame_child() {
    if std::env::var_os("NEWT_LIVE_FRAME_CHILD").is_none() {
        return;
    }
    assert!(unsafe { libc::setsid() } >= 0);
    assert_eq!(unsafe { libc::ioctl(0, libc::TIOCSCTTY as _, 0) }, 0);
    use std::io::Write as _;
    let mut out = std::io::stdout();
    // A known screen: libtest's own progress line shares this pty.
    write!(out, "\x1b[2J\x1b[H").unwrap();
    for n in 0..18 {
        writeln!(out, "TRANSCRIPT-{n:02}").unwrap();
    }
    writeln!(out, "HEADER-LINE").unwrap();
    out.flush().unwrap();
    let renderer = LiveSpillRenderer::stdout(
        8,
        true,
        Arc::new(crate::completed_spill::CompletedSpillArchive::default()),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    crate::with_live_spill_watch(true, &cancel, false, Some(renderer.as_ref()), || {
        renderer.start(1);
        for n in 0..40 {
            renderer.write(
                1,
                ToolOutputStream::Stdout,
                format!("spill-{n:02}\n").as_bytes(),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
        renderer.finish(1);
    });
    println!("RESULT-LINE");
    hold_for_parent();
}

/// The parent's side of the terminal for the file-change test: a reflowing
/// screen model fed every byte the child writes, answering the child's
/// cursor queries from that model the way a terminal would, so the frame's
/// placement and the model agree.
struct Live<'a> {
    pty: &'a Pty,
    grid: ScreenModel,
    transcript: String,
    /// The tail of the last chunk that may be the start of a cursor query
    /// split across two reads; applied once the rest arrives.
    pending: String,
}

impl Live<'_> {
    fn drain(&mut self) {
        const QUERY: &str = "\x1b[6n";
        let chunk = self.pty.screen();
        self.transcript.push_str(&chunk);
        let joined = std::mem::take(&mut self.pending) + &chunk;
        let mut rest = joined.as_str();
        while let Some(at) = rest.find(QUERY) {
            self.grid.apply(&rest.as_bytes()[..at]);
            self.pty.type_in(&format!(
                "\x1b[{};{}R",
                self.grid.cursor_row + 1,
                self.grid.cursor_col + 1
            ));
            rest = &rest[at + QUERY.len()..];
        }
        // Hold back a tail that could be the start of a split query.
        let keep = (1..QUERY.len())
            .rev()
            .find(|&n| rest.ends_with(&QUERY[..n]))
            .unwrap_or(0);
        let (apply, tail) = rest.split_at(rest.len() - keep);
        self.grid.apply(apply.as_bytes());
        self.pending = tail.to_string();
    }
}

fn reach(live: &mut Live<'_>, needle: &str) {
    reach_since(live, needle, 0, false);
}

/// Like [`reach`], but the needle must also appear in bytes the child wrote
/// after `since`. After a resize the model reflows rows the child painted at
/// the OLD width, so a grid match alone can be satisfied without the child
/// ever observing the new geometry.
fn reach_since(live: &mut Live<'_>, needle: &str, since: usize, complete_line: bool) {
    let deadline = Instant::now() + REACH;
    loop {
        live.drain();
        let fresh = strip_csi(&live.transcript[since..]);
        let witnessed = if complete_line {
            fresh
                .split_inclusive('\n')
                .any(|line| line.ends_with('\n') && line.trim_end_matches(['\r', '\n']) == needle)
        } else {
            fresh.contains(needle)
        };
        if witnessed
            && live
                .grid
                .nonempty_rows()
                .iter()
                .any(|row| row.contains(needle))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "missing {needle:?}; grid={:?}; transcript={:?}",
            live.grid.nonempty_rows(),
            live.transcript
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The visible text, with every CSI sequence removed; an absolute cursor
/// move (the frame paints each row at its own position) counts as a line
/// break, so a row painted that way is witnessed as a complete line.
fn strip_csi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.clone().next() == Some('[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    if next == 'H' {
                        out.push('\n');
                    }
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[test]
fn rich_file_changes_use_real_scroll_resize_interrupt_and_termios_restoration() {
    let pty = Pty::open();
    pty.resize(24, 80);
    let before = pty.termios_snapshot();
    let mut child = spawn_child(&pty, CHILD, "NEWT_RICH_FILE_CHANGE_CHILD");
    let mut live = Live {
        pty: &pty,
        grid: ScreenModel::new(80),
        transcript: String::new(),
        pending: String::new(),
    };
    reach(&mut live, "last_visible");
    assert!(
        live.transcript.contains("\x1b[48;"),
        "real terminal gets semantic background colors"
    );
    assert!(
        !live.transcript.contains("\x1b]"),
        "source OSC must never reach the terminal"
    );
    pty.type_in(&"\x1b[A".repeat(40));
    reach(&mut live, "Modified state.txt");
    let since = live.transcript.len();
    pty.resize(24, 12);
    live.grid.resize(12);
    // The child repaints a narrow source/header projection before widening
    // again; reflowed wide rows do not count.
    reach_since(&mut live, "⎵ Completed", since, true);
    let since = live.transcript.len();
    pty.resize(24, 80);
    live.grid.resize(80);
    reach_since(&mut live, "Modified state.txt", since, false);
    // The first press leaves explore mode; the second interrupts the turn.
    pty.type_in("\x03\x03");
    reach(&mut live, "receipt-terminal-done");
    let restored = pty.termios_snapshot();
    pty.type_in("\n");
    let status = wait_for_child(&mut child, REACH);
    let transcript = live.transcript;
    assert!(
        status.is_some_and(|status| status.success()),
        "{transcript:?}"
    );
    assert_eq!(restored, before);
    assert!(live
        .grid
        .nonempty_rows()
        .iter()
        .any(|row| row.contains("receipt-committed")));
    assert!(
        !live
            .grid
            .nonempty_rows()
            .iter()
            .any(|row| row.contains("Modified state.txt")),
        "the completed frame was erased without taking committed text"
    );
}

/// **Grounds the placement half of the unit tier on a real terminal.** A
/// live frame too tall for the rows under the transcript scrolls the
/// transcript by the deficit, and when it finishes the next committed line
/// lands on the row directly under the header, with nothing of the frame
/// left: the third blank-band case of 2026-10-06/07. The parent answers the
/// child's cursor queries from an erase-aware replay of what it painted
/// (`Pump`), as a terminal would.
#[serial_test::serial(interaction_pty)]
#[test]
#[ignore = "real-PTY acceptance tier; weekly, release, and scoped PTY CI only"]
fn a_tall_live_frame_finishes_and_the_result_lands_under_the_header() {
    let pty = Pty::open();
    pty.resize(24, 80);
    let mut child = spawn_child(&pty, LIVE_CHILD, "NEWT_LIVE_FRAME_CHILD");
    let mut pump = Pump::new(&pty, true, 24);
    let reached = pump.until("RESULT-LINE").is_some();
    pty.type_in("\n");
    let status = wait_for_child(&mut child, REACH);
    pump.pump();
    let stream = pump.transcript;
    assert!(reached, "the child never printed its result: {stream:?}");
    assert!(status.is_some_and(|status| status.success()), "{stream:?}");
    let before_result = stream.split("RESULT-LINE").next().unwrap_or_default();
    let (grid, cursor) = erasing_screen(before_result, 24);
    let header = grid
        .iter()
        .position(|row| row.trim_end() == "HEADER-LINE")
        .unwrap_or_else(|| panic!("HEADER-LINE is not on screen: {grid:#?}"));
    assert!(
        header < 18,
        "the frame did not scroll the transcript; the header is still on row \
         {header}: {grid:#?}"
    );
    assert_eq!(
        cursor,
        (header + 1, 0),
        "the result must land directly under the header, not under a band of \
         blank rows: {grid:#?}"
    );
    assert!(
        !grid
            .iter()
            .any(|row| row.contains("spill-") || row.contains('▒') || row.contains('▲')),
        "frame residue after finish: {grid:#?}"
    );
}

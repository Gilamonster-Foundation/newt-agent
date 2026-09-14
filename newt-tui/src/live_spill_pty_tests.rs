//! Real terminal witness for the rich model's existing input and erase owner.

use super::{LiveSpillRenderer, ScreenModel};
use newt_core::agentic::{CompletedSpillRenderer, FileChangePresentation};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tests_pty::Pty;

const CHILD: &str = "live_spill::tests::terminal::rich_file_change_child";
const REACH: Duration = Duration::from_secs(30);

#[test]
#[ignore = "child of the rich file-change terminal regression"]
fn rich_file_change_child() {
    if std::env::var_os("NEWT_RICH_FILE_CHANGE_CHILD").is_none() {
        return;
    }
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
}

fn reach(pty: &Pty, grid: &mut ScreenModel, transcript: &mut String, needle: &str) {
    reach_since(pty, grid, transcript, needle, 0);
}

/// Like [`reach`], but the needle must also appear in bytes the child wrote
/// after `since`. After a resize the model reflows rows the child painted at
/// the OLD width, so a grid match alone can be satisfied without the child
/// ever observing the new geometry.
fn reach_since(
    pty: &Pty,
    grid: &mut ScreenModel,
    transcript: &mut String,
    needle: &str,
    since: usize,
) {
    let deadline = Instant::now() + REACH;
    loop {
        let bytes = pty.screen();
        grid.apply(bytes.as_bytes());
        transcript.push_str(&bytes);
        let fresh = strip_csi(&transcript[since..]);
        if fresh.contains(needle) && grid.nonempty_rows().iter().any(|row| row.contains(needle)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "missing {needle:?}; grid={:?}; transcript={transcript:?}",
            grid.nonempty_rows()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn strip_csi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.clone().next() == Some('[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
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
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .env("NEWT_RICH_FILE_CHANGE_CHILD", "1")
        .env("TERM", "xterm-256color")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(pty.slave_stdio())
        .spawn()
        .unwrap();
    let mut grid = ScreenModel::new(80);
    let mut transcript = String::new();
    reach(&pty, &mut grid, &mut transcript, "last_visible");
    assert!(
        transcript.contains("\x1b[48;"),
        "real terminal gets semantic background colors"
    );
    assert!(
        !transcript.contains("\x1b]"),
        "source OSC must never reach the terminal"
    );
    pty.type_in(&"\x1b[A".repeat(40));
    reach(&pty, &mut grid, &mut transcript, "Modified state.txt");
    let since = transcript.len();
    pty.resize(24, 12);
    grid.resize(12);
    // The child repaints a narrow source/header projection before widening
    // again; reflowed wide rows do not count.
    reach_since(&pty, &mut grid, &mut transcript, "⎵ Complete", since);
    let since = transcript.len();
    pty.resize(24, 80);
    grid.resize(80);
    reach_since(
        &pty,
        &mut grid,
        &mut transcript,
        "Modified state.txt",
        since,
    );
    // The first press leaves explore mode; the second interrupts the turn.
    pty.type_in("\x03\x03");
    reach(&pty, &mut grid, &mut transcript, "receipt-terminal-done");
    let status = crate::prompt_visibility_test::wait_for_child(&mut child, REACH);
    assert!(
        status.is_some_and(|status| status.success()),
        "{transcript:?}"
    );
    assert_eq!(pty.termios_snapshot(), before);
    assert!(grid
        .nonempty_rows()
        .iter()
        .any(|row| row.contains("receipt-committed")));
    assert!(
        !grid
            .nonempty_rows()
            .iter()
            .any(|row| row.contains("Modified state.txt")),
        "the completed frame was erased without taking committed text"
    );
}

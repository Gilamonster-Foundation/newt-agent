//! **Real-resource proof of the tool-call liveness contract** (#1727).
//!
//! # What this grounds
//!
//! Real-resource tier, per CLAUDE.md: an add-on to the mocked tier, not a
//! deviation from it. `live_output`'s fake-sink tests prove the *state
//! machine* — that `start` does not yield the row, that `write` yields before
//! delegating, that `Drop` yields. None of them can prove the thing the
//! operator actually reported, because all three are assertions about a slot
//! rather than about bytes on a terminal:
//!
//! 1. **`a_silent_child_keeps_the_spinner_until_the_tool_returns`** shows the
//!    slot stays full while a silent tool runs. It cannot show that anything
//!    was ever *drawn* — a spinner that held its lease and painted nothing
//!    would pass it, and would be exactly the #1727 hang again.
//! 2. **`the_first_chunk_yields_the_row_before_the_viewport_paints`** shows
//!    the slot is empty before the inner sink is called. It cannot show that
//!    no frame lands after the handover: the ticker runs on its own thread,
//!    so the interleaving it would have to observe happens on the wire, not
//!    in the slot.
//!
//! # The contract, stated as bytes
//!
//! - a tool that prints nothing still puts frames between its header and its
//!   result — "waiting" is distinguishable from "hung";
//! - the first live byte hands the row over atomically: frames before it,
//!   **no frame at or after it**, and no erase escape stranded on the
//!   viewport's row.
//!
//! # Why a PTY, and why a child process
//!
//! `LineCaps` refuses to own a line unless stdin *and* stdout are real
//! terminals — correct, and precisely why this bug never surfaced in a piped
//! test. And `cargo test` installs a thread-local capture, so each scenario
//! runs in a child (this same binary, re-invoked with `--nocapture`) whose
//! fd 0/1 *are* the pty. No filesystem, no network, no service.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use tests_pty::Pty;

use super::live_output::ToolSpinner;
use crate::agentic::{LiveToolOutput, ToolOutputStream};

/// The tool header, as `ToolDisplay::call` commits it.
const HEADER: &str = "⚙  run_command: sleep";
/// The canonical result, as `ToolDisplay::result` commits it afterwards.
const RESULT: &str = "▒ (exit 0)";
/// What the wrapped live sink paints on the row the spinner hands it.
const LIVE_MARK: &str = "LIVEBYTES:";
/// Long enough for the 100 ms ticker to get several chances to redraw.
const DWELL: Duration = Duration::from_millis(300);

/// Braille frames — the spinner's alphabet — present in `s`.
fn frames(s: &str) -> Vec<char> {
    s.chars()
        .filter(|c| ('\u{2800}'..='\u{28FF}').contains(c))
        .collect()
}

fn flush() {
    let _ = std::io::stdout().flush();
}

/// Stands in for the live-output viewport: paints one marked row per chunk.
struct MarkerSink;

impl LiveToolOutput for MarkerSink {
    fn start(&self, _generation: u64) {}
    fn write(&self, _generation: u64, _stream: ToolOutputStream, chunk: &[u8]) {
        println!("{LIVE_MARK}{}", String::from_utf8_lossy(chunk));
        flush();
    }
    fn finish(&self, _generation: u64) {}
    fn abandon(&self, _generation: u64) {}
}

// ---------------------------------------------------------------------------
// The children: the scenarios themselves, run with fd 0/1 on a pty.
// ---------------------------------------------------------------------------

/// A tool that runs for a while and prints NOTHING — `gh pr list` waiting on
/// the network, an MCP call, `experience_recall`. The funnel's spinner is the
/// only thing between the header and the result.
#[test]
#[ignore = "child process of the tool-spinner PTY test"]
fn silent_tool_child() {
    if std::env::var_os("NEWT_TOOL_SPINNER_PTY_CHILD").as_deref() != Some("silent".as_ref()) {
        return;
    }
    println!("{HEADER} 1");
    flush();
    {
        // The real entry point, through `LineCaps::detect()`: fd 0/1 are the
        // pty and TERM is set by the parent, so this must yield a spinner.
        let spinner = ToolSpinner::start("run_command", true);
        assert!(
            spinner.is_live(),
            "a pty child with TERM set must be able to own the line"
        );
        std::thread::sleep(DWELL); // the tool runs; not one byte of output
    } // the tool returns: the funnel drops the spinner, which erases
    println!("{RESULT}");
    flush();
}

/// A tool whose first output arrives late: frames until that byte, then the
/// row belongs to the viewport and the spinner must never paint again.
#[test]
#[ignore = "child process of the tool-spinner PTY test"]
fn delayed_first_output_child() {
    if std::env::var_os("NEWT_TOOL_SPINNER_PTY_CHILD").as_deref() != Some("delayed".as_ref()) {
        return;
    }
    println!("{HEADER} 2");
    flush();
    {
        let spinner = ToolSpinner::start("run_command", true);
        let sink = spinner
            .wrap(Some(Arc::new(MarkerSink) as Arc<dyn LiveToolOutput>))
            .expect("a live sink is wrapped");
        std::thread::sleep(DWELL); // the child is silent: frames
        sink.start(1);
        // `start` is bookkeeping, NOT a handover — frames must continue.
        std::thread::sleep(DWELL);
        sink.write(1, ToolOutputStream::Stdout, b"first output");
        // The row now belongs to the viewport. Give the ticker several more
        // chances to prove it will not take it back.
        std::thread::sleep(DWELL);
    }
    println!("{RESULT}");
    flush();
}

// ---------------------------------------------------------------------------
// The parents
// ---------------------------------------------------------------------------

fn run_scenario(scenario: &str, child_test: &str) -> String {
    let pty = Pty::open();
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("the test binary re-invokes itself"),
    )
    .args(["--exact", child_test, "--ignored", "--nocapture"])
    .env("NEWT_TOOL_SPINNER_PTY_CHILD", scenario)
    // `LineCaps` refuses an absent/`dumb` TERM, and CI often has neither —
    // set it so the child's capability is the pty's, not the runner's.
    .env("TERM", "xterm-256color")
    .stdin(pty.slave_stdio())
    .stdout(pty.slave_stdio())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn the pty child");
    let status = child.wait().expect("wait for the pty child");
    let screen = pty.screen();
    assert!(
        status.success(),
        "the {scenario} scenario child failed.\n\nscreen:\n{screen:?}"
    );
    screen
}

fn locate(screen: &str, needle: &str, what: &str) -> usize {
    screen
        .find(needle)
        .unwrap_or_else(|| panic!("{what} never reached the terminal.\n\nscreen:\n{screen:?}"))
}

/// #1727 as the operator met it: a long, silent tool call showed NOTHING
/// under its header, so waiting was indistinguishable from hung.
#[serial_test::serial(tty_arbiter)]
#[test]
fn a_silent_tool_call_is_never_a_blank_row() {
    let screen = run_scenario(
        "silent",
        "agentic::tools::tool_spinner_pty_test::silent_tool_child",
    );
    let header_end = locate(&screen, HEADER, "the tool header") + HEADER.len();
    let result_at = locate(&screen, RESULT, "the tool result");

    assert!(
        !frames(&screen[header_end..result_at]).is_empty(),
        "no spinner between the header and the result — this IS #1727.\n\nbetween:\n{:?}",
        &screen[header_end..result_at]
    );
    // It is the TOOL's spinner, not a leftover inference one: the label is
    // the tool's presentation name.
    assert!(
        screen[header_end..result_at].contains("run_command…"),
        "the spinner did not carry the tool's label.\n\nbetween:\n{:?}",
        &screen[header_end..result_at]
    );
    // And the row was handed back before the result was committed: nothing
    // may animate at or after the canonical output.
    assert!(
        frames(&screen[result_at..]).is_empty(),
        "a spinner frame survived past the result.\n\nafter:\n{:?}",
        &screen[result_at..]
    );
}

/// The hand-off, on the wire: frames until the first live byte, then the row
/// belongs to the viewport and the spinner is gone — no stale chrome, no
/// frame painted over the viewport's own first row.
#[serial_test::serial(tty_arbiter)]
#[test]
fn the_first_live_byte_takes_the_row_and_the_spinner_never_returns() {
    let screen = run_scenario(
        "delayed",
        "agentic::tools::tool_spinner_pty_test::delayed_first_output_child",
    );
    let header_end = locate(&screen, HEADER, "the tool header") + HEADER.len();
    let live_at = locate(&screen, LIVE_MARK, "the live output");
    let result_at = locate(&screen, RESULT, "the tool result");

    assert!(
        !frames(&screen[header_end..live_at]).is_empty(),
        "no spinner covered the child before its first byte.\n\nbefore:\n{:?}",
        &screen[header_end..live_at]
    );
    // THE invariant: after the handover the old spinner must never paint.
    assert!(
        frames(&screen[live_at..]).is_empty(),
        "a spinner frame landed at or after the live output — stale chrome on \
         the viewport's row.\n\nafter the handover:\n{:?}",
        &screen[live_at..]
    );
    assert!(
        live_at < result_at,
        "the live output must precede the canonical result"
    );
}

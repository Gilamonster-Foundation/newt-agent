//! The **first-run setup / provisioning screen** — the TUI shown while the
//! on-host summarizer model is downloaded on first launch.
//!
//! The binary (newt-cli) provisions the model on a background thread and hands
//! `run_code` a [`SetupHandle`]; the splash then COVERS that work with a
//! spinner and status line under the logo, so the download never runs as raw
//! cooked-mode output before the TUI comes up. [`SetupEvent`]s stream progress
//! from that thread to this screen.
//!
//! This is the UI of the setup step. The *logic* lives elsewhere: [`crate::setup`]
//! is the human-driven config wizard (`newt setup`), and [`crate::wizard`] is the
//! silent auto-prober behind `newt init`. Extracted from `lib.rs` in the #1096
//! functional-cohesion pass; the module name says what it is — the UI, not the
//! logic.

use std::io::{self, Write as _};

use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    queue,
    style::{Color as CtColor, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};

use newt_core::agentic::NEWT_ORANGE_CT;

use crate::logo_for_size;

/// Progress events from the background provisioning thread to the setup screen.
pub enum SetupEvent {
    /// A named step began (e.g. `weights`, `tokenizer`).
    Step(String),
    /// Byte progress on the current step (`total` unknown until headers arrive).
    Progress { done: u64, total: Option<u64> },
    /// Every step finished — the model is provisioned.
    Done,
    /// Setup failed (offline / firewalled / disk); the session continues degraded.
    Failed(String),
}

/// Handed to [`run_code`] by the binary when a first-run provision is needed.
/// `rx` streams [`SetupEvent`]s from the download thread; setting `cancel` asks
/// that thread to stop (triple-Esc / Ctrl+C).
pub struct SetupHandle {
    /// Human label for what's being set up (e.g. `on-host summarizer (qwen2.5-0.5b)`).
    pub what: String,
    /// Progress stream from the background thread.
    pub rx: std::sync::mpsc::Receiver<SetupEvent>,
    /// Set to request the background thread abort.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Is this key an abort press (Esc or Ctrl+C)? Three in a row abort setup.
fn is_abort_key(ev: &Event) -> bool {
    match ev {
        Event::Key(KeyEvent {
            code: KeyCode::Esc, ..
        }) => true,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            ..
        }) => modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

/// The status line from the latest provisioning state.
fn setup_status_line(what: &str, step: &str, done: u64, total: Option<u64>) -> String {
    let mb = |b: u64| b / 1_048_576;
    match total.filter(|&t| t > 0) {
        Some(t) => format!(
            "setting up {what} — {step} {}/{} MB  {}%",
            mb(done),
            mb(t),
            done.saturating_mul(100) / t
        ),
        None => format!("setting up {what} — {step} {} MB", mb(done)),
    }
}

/// Poll `rx` non-blocking, folding events into the running state. Returns
/// `Some(Ok/Err)` once setup is finished (or the sender died), else `None`.
fn drain_setup(
    rx: &std::sync::mpsc::Receiver<SetupEvent>,
    step: &mut String,
    done: &mut u64,
    total: &mut Option<u64>,
) -> Option<Result<(), String>> {
    use std::sync::mpsc::TryRecvError;
    loop {
        match rx.try_recv() {
            Ok(SetupEvent::Step(s)) => {
                *step = s;
                *done = 0;
                *total = None;
            }
            Ok(SetupEvent::Progress { done: d, total: t }) => {
                *done = d;
                *total = t;
            }
            Ok(SetupEvent::Done) => return Some(Ok(())),
            Ok(SetupEvent::Failed(e)) => return Some(Err(e)),
            Err(TryRecvError::Empty) => return None,
            // Sender dropped without a terminal event → the thread died.
            Err(TryRecvError::Disconnected) => return Some(Err("interrupted".into())),
        }
    }
}

/// The alt-screen setup screen: logo + a prominent spinner/status while the model
/// provisions on a background thread. Blocks input except a triple abort. Returns
/// when setup finishes, fails, or is aborted (the caller then shows the splash).
pub(crate) fn run_setup_screen(
    out: &mut io::Stdout,
    color: bool,
    setup: SetupHandle,
) -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let (logo, _logo_cols) = logo_for_size(cols, rows);
    let logo_rows = logo.lines().count() as u16;

    let mut frame = 0usize;
    let mut aborts = 0u8;
    let mut step = "starting".to_string();
    let (mut done, mut total) = (0u64, None);

    loop {
        let finished = drain_setup(&setup.rx, &mut step, &mut done, &mut total);

        queue!(out, Clear(ClearType::All), MoveTo(0, 0))?;
        write!(out, "{}", logo.replace('\n', "\r\n"))?;
        let row = logo_rows + 1;
        let (glyph, line, hint) = match &finished {
            None => (
                newt_core::tty::SPINNER_FRAMES[frame % newt_core::tty::SPINNER_FRAMES.len()]
                    .to_string(),
                setup_status_line(&setup.what, &step, done, total),
                "triple-Esc to skip (uses the session model instead)",
            ),
            Some(Ok(())) => (
                "✓".to_string(),
                format!("ready — {} set up", setup.what),
                "",
            ),
            Some(Err(e)) => (
                "⚠".to_string(),
                format!("setup skipped ({e}) — will use the session model"),
                "",
            ),
        };
        queue!(out, MoveTo(2, row))?;
        if color {
            queue!(out, SetForegroundColor(NEWT_ORANGE_CT))?;
        }
        queue!(out, Print(format!("{glyph}  {line}")), ResetColor)?;
        if !hint.is_empty() {
            queue!(
                out,
                MoveTo(2, row + 1),
                SetForegroundColor(CtColor::DarkGrey),
                Print(hint),
                ResetColor
            )?;
        }
        out.flush()?;

        if finished.is_some() {
            // Hold briefly so the result is seen, then drain any keys typed during
            // setup so the following splash isn't instantly dismissed by them.
            let _ = event::poll(std::time::Duration::from_millis(800))?;
            while event::poll(std::time::Duration::from_millis(0))? {
                let _ = event::read()?;
            }
            return Ok(());
        }

        // Animate + poll input at ~100ms. Non-abort keys are swallowed (blocked);
        // three consecutive Esc/Ctrl+C cancel the download and return.
        if event::poll(std::time::Duration::from_millis(100))? {
            if is_abort_key(&event::read()?) {
                aborts += 1;
                if aborts >= 3 {
                    setup.cancel.store(true, Ordering::SeqCst);
                    while event::poll(std::time::Duration::from_millis(0))? {
                        let _ = event::read()?;
                    }
                    return Ok(());
                }
            } else {
                aborts = 0;
            }
        }
        frame += 1;
    }
}

/// The no-splash analog: a single carriage-return-updating spinner line (the
/// header is already printed by the caller). No input capture — Ctrl+C is the
/// terminal's SIGINT.
pub(crate) fn run_setup_inline(setup: &SetupHandle) {
    let mut frame = 0usize;
    let mut step = "starting".to_string();
    let (mut done, mut total) = (0u64, None);
    loop {
        match drain_setup(&setup.rx, &mut step, &mut done, &mut total) {
            None => {
                eprint!(
                    "\r  {}  {}   ",
                    newt_core::tty::SPINNER_FRAMES[frame % newt_core::tty::SPINNER_FRAMES.len()],
                    setup_status_line(&setup.what, &step, done, total)
                );
                let _ = io::stderr().flush();
            }
            Some(r) => {
                let msg = match r {
                    Ok(()) => format!("✓ {} ready", setup.what),
                    Err(e) => format!("⚠ setup skipped ({e}) — using the session model"),
                };
                eprintln!("\r  {msg}                    ");
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        frame += 1;
    }
}

#[cfg(test)]
mod setup_screen_tests {
    use super::{drain_setup, is_abort_key, setup_status_line, SetupEvent};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    const MB: u64 = 1_048_576;

    #[test]
    fn status_line_shows_pct_with_total_and_bare_mb_without() {
        let known = setup_status_line("summarizer", "weights", 50 * MB, Some(100 * MB));
        assert!(known.contains("50/100 MB"), "{known}");
        assert!(known.contains("50%"), "{known}");
        let unknown = setup_status_line("summarizer", "weights", 7 * MB, None);
        assert!(
            unknown.contains("7 MB") && !unknown.contains('%'),
            "{unknown}"
        );
    }

    #[test]
    fn abort_key_is_esc_or_ctrl_c_only() {
        assert!(is_abort_key(&Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE
        ))));
        assert!(is_abort_key(&Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        ))));
        // A bare 'c' or Enter must NOT abort — the splash blocks ordinary input.
        assert!(!is_abort_key(&Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        ))));
        assert!(!is_abort_key(&Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        ))));
    }

    #[test]
    fn drain_folds_progress_and_reports_done() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (mut step, mut done, mut total) = (String::new(), 0u64, None);
        assert!(drain_setup(&rx, &mut step, &mut done, &mut total).is_none());
        tx.send(SetupEvent::Step("weights".into())).unwrap();
        tx.send(SetupEvent::Progress {
            done: 42,
            total: Some(100),
        })
        .unwrap();
        assert!(drain_setup(&rx, &mut step, &mut done, &mut total).is_none());
        assert_eq!((step.as_str(), done, total), ("weights", 42, Some(100)));
        tx.send(SetupEvent::Done).unwrap();
        assert!(matches!(
            drain_setup(&rx, &mut step, &mut done, &mut total),
            Some(Ok(()))
        ));
    }

    #[test]
    fn drain_reports_sender_death_as_error() {
        // If the download thread dies without a terminal event, the screen must
        // stop waiting (else it spins forever) — Disconnected → Err.
        let (tx, rx) = std::sync::mpsc::channel::<SetupEvent>();
        drop(tx);
        let (mut step, mut done, mut total) = (String::new(), 0u64, None);
        assert!(matches!(
            drain_setup(&rx, &mut step, &mut done, &mut total),
            Some(Err(_))
        ));
    }
}

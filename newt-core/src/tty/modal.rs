use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::io::{self, IsTerminal, Write};
use std::marker::PhantomData;
use std::time::Duration;

use super::PromptWindow;

pub const MODAL_CONTROL_HINT: &str = "Esc=back · Ctrl-C/Ctrl-D=exit";

/// The input glyph a modal draws its answer line behind. Shares the chevron
/// morphology of the chat prompt (`newt_tui::prompt`) so a modal reads as the
/// same input surface the user already types at, rather than a second style of
/// prompt stacked under it.
pub const MODAL_INPUT_GLYPH: &str = "❯ ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptLine {
    Line(String),
    Back,
    Exit,
    Eof,
}

/// Raw mode for the modal's read, restored to EXACTLY what it was on drop.
///
/// Not `crossterm::terminal::enable_raw_mode`, and the difference is the
/// bug: crossterm keeps ONE process-global "mode prior to raw" and makes a
/// second `enable_raw_mode` a no-op while it is set. Under the cockpit
/// (#1669) the terminal thread already owns raw mode for the whole session,
/// so the modal's request did nothing — and `StdinToken::acquire` had just
/// switched the tty to canonical+echo for the line-reader path. Result: keys
/// line-buffered until Enter, echoed by the kernel over the editor row, and a
/// prompt that looked hung. Saving and restoring the termios here, the way
/// `StdinToken` does for line mode, makes nested ownership simply compose.
struct RawGuard {
    #[cfg(unix)]
    prev: Option<libc::termios>,
}

impl RawGuard {
    fn enter() -> io::Result<Self> {
        #[cfg(unix)]
        {
            // SAFETY: termios round-trip on stdin, restored on drop.
            let prev = unsafe {
                let fd = libc::STDIN_FILENO;
                let mut prev: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(fd, &mut prev) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut raw = prev;
                libc::cfmakeraw(&mut raw);
                if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                    return Err(io::Error::last_os_error());
                }
                prev
            };
            Ok(Self { prev: Some(prev) })
        }
        #[cfg(not(unix))]
        {
            crossterm::terminal::enable_raw_mode()?;
            Ok(Self {})
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(prev) = self.prev.take() {
            // SAFETY: restoring the termios captured in `enter`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &prev);
            }
        }
        #[cfg(not(unix))]
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Classify one submitted headless line (the piped/non-TTY branch, after its
/// EOF check): a line that is exactly the literal ESC byte is the piped
/// stand-in for the Back control; anything else — including an empty
/// submission — is an answer, with the trailing newline family trimmed. Pure
/// so the piped convention is testable without a terminal or a stdin read
/// (#1823 A0 freeze; the Ok(0) => Eof arm above it stays in the caller).
fn classify_headless_prompt_line(line: &str) -> PromptLine {
    let line = line.trim_end_matches(['\r', '\n']);
    if line == "\u{1b}" {
        PromptLine::Back
    } else {
        PromptLine::Line(line.into())
    }
}

/// Read a line while `window` exclusively owns the terminal.
pub fn read_prompt_window_line(window: &PromptWindow, prompt: &str) -> io::Result<PromptLine> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        window.ask(prompt)?;
        let mut line = String::new();
        if window.read_line_into(&mut line)? == 0 {
            return Ok(PromptLine::Eof);
        }
        return Ok(classify_headless_prompt_line(&line));
    }

    let result = {
        let _guard = take_modal_ownership(window)?;
        window.ask(&render(prompt, "", true)?)?;
        let mut value = String::new();
        loop {
            let key = match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => key,
                Event::Paste(text) => {
                    value.extend(text.chars().filter(|ch| !ch.is_control()));
                    window.ask(&render(prompt, &value, false)?)?;
                    continue;
                }
                _ => continue,
            };
            if let Some(line) = prompt_control(&key) {
                break line;
            }
            match key.code {
                KeyCode::Enter => break PromptLine::Line(value),
                KeyCode::Backspace => {
                    value.pop();
                    window.ask(&render(prompt, &value, false)?)?;
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::ALT)
                        && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    value.push(ch);
                    window.ask(&render(prompt, &value, false)?)?;
                }
                _ => {}
            }
        }
    };
    window.notice("")?;
    Ok(result)
}

/// A pollable source of terminal control keys during a prompt wait. The trait is
/// the test seam for the web-decision reader-recovery lifecycle (defect 1): a
/// fake implementation can script `Err`/`Back`/`Exit`/`None` sequences without a
/// real terminal, so the gate's recovery logic is unit-testable in the
/// fully-mocked tier. Real terminals use [`PromptControlReader`].
pub trait ControlReader {
    /// Poll for a control key for up to `timeout`. `Ok(None)` on no input;
    /// `Ok(Some(_))` on a control/line/EOF event; `Err` on an I/O failure (an
    /// [`io::ErrorKind::Interrupted`] error is transient and should be retried
    /// on the SAME reader; any other error means the reader is broken).
    fn poll(&mut self, timeout: Duration) -> io::Result<Option<PromptLine>>;
}

/// Holds raw mode for as long as the reader lives — the guard is here for
/// its `Drop`, which is the whole point of owning it.
pub struct PromptControlReader<'w> {
    _raw: RawGuard,
    _window: PhantomData<&'w PromptWindow>,
}

pub fn modal_prompt_controls<'w>(window: &'w PromptWindow) -> io::Result<PromptControlReader<'w>> {
    Ok(PromptControlReader {
        _raw: take_modal_ownership(window)?,
        _window: PhantomData,
    })
}

impl ControlReader for PromptControlReader<'_> {
    fn poll(&mut self, timeout: Duration) -> io::Result<Option<PromptLine>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => Ok(prompt_control(&key)),
            _ => Ok(None),
        }
    }
}

fn prompt_control(key: &KeyEvent) -> Option<PromptLine> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(ch) if ctrl && matches!(ch, 'c' | 'd') => Some(PromptLine::Exit),
        KeyCode::Esc => Some(PromptLine::Back),
        _ => None,
    }
}

fn render(prompt: &str, value: &str, first: bool) -> io::Result<String> {
    let mut out = Vec::new();
    if first {
        write!(out, "{}", prompt.replace('\n', "\r\n"))?;
    } else {
        crossterm::queue!(
            out,
            crossterm::cursor::MoveToColumn(0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine)
        )?;
        write!(out, "{}", prompt.rsplit_once('\n').map_or(prompt, |x| x.1))?;
    }
    write!(out, "{value}")?;
    String::from_utf8(out).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn take_modal_ownership(_window: &PromptWindow) -> io::Result<RawGuard> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminal required",
        ));
    }
    RawGuard::enter()
}

#[cfg(all(test, unix))]
mod raw_guard_tests {
    use super::RawGuard;

    /// stdin as a real pty for the duration of a test: `RawGuard` operates on
    /// `STDIN_FILENO`, and only a terminal has termios. Restores fd 0 on drop.
    struct StdinPty {
        saved: libc::c_int,
        master: libc::c_int,
    }

    impl StdinPty {
        fn install() -> Self {
            // SAFETY: openpty + dup2 on descriptors this test owns; restored
            // in Drop.
            unsafe {
                let (mut master, mut slave) = (-1, -1);
                assert_eq!(
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut::<libc::termios>(),
                        std::ptr::null_mut::<libc::winsize>()
                    ),
                    0
                );
                let saved = libc::dup(0);
                assert!(libc::dup2(slave, 0) >= 0);
                libc::close(slave);
                Self { saved, master }
            }
        }
        fn lflag() -> libc::tcflag_t {
            // SAFETY: tcgetattr into a zeroed termios.
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(0, &mut t), 0);
                t.c_lflag
            }
        }
        fn set_canonical_echo(on: bool) {
            // SAFETY: termios round-trip on fd 0 (our pty slave).
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(0, &mut t), 0);
                if on {
                    t.c_lflag |= libc::ICANON | libc::ECHO;
                } else {
                    t.c_lflag &= !(libc::ICANON | libc::ECHO);
                }
                assert_eq!(libc::tcsetattr(0, libc::TCSANOW, &t), 0);
            }
        }
    }

    impl Drop for StdinPty {
        fn drop(&mut self) {
            // SAFETY: putting fd 0 back and closing what we opened.
            unsafe {
                libc::dup2(self.saved, 0);
                libc::close(self.saved);
                libc::close(self.master);
            }
        }
    }

    /// The bug (#1669 cockpit): with crossterm's process-global raw-mode
    /// state already set by another owner, `enable_raw_mode` is a no-op, so a
    /// modal opened over a canonical+echo tty stayed canonical — keys buffered
    /// until Enter and echoed by the kernel over the editor. The guard must
    /// take raw mode by ITSELF, whatever crossterm believes.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_guard_takes_raw_mode_regardless_of_crossterms_global_state() {
        let _stdin = StdinPty::install();
        StdinPty::set_canonical_echo(true);
        // Simulate the cockpit: crossterm already thinks raw mode is on.
        let _ = crossterm::terminal::enable_raw_mode();
        // …but the tty is canonical+echo (the line-reader path just set it).
        StdinPty::set_canonical_echo(true);
        assert!(
            StdinPty::lflag() & libc::ICANON != 0,
            "precondition: canonical"
        );
        {
            let _guard = RawGuard::enter().expect("raw");
            let l = StdinPty::lflag();
            assert_eq!(
                l & libc::ICANON,
                0,
                "the modal's read must be non-canonical"
            );
            assert_eq!(
                l & libc::ECHO,
                0,
                "the kernel must not echo over the editor"
            );
        }
        // Restored to EXACTLY the prior state — canonical+echo, which the
        // enclosing `StdinToken` then restores in turn.
        let l = StdinPty::lflag();
        assert!(
            l & libc::ICANON != 0 && l & libc::ECHO != 0,
            "prior mode restored on drop"
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// A0 freeze (#1823): the piped/headless prompt-line convention, pinned. The
/// deliberately different terminal path (key-by-key with prompt_control) has
/// its own weekly PTY tier; THIS is the contract `newt solve`-when-piped and
/// the eval harness rely on.
///
/// Boundary of this pin: these tests exercise the pure classifier directly;
/// the guard that `read_prompt_window_line`'s non-TTY branch still CALLS it
/// is the dead-code lint under the zero-warnings gate (the fn's only
/// production caller is that branch). The C0/C1 slice that reworks the
/// branch inherits responsibility for re-pinning the wiring.
#[cfg(test)]
mod headless_line_tests {
    use super::*;

    #[test]
    fn a_literal_esc_line_is_the_piped_back_control() {
        assert_eq!(classify_headless_prompt_line("\u{1b}\n"), PromptLine::Back);
        assert_eq!(classify_headless_prompt_line("\u{1b}"), PromptLine::Back);
    }

    #[test]
    fn an_answer_keeps_its_content_and_loses_only_the_newline_family() {
        assert_eq!(
            classify_headless_prompt_line("qwen2.5-coder:7b\r\n"),
            PromptLine::Line("qwen2.5-coder:7b".into())
        );
        // Interior whitespace is the answer's own business.
        assert_eq!(
            classify_headless_prompt_line("  padded  \n"),
            PromptLine::Line("  padded  ".into())
        );
    }

    #[test]
    fn an_explicitly_empty_submission_is_an_empty_answer_not_a_control() {
        // EOF (Ok(0)) is classified UPSTREAM as PromptLine::Eof; an empty line
        // that WAS submitted stays an answer — the distinction
        // request_user_input's typed outcomes depend on.
        assert_eq!(
            classify_headless_prompt_line("\n"),
            PromptLine::Line(String::new())
        );
    }

    #[test]
    fn esc_with_trailing_content_is_an_answer_not_back() {
        assert_eq!(
            classify_headless_prompt_line("\u{1b}[A\n"),
            PromptLine::Line("\u{1b}[A".into())
        );
    }
}

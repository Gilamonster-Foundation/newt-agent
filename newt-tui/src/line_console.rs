//! The wizard/form I/O seam: a line-based `Console` every setup flow is
//! parameterised over, so scripted answers drive the whole flow in tests
//! (docs/decisions/tty_widget_suite.md tracks the eventual `PromptWindow`
//! migration — everything here stays one mechanical rename away).
//!
//! Plain-scroller-safe (docs/decisions/plain_scroller_tui.md): the raw-mode
//! reads below are per-prompt cooked-in/cooked-out line reads — the same
//! carve-out class as the old first-run countdown — never an alt screen or a
//! UI loop. Raw mode buys two things the wizard needs: secrets typed without
//! echo, and Esc/Ctrl-C surfacing as catchable events (so first-run setup can
//! fall back to defaults instead of the process dying mid-wizard).

use std::io::{self, Write};

pub trait Console {
    fn ask(&mut self, prompt: &str) -> io::Result<String>;
    /// Ask without echoing the answer (tokens, passphrases). Default:
    /// delegate to [`ask`](Console::ask) so scripted test consoles answer
    /// secrets like any other prompt.
    fn ask_secret(&mut self, prompt: &str) -> io::Result<String> {
        self.ask(prompt)
    }
    fn say(&mut self, line: &str);
}

pub struct StdinConsole;
impl Console for StdinConsole {
    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut buf = String::new();
        let n = io::stdin().read_line(&mut buf)?;
        if n == 0 {
            return Ok(String::new());
        }
        Ok(buf.trim().to_string())
    }
    fn ask_secret(&mut self, prompt: &str) -> io::Result<String> {
        read_line_raw(prompt, Echo::Stars)
    }
    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

pub fn is_yes(input: &str, default: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => false,
    }
}

/// Whether a raw-mode line read shows the characters typed. `Stars` echoes
/// one `*` per character — a secret still gets keystroke feedback (a fully
/// silent prompt reads as a hung terminal, per field testing) without ever
/// showing the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Echo {
    Chars,
    Stars,
}

/// What one key event does to a raw-mode line read. Pure — unit-tested
/// without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStep {
    Push(char),
    Backspace,
    Done,
    /// Esc or Ctrl-C — surfaces as `io::ErrorKind::Interrupted`.
    Abort,
    /// Ctrl-D on an empty buffer — surfaces as `io::ErrorKind::UnexpectedEof`.
    Eof,
    Ignore,
}

/// Map a key event to a [`KeyStep`]. Pure.
pub fn key_step(ev: &crossterm::event::KeyEvent, buf_empty: bool) -> KeyStep {
    use crossterm::event::{KeyCode, KeyModifiers};
    if ev.kind == crossterm::event::KeyEventKind::Release {
        return KeyStep::Ignore;
    }
    match (ev.code, ev.modifiers) {
        (KeyCode::Enter, _) => KeyStep::Done,
        (KeyCode::Esc, _) => KeyStep::Abort,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => KeyStep::Abort,
        (KeyCode::Char('d'), KeyModifiers::CONTROL) if buf_empty => KeyStep::Eof,
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => KeyStep::Ignore,
        (KeyCode::Backspace, _) => KeyStep::Backspace,
        (KeyCode::Char(c), m) if m.is_empty() || m == KeyModifiers::SHIFT => KeyStep::Push(c),
        _ => KeyStep::Ignore,
    }
}

/// RAII cooked-mode restorer — every exit path (incl. `?`) leaves the
/// terminal usable, the same discipline the first-run countdown used.
struct RawGuard;
impl RawGuard {
    fn enter() -> Option<Self> {
        crossterm::terminal::enable_raw_mode().ok().map(|_| Self)
    }
}
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// One raw-mode line read: prompt, then per-key handling via [`key_step`].
/// `Abort` → `ErrorKind::Interrupted`; `Eof` → `ErrorKind::UnexpectedEof`.
/// Not a terminal (or raw mode unavailable) → falls back to a plain cooked
/// `read_line`, so piped/scripted invocations keep working.
pub fn read_line_raw(prompt: &str, echo: Echo) -> io::Result<String> {
    use std::io::IsTerminal as _;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return cooked_read(prompt);
    }
    print!("{prompt}");
    io::stdout().flush()?;
    let Some(_guard) = RawGuard::enter() else {
        return cooked_read(prompt);
    };
    let mut buf = String::new();
    loop {
        let crossterm::event::Event::Key(ev) = crossterm::event::read()? else {
            continue;
        };
        match key_step(&ev, buf.is_empty()) {
            KeyStep::Push(c) => {
                buf.push(c);
                match echo {
                    Echo::Chars => print!("{c}"),
                    Echo::Stars => print!("*"),
                }
                io::stdout().flush()?;
            }
            KeyStep::Backspace => {
                if buf.pop().is_some() {
                    print!("\x08 \x08");
                    io::stdout().flush()?;
                }
            }
            KeyStep::Done => {
                println!("\r");
                return Ok(buf.trim().to_string());
            }
            KeyStep::Abort => {
                println!("\r");
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            KeyStep::Eof => {
                println!("\r");
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
            }
            KeyStep::Ignore => {}
        }
    }
}

fn cooked_read(prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// The first-run console: like [`StdinConsole`] but every prompt goes
/// through the raw-mode reader, so Esc/Ctrl-C/Ctrl-D become catchable
/// `io::Error`s (the wizard falls back to probe-and-write defaults) instead
/// of SIGINT killing the process mid-setup.
pub struct FirstRunConsole;
impl Console for FirstRunConsole {
    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        read_line_raw(prompt, Echo::Chars)
    }
    fn ask_secret(&mut self, prompt: &str) -> io::Result<String> {
        read_line_raw(prompt, Echo::Stars)
    }
    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn key_step_table() {
        assert_eq!(
            key_step(&ev(KeyCode::Char('a'), KeyModifiers::NONE), true),
            KeyStep::Push('a')
        );
        assert_eq!(
            key_step(&ev(KeyCode::Char('A'), KeyModifiers::SHIFT), false),
            KeyStep::Push('A')
        );
        assert_eq!(
            key_step(&ev(KeyCode::Backspace, KeyModifiers::NONE), false),
            KeyStep::Backspace
        );
        assert_eq!(
            key_step(&ev(KeyCode::Enter, KeyModifiers::NONE), false),
            KeyStep::Done
        );
        assert_eq!(
            key_step(&ev(KeyCode::Esc, KeyModifiers::NONE), false),
            KeyStep::Abort
        );
        assert_eq!(
            key_step(&ev(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            KeyStep::Abort
        );
        // Ctrl-D: EOF only on an empty buffer (like a shell).
        assert_eq!(
            key_step(&ev(KeyCode::Char('d'), KeyModifiers::CONTROL), true),
            KeyStep::Eof
        );
        assert_eq!(
            key_step(&ev(KeyCode::Char('d'), KeyModifiers::CONTROL), false),
            KeyStep::Ignore
        );
        // Other control chords are ignored, not typed.
        assert_eq!(
            key_step(&ev(KeyCode::Char('x'), KeyModifiers::ALT), false),
            KeyStep::Ignore
        );
    }

    #[test]
    fn ask_secret_default_impl_delegates_to_ask() {
        struct Scripted(Vec<String>);
        impl Console for Scripted {
            fn ask(&mut self, _p: &str) -> io::Result<String> {
                Ok(self.0.remove(0))
            }
            fn say(&mut self, _l: &str) {}
        }
        let mut c = Scripted(vec!["sk-secret".to_string()]);
        assert_eq!(c.ask_secret("Key: ").unwrap(), "sk-secret");
    }
}

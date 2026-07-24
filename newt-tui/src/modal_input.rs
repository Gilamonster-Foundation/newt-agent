//! Shared line reader for nested operator forms.
//!
//! The top-level chat editors own rich editing/history. Nested forms need only
//! a small contract: Enter submits, Escape goes Back immediately, and Ctrl-D
//! invokes the process-wide emergency brake. Non-TTY EOF stays an ordinary EOF.

use std::io::{self, IsTerminal, Write};

use newt_core::tty::PromptWindow;

/// Shared operator-facing vocabulary for every nested text form.
pub(crate) const MODAL_CONTROL_HINT: &str = "Esc=Back · Ctrl+D=Emergency brake — STOP RIGHT NOW";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModalLine {
    Line(String),
    Back,
    Eof,
}

struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

pub(crate) fn read_modal_line(prompt: &str) -> io::Result<ModalLine> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(ModalLine::Eof);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        return if line == "\u{1b}" {
            Ok(ModalLine::Back)
        } else {
            Ok(ModalLine::Line(line.to_string()))
        };
    }

    read_tty_line(
        |value, initial| {
            let mut out = io::stdout();
            redraw(&mut out, prompt, value, initial)
        },
        || {
            let mut out = io::stdout();
            writeln!(out, "\r")?;
            out.flush()
        },
    )
}

/// Read a nested line while a [`PromptWindow`] owns both halves of the
/// terminal.
///
/// Requiring the window keeps the arbiter's proof intact: callers cannot put a
/// raw, per-keystroke prompt on top of a live spinner or race the turn watcher.
/// On a TTY, Escape is delivered immediately as [`ModalLine::Back`] and Ctrl-D
/// invokes Newt's process-wide emergency brake. A closed/piped stdin remains a
/// plain [`ModalLine::Eof`].
pub(crate) fn read_prompt_window_line(
    window: &PromptWindow,
    prompt: &str,
) -> io::Result<ModalLine> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        window.ask(prompt)?;
        let mut line = String::new();
        if window.read_line_into(&mut line)? == 0 {
            return Ok(ModalLine::Eof);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        return if line == "\u{1b}" {
            Ok(ModalLine::Back)
        } else {
            Ok(ModalLine::Line(line.to_string()))
        };
    }

    read_tty_line(
        |value, initial| {
            let mut rendered = Vec::new();
            redraw(&mut rendered, prompt, value, initial)?;
            let rendered = String::from_utf8(rendered)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            window.ask(&rendered)
        },
        || window.notice(""),
    )
}

/// Paint the whole prompt once, then only repaint its final row as the value is
/// edited. Permission questions are multi-line; repainting the entire prompt
/// after every key would duplicate the explanatory rows in scrollback.
fn redraw(out: &mut impl Write, prompt: &str, value: &str, initial: bool) -> io::Result<()> {
    if initial {
        // Crossterm raw mode disables OPOST/ONLCR. Spell CRLF explicitly so a
        // multi-line permission explanation does not render as a staircase.
        for part in prompt.split_inclusive('\n') {
            if let Some(row) = part.strip_suffix('\n') {
                write!(out, "{row}")?;
                if !row.ends_with('\r') {
                    write!(out, "\r")?;
                }
                writeln!(out)?;
            } else {
                write!(out, "{part}")?;
            }
        }
        write!(out, "{value}")?;
    } else {
        let final_row = prompt.rsplit_once('\n').map_or(prompt, |(_, row)| row);
        write!(out, "\r\x1b[2K{final_row}{value}")?;
    }
    out.flush()
}

fn read_tty_line(
    mut redraw_line: impl FnMut(&str, bool) -> io::Result<()>,
    mut finish_line: impl FnMut() -> io::Result<()>,
) -> io::Result<ModalLine> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

    crossterm::terminal::enable_raw_mode()?;
    let guard = RawGuard;
    // Enter per-keystroke mode before showing the prompt. This removes the
    // canonical-mode race where a very fast Ctrl-D could be consumed as an EOF
    // marker between painting the question and starting the event reader.
    redraw_line("", true)?;
    let mut value = String::new();
    let outcome = loop {
        match event::read()? {
            event if crate::event_is_emergency_brake(&event) => crate::emergency_brake(),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('c') if ctrl => {
                        break ModalLine::Back;
                    }
                    KeyCode::Esc => {
                        break ModalLine::Back;
                    }
                    KeyCode::Enter => {
                        break ModalLine::Line(value);
                    }
                    KeyCode::Backspace => {
                        let _ = value.pop();
                        redraw_line(&value, false)?;
                    }
                    KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                        value.push(ch);
                        redraw_line(&value, false)?;
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                value.extend(text.chars().filter(|ch| !ch.is_control()));
                redraw_line(&value, false)?;
            }
            _ => {}
        }
    };
    // Return to cooked mode before emitting the terminating newline. This keeps
    // ONLCR translation intact and, more importantly, ensures callers never
    // observe a raw terminal after Back/submit.
    drop(guard);
    finish_line()?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_vocabulary_separates_back_from_stream_eof() {
        assert_ne!(ModalLine::Back, ModalLine::Eof);
        assert_eq!(
            ModalLine::Line("x".to_string()),
            ModalLine::Line("x".to_string())
        );
    }
}

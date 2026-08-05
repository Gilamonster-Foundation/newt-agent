use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::io::{self, IsTerminal, Write};
use std::marker::PhantomData;
use std::time::Duration;

use super::PromptWindow;

pub const MODAL_CONTROL_HINT: &str = "Esc=back · Ctrl-C/Ctrl-D=exit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptLine {
    Line(String),
    Back,
    Exit,
    Eof,
}

struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
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
        let line = line.trim_end_matches(['\r', '\n']);
        return Ok(if line == "\u{1b}" {
            PromptLine::Back
        } else {
            PromptLine::Line(line.into())
        });
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

pub struct PromptControlReader<'w>(RawGuard, PhantomData<&'w PromptWindow>);

pub fn modal_prompt_controls<'w>(window: &'w PromptWindow) -> io::Result<PromptControlReader<'w>> {
    Ok(PromptControlReader(
        take_modal_ownership(window)?,
        PhantomData,
    ))
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
    crossterm::terminal::enable_raw_mode()?;
    Ok(RawGuard)
}

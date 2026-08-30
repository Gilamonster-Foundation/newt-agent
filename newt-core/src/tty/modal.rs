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

/// The shared raw-mode guard (C2b, #1891): promoted out of this module to
/// `tty::raw_mode` when the RichTUI interaction frame needed it too. Its doc
/// carries the #1770 reasoning this module paid for.
use super::raw_mode::RawModeGuard as RawGuard;

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

/// Read a line while `window` exclusively owns the terminal, showing what is
/// typed according to `echo`.
///
/// The piped branch below echoes NOTHING either way, so `echo` is a
/// TTY-branch policy: a pipe never renders the value, and a secret read from
/// one is as safe as an ordinary answer.
pub fn read_prompt_window_line(
    window: &PromptWindow,
    prompt: &str,
    echo: Echo,
) -> io::Result<PromptLine> {
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
                    window.ask(&render(prompt, &echo.display(&value), false)?)?;
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
                    window.ask(&render(prompt, &echo.display(&value), false)?)?;
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::ALT)
                        && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    value.push(ch);
                    window.ask(&render(prompt, &echo.display(&value), false)?)?;
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

/// What a modal read SHOWS of what has been typed.
///
/// A required parameter of [`read_prompt_window_line`], not a default and not
/// a `bool`. A secret prompt that echoes is not a cosmetic defect — the key
/// lands in the scrollback, and from there in whatever captured it. Making
/// the policy unspellable-by-omission is the cheapest way to keep that from
/// being one forgotten argument away, and the interaction path never spells
/// it by hand at all: `present_on_terminal` derives it from the definition,
/// so a `ControlKind::Secret` masks itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Echo {
    /// Show the characters typed. Every ordinary prompt.
    Chars,
    /// Show one `*` per character. A secret still gets keystroke feedback —
    /// a fully silent prompt reads as a hung terminal, per field testing —
    /// without ever showing the value.
    Stars,
}

impl Echo {
    /// What to DISPLAY for `value` under this policy.
    ///
    /// Public because the property worth testing is "the value never appears
    /// in what is shown", and that belongs to callers who ask for secrets —
    /// `newt-tui`'s credential contract asserts it over the real planted key.
    ///
    /// Counts characters, not bytes: a multi-byte key would otherwise draw
    /// more stars than it has characters and leak its byte length.
    #[must_use]
    pub fn display(self, value: &str) -> String {
        match self {
            Self::Chars => value.to_string(),
            Self::Stars => "*".repeat(value.chars().count()),
        }
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

/// **The wiring pin A0 deferred to this slice** (C0b, #1860).
///
/// `headless_line_tests` above exercises the pure classifier directly, and
/// its own doc records the boundary: *"the guard that
/// `read_prompt_window_line`'s non-TTY branch still CALLS it is the
/// dead-code lint … The C0/C1 slice that reworks the branch inherits
/// responsibility for re-pinning the wiring."* C0b discharges that.
///
/// The dead-code lint is a weak guard for this: it proves the function has
/// SOME caller, not that the piped branch is the caller, so moving the call
/// into a test helper would keep the lint quiet while the piped convention
/// silently stopped applying.
///
/// **C0b changes nothing here.** The epic's global acceptance criterion —
/// *"Headless/protocol modes never wait, choose defaults, or emit terminal
/// bytes"* — and this A0 freeze do not conflict, because `!is_terminal()` is
/// a property of a file descriptor and not a mode. This branch serves a
/// PIPED-but-ANSWERED session (the eval harness, `printf … | newt solve`)
/// where a writer is present and an answer is coming; the epic's criterion
/// governs headless/protocol modes, which never construct a `PromptWindow`
/// at all and are served by `crate::markup::headless`. The same C0 bullet
/// that carries the criterion also says to *preserve pipe behavior*, which
/// only coheres under that reading.
#[cfg(test)]
mod c0b_wiring {
    /// The production half of this file, with the test modules removed.
    fn production_source() -> String {
        include_str!("modal.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("the production half")
            .to_string()
            + include_str!("modal.rs")
                .split("#[cfg(all(test, unix))]")
                .next()
                .expect("the production half")
    }

    /// The body of `fn <name>`, by brace depth.
    fn function_body(source: &str, name: &str) -> Option<String> {
        let anchor = format!("fn {name}(");
        let start = source.find(&anchor)?;
        let mut depth = 0i32;
        let mut opened = false;
        let mut body = String::new();
        for ch in source[start..].chars() {
            body.push(ch);
            match ch {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
            if opened && depth <= 0 {
                break;
            }
        }
        opened.then_some(body)
    }

    /// **The piped branch still classifies through the frozen convention.**
    #[test]
    fn the_non_tty_branch_still_calls_the_frozen_classifier() {
        let source = production_source();
        let body = function_body(&source, "read_prompt_window_line")
            .expect("`fn read_prompt_window_line` not found at all");
        assert!(
            body.contains("is_terminal()"),
            "the non-TTY branch is gone from the reader: {body}"
        );
        assert!(
            body.contains("classify_headless_prompt_line(&line)"),
            "the piped branch no longer routes through the A0-frozen \
             classifier — the convention `newt solve`-when-piped and the eval \
             harness rely on is silently not applying: {body}"
        );
        // The EOF arm stays upstream of the classifier, which is what makes
        // an explicitly-submitted empty line distinguishable from Ctrl-D.
        assert!(
            body.contains("PromptLine::Eof"),
            "the Ok(0) => Eof arm left the reader: {body}"
        );
    }

    /// **Anti-vacuous twin.** The extractor must fail on a branch that
    /// stopped calling the classifier, or the guard above passes on any
    /// file it cannot parse.
    #[test]
    fn the_wiring_guard_notices_a_branch_that_stopped_classifying() {
        let rewired = "fn read_prompt_window_line(w: &W, p: &str) -> R {\n\
                       if !io::stdin().is_terminal() {\n\
                           return Ok(PromptLine::Line(line));\n\
                       }\n\
                       }";
        let body = function_body(rewired, "read_prompt_window_line").expect("body");
        assert!(
            !body.contains("classify_headless_prompt_line(&line)"),
            "the twin's fixture already classifies, so it proves nothing"
        );
        // ...and the real one does, so the guard discriminates.
        let real = production_source();
        assert!(function_body(&real, "read_prompt_window_line")
            .expect("body")
            .contains("classify_headless_prompt_line(&line)"));
    }
}

#[cfg(test)]
mod echo_policy {
    use super::Echo;

    /// A masked read shows one `*` per CHARACTER, never per byte.
    ///
    /// Byte-counting would leak the key's length — a 20-character key with
    /// a multi-byte character in it would draw more stars than it has
    /// characters, and the difference is information about the secret.
    #[test]
    fn stars_mask_by_character_and_never_reveal_the_value() {
        assert_eq!(Echo::Stars.display("sk-live-abc"), "***********");
        assert_eq!(Echo::Stars.display(""), "");
        // Multi-byte: 3 chars, 3 stars — not 7 bytes, 7 stars.
        assert_eq!(Echo::Stars.display("é☃x"), "***");
        // The value itself never appears in what is displayed.
        for secret in ["sk-live-abc", "é☃x", "hunter2"] {
            let shown = Echo::Stars.display(secret);
            assert!(
                !shown.contains(secret),
                "the value leaked into the masked display: {shown:?}"
            );
        }
    }

    /// **The anti-vacuous twin.** If `display` always returned stars — or
    /// always returned the empty string — the assertions above would pass
    /// while the ordinary prompt drew nothing. `Chars` must be verbatim.
    #[test]
    fn chars_show_the_value_verbatim() {
        assert_eq!(Echo::Chars.display("sk-live-abc"), "sk-live-abc");
        assert_eq!(Echo::Chars.display("é☃x"), "é☃x");
        assert_eq!(Echo::Chars.display(""), "");
        assert_ne!(
            Echo::Chars.display("hunter2"),
            Echo::Stars.display("hunter2"),
            "the two policies must actually differ"
        );
    }
}

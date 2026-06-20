//! Rich inline-TUI input surface (issue #416) — behind the `rich-tui` feature.
//!
//! This is the production port of `examples/rich_tui_spike.rs`, reshaped to the
//! [`InputSurface`](crate::InputSurface) contract so `run_chat` can drive it the
//! same way it drives the lean surface. It renders a ratatui
//! `Viewport::Inline` region pinned to the bottom of the terminal — **no
//! alternate screen** — so submitted turns and model output flow into real
//! scrollback. On a TTY (and only when the `rich-tui` feature is compiled in) it
//! replaces the lean surface; everywhere else (piped, headless, wyvern) the
//! lean crossterm surface handles input.
//!
//! ## Submit semantics (parity with the lean path)
//! - **Enter** submits — unless the line is mid-continuation
//!   ([`footer_continues`](crate::footer_continues): a `! …\` host-shell line or
//!   an open `"""`/`'''` block), in which case Enter adds a line. This reuses
//!   the *exact* continuation classifier, so multi-line entry behaves
//!   identically across both surfaces.
//! - **Shift-Enter** inserts a newline in every mode (terminal permitting).
//!   **Ctrl-O** inserts a newline only in the **modeless** modes (emacs/nano),
//!   where it is idiomatic (emacs `open-line`). In **vi**, Ctrl-O is left free
//!   for its real semantics (jumplist back in NORMAL, insert-normal in INSERT);
//!   vi users open lines with `o`/`O`.
//! - **Ctrl-C** abandons the current line (clears it, stays in the session — a
//!   shell-like "give me a clean line", NOT an exit). **Exit** is mode-idiomatic:
//!   `C-x C-c` (emacs), `^X` (nano), `:q`/`:wq` (vi), `Ctrl-D` on an empty
//!   buffer, or typing `/exit`.
//! - Vi ex-commands: `:w` submits (= Enter); `:wq`/`:x` submit, run the turn,
//!   then **end the conversation and quit** (behind a `[y/N]` confirm; the `!`
//!   forms skip it); `:q`/`:q!` quit without sending.
//!
//! ## Vi gaps (future faithful-keymap work, issue #416 follow-up)
//! - **`.` repeat** (replay the last change, incl. insert sessions) — not yet.
//! - **`df{c}` / `dt{c}`** (char-search as an operator target) — `f/F/t/T` work
//!   as standalone motions, but not yet after an operator.
//! - **`R` overwrite**, exact **`P`**, operator counts (`d2w`), big-word
//!   distinction (`W`/`B`/`E` alias the small-word motions today).
//!
//! ## Not yet (documented limitations of v1)
//! - No in-session history recall (Up/Down navigate the buffer, not history);
//!   submitted entries are still **persisted** to the shared history file so the
//!   lean path sees them next session.
//! - The status row shows the live clock + edit mode only; model / plan-mode
//!   tokens land with the status-row work (issue #416 follow-up).
//! - The per-turn event loop (`read_line`) needs a real TTY and is exercised by
//!   creature-testing, not unit tests; the editing/state logic below is fully
//!   unit-tested.

use std::cell::Cell;
use std::io::{self, Stdout, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use tui_textarea::{CursorMove, TextArea};

use crate::{footer_continues, InputSurface, ReadOutcome};

// Opt-in wide-gutter width (`NEWT_GUTTER=auto`/`tui.gutter=N`): a fixed left
// column for the input-row indicator. Since #527 the clock/mode/model live on the
// status header row, so the gutter only carries `❯`/`:` — this stays a generous
// fixed width for the opt-in aligned layout; the default is the 1-col overhang.
const GUTTER_W: u16 = 19;
const MAX_INPUT_ROWS: u16 = 8;
/// Auto-gutter threshold: use the left gutter only while it stays under this
/// fraction of the terminal width; on a squished terminal, drop it and stack
/// the prompt on its own line. (A `[tui] gutter = auto|on|off` setting later.)
const GUTTER_MAX_FRACTION: f32 = 0.33;

type Term = Terminal<CrosstermBackend<Stdout>>;

fn use_gutter(width: u16) -> bool {
    width > 0 && (GUTTER_W as f32) <= GUTTER_MAX_FRACTION * width as f32
}

/// Resolve the effective gutter / input-indent width (columns) from the
/// `[tui] gutter` setting and the terminal width:
/// - `None` (auto): a prompt-width gutter (`GUTTER_W`) when it stays under ~1/3
///   of the width, else `0` (stacked prompt).
/// - `Some(n)`: exactly `n`, clamped so it can't consume the whole line.
///
/// A result `>= GUTTER_W` is wide enough to hold the inline prompt; a smaller
/// result (including `0`) stacks the prompt on its own row and indents the input
/// that many columns.
fn resolve_gutter(setting: Option<u16>, width: u16) -> u16 {
    match setting {
        None => {
            if use_gutter(width) {
                GUTTER_W
            } else {
                0
            }
        }
        Some(n) => n.min(width.saturating_sub(1)),
    }
}

/// One step of the editor: what the loop should do after handling a key.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Step {
    /// Keep editing.
    Continue,
    /// Accept the buffer as this turn's input.
    Submit,
    /// vi `:wq`/`:x` (confirmed) — submit this turn, run it to completion, then
    /// END the conversation and exit (the next launch starts fresh). Distinct
    /// from [`Step::Submit`] (stay) and [`Step::Eof`] (suspend & resume later).
    SubmitQuit,
    /// End of input — clean exit (Ctrl-D on empty, `:q`, `C-x C-c`, nano `^X`).
    Eof,
}

/// A pending `[y/N]` confirmation in the vi `:`-line. Today only `:wq`/`:x`
/// arms one ("send prompt then quit?"); modeled as an enum so other
/// destructive ex-commands can reuse the same gate.
#[derive(Clone, Copy, PartialEq)]
enum Confirm {
    /// `:wq` / `:x` — submit + end-conversation + quit, pending a `y`.
    SubmitQuit,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Insert,
}

#[derive(Clone, Copy, PartialEq)]
enum Pending {
    None,
    Op(char),
    Replace,
    G,
    /// Awaiting the target char of an `f`/`F`/`t`/`T` char-search (the stored
    /// char is the search kind).
    Find(char),
}

/// The vi state machine — a faithful subset of rustyline's `vi_command`, ported
/// onto tui-textarea. NORMAL/INSERT · `h l j k w b e 0 ^ $ G gg` (counts) ·
/// `f F t T` char-search + `; ,` repeat · `i I a A o O` ·
/// `x X D C s S r{c} u Ctrl-R p J` · `d/c/y{motion}` + `dd/cc/yy` ·
/// Ctrl-O/Ctrl-I jumplist + `:jumps` · i_CTRL-O insert-normal.
struct Vi {
    mode: Mode,
    pending: Pending,
    count: usize,
    /// `:`-command line buffer (`:wq`, `:q`, …); `Some` while active.
    ex: Option<String>,
    /// A pending `[y/N]` confirmation (e.g. `:wq` → "send prompt then quit?").
    /// While `Some`, the next key is the answer — `y`/`Y` confirms, anything
    /// else cancels back to NORMAL editing.
    confirm: Option<Confirm>,
    /// i_CTRL-O: run exactly one Normal command from INSERT, then resume INSERT.
    insert_normal: bool,
    /// Jumplist: positions we jumped *from*, older toward the front of `jback`;
    /// `jfwd` holds positions undone by Ctrl-O so Ctrl-I can redo them. Browser
    /// back/forward model. Each entry is a `(row, col)` cursor position.
    jback: Vec<(usize, usize)>,
    jfwd: Vec<(usize, usize)>,
    /// A one-shot message to print to scrollback (e.g. `:jumps` output).
    msg: Option<String>,
    /// Last `f`/`F`/`t`/`T` search as `(kind, target)`, for `;` (repeat) and
    /// `,` (repeat reversed).
    last_find: Option<(char, char)>,
}

impl Vi {
    fn new() -> Self {
        Self {
            mode: Mode::Insert,
            pending: Pending::None,
            count: 0,
            ex: None,
            confirm: None,
            insert_normal: false,
            jback: Vec::new(),
            jfwd: Vec::new(),
            msg: None,
            last_find: None,
        }
    }

    fn take_count(&mut self) -> usize {
        let n = self.count.max(1);
        self.count = 0;
        n
    }

    /// Record the current cursor position as a jump origin (and drop the forward
    /// history) — called just before a "far" motion (`gg`, `G`).
    fn record_jump(&mut self, ta: &TextArea) {
        self.jback.push(ta.cursor());
        self.jfwd.clear();
    }

    /// Ctrl-O — jump to an older position (jumplist back).
    fn jump_back(&mut self, ta: &mut TextArea) {
        if let Some(prev) = self.jback.pop() {
            self.jfwd.push(ta.cursor());
            ta.move_cursor(CursorMove::Jump(prev.0 as u16, prev.1 as u16));
        }
    }

    /// Ctrl-I / Tab — jump to a newer position (jumplist forward).
    fn jump_forward(&mut self, ta: &mut TextArea) {
        if let Some(next) = self.jfwd.pop() {
            self.jback.push(ta.cursor());
            ta.move_cursor(CursorMove::Jump(next.0 as u16, next.1 as u16));
        }
    }

    /// Render the jumplist for `:jumps` (1-based row:col, like vim's line:col).
    fn format_jumps(&self) -> String {
        let fmt = |v: &[(usize, usize)]| {
            if v.is_empty() {
                "—".to_string()
            } else {
                v.iter()
                    .map(|(r, c)| format!("{}:{}", r + 1, c + 1))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        };
        format!(
            "jumps  back: {}  forward: {}",
            fmt(&self.jback),
            fmt(&self.jfwd)
        )
    }

    /// The `:`-command line. `:w` submits (= Enter); `:wq`/`:x` submit-then-end
    /// the conversation and quit, behind a `[y/N]` confirm (the `!` forms skip
    /// the prompt); `:q`/`:q!` quit. Esc or backspacing past the `:` cancels.
    fn ex_input(&mut self, key: KeyEvent) -> Step {
        match key.code {
            KeyCode::Esc => self.ex = None,
            KeyCode::Enter => {
                let cmd = self.ex.take().unwrap_or_default();
                match cmd.as_str() {
                    // `:w` = write = submit, same as Enter (vi muscle memory).
                    "w" => return Step::Submit,
                    // `:wq`/`:x` = send, run to completion, then end+quit — but
                    // that combination is destructive, so confirm first.
                    "wq" | "x" => {
                        self.confirm = Some(Confirm::SubmitQuit);
                        return Step::Continue;
                    }
                    // `:wq!`/`:x!` — the `!` means "I'm sure": skip the confirm.
                    "wq!" | "x!" => return Step::SubmitQuit,
                    "q" | "q!" => return Step::Eof,
                    "jumps" => self.msg = Some(self.format_jumps()),
                    "help" | "h" => self.msg = Some(help_text(Edit::Vi)),
                    _ => {} // unknown command just cancels
                }
                return Step::Continue;
            }
            KeyCode::Backspace => {
                if let Some(ex) = self.ex.as_mut() {
                    if ex.pop().is_none() {
                        self.ex = None; // backspaced past the `:`
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(ex) = self.ex.as_mut() {
                    ex.push(c);
                }
            }
            _ => {}
        }
        Step::Continue
    }

    /// Answer a pending `[y/N]` confirmation: `y`/`Y` commits the action,
    /// anything else (n/N/Esc/Enter/…) cancels back to NORMAL editing. The
    /// confirm is always cleared.
    fn confirm_input(&mut self, key: KeyEvent, what: Confirm) -> Step {
        self.confirm = None;
        let yes = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
        match what {
            Confirm::SubmitQuit if yes => Step::SubmitQuit,
            _ => Step::Continue,
        }
    }

    fn input(&mut self, key: KeyEvent, ta: &mut TextArea) -> Step {
        if let Some(what) = self.confirm {
            return self.confirm_input(key, what);
        }
        if self.ex.is_some() {
            return self.ex_input(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match self.mode {
            Mode::Insert => {
                // i_CTRL-O: drop to NORMAL for exactly one command, then resume
                // INSERT. Unlike Esc it does NOT shift the cursor back a char.
                if ctrl && key.code == KeyCode::Char('o') {
                    self.mode = Mode::Normal;
                    self.insert_normal = true;
                } else if key.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                    ta.move_cursor(CursorMove::Back);
                } else {
                    ta.input(key);
                }
                Step::Continue
            }
            Mode::Normal => {
                // Esc in NORMAL cancels any incomplete command — a pending
                // operator (`d`/`c`/`y`), char-search (`f`…), `r`/`g`, or a
                // building count — and ends a one-shot i_CTRL-O so we stay in
                // NORMAL. Idle Esc is then a harmless no-op (extra presses just
                // confirm NORMAL), matching vim.
                if key.code == KeyCode::Esc {
                    self.pending = Pending::None;
                    self.count = 0;
                    self.insert_normal = false;
                    return Step::Continue;
                }
                if ctrl && key.code == KeyCode::Char('r') {
                    ta.redo();
                    return Step::Continue;
                }
                // Ctrl-O = jumplist back, Ctrl-I (Tab) = forward.
                if ctrl && key.code == KeyCode::Char('o') {
                    self.jump_back(ta);
                    return Step::Continue;
                }
                if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
                    self.jump_forward(ta);
                    return Step::Continue;
                }
                let step = self.normal(key, ta);
                // i_CTRL-O: once a full command has executed (no operator/count
                // still pending), return to INSERT.
                if self.insert_normal && self.pending == Pending::None && self.count == 0 {
                    self.mode = Mode::Insert;
                    self.insert_normal = false;
                }
                step
            }
        }
    }

    fn normal(&mut self, key: KeyEvent, ta: &mut TextArea) -> Step {
        let KeyCode::Char(c) = key.code else {
            match key.code {
                KeyCode::Left | KeyCode::Backspace => ta.move_cursor(CursorMove::Back),
                KeyCode::Right => ta.move_cursor(CursorMove::Forward),
                KeyCode::Up => ta.move_cursor(CursorMove::Up),
                KeyCode::Down => ta.move_cursor(CursorMove::Down),
                _ => {}
            }
            return Step::Continue;
        };
        match self.pending {
            Pending::Replace => {
                ta.delete_next_char();
                ta.insert_char(c);
                ta.move_cursor(CursorMove::Back);
                self.pending = Pending::None;
                return Step::Continue;
            }
            Pending::G => {
                self.pending = Pending::None;
                if c == 'g' {
                    self.record_jump(ta); // gg is a jump
                    ta.move_cursor(CursorMove::Top);
                }
                return Step::Continue;
            }
            Pending::Op(op) => {
                self.apply_operator(op, c, ta);
                self.pending = Pending::None;
                return Step::Continue;
            }
            Pending::Find(kind) => {
                // `c` is the target char of an f/F/t/T search.
                self.pending = Pending::None;
                self.last_find = Some((kind, c));
                char_search(ta, kind, c);
                return Step::Continue;
            }
            Pending::None => {}
        }
        if c.is_ascii_digit() && !(c == '0' && self.count == 0) {
            self.count = self.count.saturating_mul(10) + (c as usize - '0' as usize);
            return Step::Continue;
        }
        if is_motion(c) {
            let n = self.take_count();
            if c == 'G' {
                self.record_jump(ta); // G is a jump
            }
            apply_motion(ta, c, n);
            return Step::Continue;
        }
        let n = self.take_count();
        match c {
            'i' => self.mode = Mode::Insert,
            'I' => {
                ta.move_cursor(CursorMove::Head);
                self.mode = Mode::Insert;
            }
            'a' => {
                ta.move_cursor(CursorMove::Forward);
                self.mode = Mode::Insert;
            }
            'A' => {
                ta.move_cursor(CursorMove::End);
                self.mode = Mode::Insert;
            }
            'o' => {
                ta.move_cursor(CursorMove::End);
                ta.insert_newline();
                self.mode = Mode::Insert;
            }
            'O' => {
                ta.move_cursor(CursorMove::Head);
                ta.insert_newline();
                ta.move_cursor(CursorMove::Up);
                self.mode = Mode::Insert;
            }
            'x' => {
                for _ in 0..n {
                    ta.delete_next_char();
                }
            }
            'X' => {
                for _ in 0..n {
                    ta.delete_char();
                }
            }
            'D' => {
                ta.delete_line_by_end();
            }
            'C' => {
                ta.delete_line_by_end();
                self.mode = Mode::Insert;
            }
            'J' => {
                // Join the line(s) below onto the current line with a single
                // space. `{count}J` joins `count` lines (min effect: the one
                // line below). No-op on the last line (nothing below to join).
                let joins = n.saturating_sub(1).max(1);
                for _ in 0..joins {
                    if ta.cursor().0 + 1 >= ta.lines().len() {
                        break;
                    }
                    ta.move_cursor(CursorMove::End);
                    ta.insert_char(' ');
                    ta.delete_next_char(); // remove the line break → pull next line up
                }
            }
            's' => {
                for _ in 0..n {
                    ta.delete_next_char();
                }
                self.mode = Mode::Insert;
            }
            'S' => {
                ta.move_cursor(CursorMove::Head);
                ta.delete_line_by_end();
                self.mode = Mode::Insert;
            }
            'r' => self.pending = Pending::Replace,
            'u' => {
                ta.undo();
            }
            'p' | 'P' => {
                ta.paste();
            }
            'd' | 'c' | 'y' => self.pending = Pending::Op(c),
            'g' => self.pending = Pending::G,
            // Char-search: `f`/`F`/`t`/`T` wait for a target char (Pending::Find).
            'f' | 'F' | 't' | 'T' => self.pending = Pending::Find(c),
            // `;` repeats the last f/F/t/T; `,` repeats it reversed.
            ';' => {
                if let Some((kind, target)) = self.last_find {
                    char_search(ta, kind, target);
                }
            }
            ',' => {
                if let Some((kind, target)) = self.last_find {
                    char_search(ta, reverse_find(kind), target);
                }
            }
            ':' => self.ex = Some(String::new()),
            // #530: an unbound NORMAL key (e.g. `q`) used to do nothing at all,
            // leaving a user who didn't realise the prompt is modal stuck with
            // no feedback. Nudge them toward INSERT.
            _ => {
                self.msg = Some("vi NORMAL — press i to insert · :help for commands".to_string());
            }
        }
        Step::Continue
    }

    fn apply_operator(&mut self, op: char, target: char, ta: &mut TextArea) {
        if target == op {
            match op {
                'c' => {
                    ta.move_cursor(CursorMove::Head);
                    ta.delete_line_by_end();
                    self.mode = Mode::Insert;
                }
                'd' => {
                    ta.move_cursor(CursorMove::Head);
                    ta.delete_line_by_end();
                    ta.delete_next_char();
                }
                'y' => {
                    let start = ta.cursor();
                    ta.move_cursor(CursorMove::Head);
                    ta.start_selection();
                    ta.move_cursor(CursorMove::End);
                    ta.copy();
                    ta.cancel_selection();
                    ta.move_cursor(CursorMove::Jump(start.0 as u16, start.1 as u16));
                }
                _ => {}
            }
            return;
        }
        if !is_motion(target) {
            return;
        }
        let start = ta.cursor();
        ta.start_selection();
        apply_motion(ta, target, 1);
        match op {
            'd' => {
                ta.cut();
            }
            'c' => {
                ta.cut();
                self.mode = Mode::Insert;
            }
            'y' => {
                ta.copy();
                ta.cancel_selection();
                ta.move_cursor(CursorMove::Jump(start.0 as u16, start.1 as u16));
            }
            _ => ta.cancel_selection(),
        }
    }

    /// Status label — `vi` lit up plus a one-letter mode (`N`/`I`), so a vi user
    /// always knows the surface is modal and which mode they're in, without the
    /// long `NORMAL`/`INSERT` words widening the gutter for every mode.
    /// Short mode label (`vi N` / `vi I`) — used by tests to assert mode flips;
    /// the live status row shows the mode via its indicator/hint instead.
    #[cfg(test)]
    fn mode_label(&self) -> &'static str {
        match self.mode {
            Mode::Normal => "vi N",
            Mode::Insert => "vi I",
        }
    }
}

fn is_motion(c: char) -> bool {
    matches!(
        c,
        'h' | 'l' | 'j' | 'k' | 'w' | 'W' | 'b' | 'B' | 'e' | 'E' | '0' | '^' | '$' | 'G'
    )
}

fn apply_motion(ta: &mut TextArea, c: char, n: usize) {
    let mv = match c {
        'h' => CursorMove::Back,
        'l' => CursorMove::Forward,
        'j' => CursorMove::Down,
        'k' => CursorMove::Up,
        'w' | 'W' => CursorMove::WordForward,
        'b' | 'B' => CursorMove::WordBack,
        'e' | 'E' => CursorMove::WordEnd,
        '0' | '^' => CursorMove::Head,
        '$' => CursorMove::End,
        'G' => CursorMove::Bottom,
        _ => return,
    };
    for _ in 0..n {
        ta.move_cursor(mv);
    }
}

/// Move the cursor by an `f`/`F`/`t`/`T` char-search on the current line:
/// `f` lands on the next `target`, `t` just before it; `F` lands on the previous
/// `target`, `T` just after it. No move if the target isn't found on the line.
fn char_search(ta: &mut TextArea, kind: char, target: char) {
    let (row, col) = ta.cursor();
    let chars: Vec<char> = ta.lines()[row].chars().collect();
    let dest = match kind {
        'f' => (col + 1..chars.len()).find(|&i| chars[i] == target),
        't' => (col + 1..chars.len())
            .find(|&i| chars[i] == target)
            .map(|i| i - 1),
        'F' => (0..col).rev().find(|&i| chars[i] == target),
        'T' => (0..col).rev().find(|&i| chars[i] == target).map(|i| i + 1),
        _ => None,
    };
    if let Some(i) = dest {
        ta.move_cursor(CursorMove::Jump(row as u16, i as u16));
    }
}

/// The opposite search kind, for `,` (repeat reversed): `f`↔`F`, `t`↔`T`.
fn reverse_find(kind: char) -> char {
    match kind {
        'f' => 'F',
        'F' => 'f',
        't' => 'T',
        'T' => 't',
        other => other,
    }
}

/// A compact, mode-specific cheatsheet printed into scrollback by the
/// mode-idiomatic help key (nano `^G`, emacs `Ctrl-h`, vi `:help`). Kept to a
/// few short lines so it stays legible in narrow tmux/ssh panes — this is
/// scrollback output, not a pager (a full scrollable help viewer is a separate
/// surface decision).
fn help_text(edit: Edit) -> String {
    match edit {
        Edit::Vi => [
            "vi  Esc=NORMAL · i I a A o O=insert · :w=send · :wq=send+end · :q=quit · :jumps :help",
            "    move: h j k l · w b e · 0 ^ $ · gg G · f F t T ; , · Ctrl-O/Ctrl-I jumps",
            "    edit: x X · dd dw D C s S r · yy p · J join · u Ctrl-R · d/c/y+motion",
        ]
        .join("\n"),
        Edit::Nano => {
            "nano  Enter=submit · Ctrl-O or Shift-Enter=newline · ^X=exit · ^G=help".to_string()
        }
        Edit::Emacs => {
            "emacs  Enter=submit · Ctrl-O or Shift-Enter=newline · C-x C-c=exit · Ctrl-h=help"
                .to_string()
        }
    }
}

/// The editor mode. **Default is Nano** (modeless, tui-textarea's native
/// emacs-style bindings — the most approachable); Emacs is the same bindings
/// under a different label, and Vi is opt-in via `[tui] edit_mode` / `/vi`. Read
/// from the shared edit-mode source ([`crate::resolve_edit_mode`]).
#[derive(Clone, Copy, PartialEq)]
enum Edit {
    Emacs,
    Nano,
    Vi,
}

impl Edit {
    /// Whether this mode uses tui-textarea's native (modeless, emacs-style)
    /// bindings — true for both Emacs and Nano.
    fn is_modeless(self) -> bool {
        matches!(self, Self::Emacs | Self::Nano)
    }
}

fn current_edit() -> Edit {
    match crate::resolve_edit_mode() {
        newt_core::EditMode::Vi => Edit::Vi,
        newt_core::EditMode::Nano => Edit::Nano,
        newt_core::EditMode::Emacs => Edit::Emacs,
    }
}

/// Per-turn editor: the edit mode plus (for vi) the mode/operator state.
struct Editor {
    edit: Edit,
    vi: Vi,
    /// emacs `C-x` prefix is armed, awaiting the second key (`C-c` to quit).
    cx_pending: bool,
}

impl Editor {
    fn new(edit: Edit) -> Self {
        Self {
            edit,
            vi: Vi::new(),
            cx_pending: false,
        }
    }

    /// Handle one key. Submit / interrupt / EOF and the continuation-aware Enter
    /// are shared by both modes via the [`crate::footer_continues`] classifier.
    fn input(&mut self, key: KeyEvent, ta: &mut TextArea) -> Step {
        // A pending vi `[y/N]` confirmation (e.g. `:wq`) owns the next key
        // outright — it must run BEFORE the shared Enter/Ctrl handling, or
        // Enter would submit and Ctrl-C would clear the line instead of
        // answering the prompt.
        if self.edit == Edit::Vi && self.vi.confirm.is_some() {
            return self.vi.input(key, ta);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Mode-idiomatic exit. emacs: `C-x C-c` (the `C-x` prefix is armed here
        // and must be checked BEFORE the bare `Ctrl-C` interrupt below). nano:
        // `^X` exits directly. vi uses `:q`/`:wq`.
        if self.cx_pending {
            self.cx_pending = false;
            if ctrl && key.code == KeyCode::Char('c') {
                return Step::Eof;
            }
            // Any other key cancels the prefix and is handled normally below.
        }
        if ctrl && key.code == KeyCode::Char('x') {
            match self.edit {
                Edit::Emacs => {
                    self.cx_pending = true;
                    return Step::Continue;
                }
                Edit::Nano => return Step::Eof,
                Edit::Vi => {}
            }
        }
        if ctrl {
            match key.code {
                // Ctrl-C at the prompt abandons the current line (clear it, fresh
                // start) and stays in the session — it does NOT exit. During a
                // turn Ctrl-C interrupts (see `watch_for_interrupt`); exit is
                // Ctrl-D / `:q` / `/exit`. The hint makes the mapping discoverable
                // (repeated Ctrl-C just re-shows it). Matches a shell's "^C gives
                // me a clean line" reflex.
                KeyCode::Char('c') => {
                    *ta = new_textarea(self.edit);
                    self.vi = Vi::new();
                    self.cx_pending = false;
                    self.vi.msg = Some("Ctrl-C to interrupt · Ctrl-D to exit".to_string());
                    return Step::Continue;
                }
                KeyCode::Char('d') => {
                    return if buffer_is_empty(ta) {
                        Step::Eof
                    } else {
                        Step::Submit
                    };
                }
                // Ctrl-O inserts a newline ONLY in the modeless modes
                // (emacs/nano) — in emacs this is idiomatic (`open-line`). In
                // vi, Ctrl-O is the jumplist "jump back" command (Ctrl-I forward,
                // `:jumps` to view), so we must NOT hijack it; vi users open
                // lines with `o`/`O` (and Shift-Enter still works). The jumplist
                // itself is a documented vi gap (TODO), so Ctrl-O is a no-op in
                // vi for now rather than wrongly inserting a newline.
                KeyCode::Char('o') if self.edit.is_modeless() => {
                    ta.insert_newline();
                    return Step::Continue;
                }
                // Mode-idiomatic help → print a cheatsheet to scrollback. nano
                // uses `^G`, emacs uses `Ctrl-h` (terminal permitting: some send
                // Backspace for Ctrl-h, in which case the hint key just no-ops).
                KeyCode::Char('g') if self.edit == Edit::Nano => {
                    self.vi.msg = Some(help_text(self.edit));
                    return Step::Continue;
                }
                KeyCode::Char('h') if self.edit == Edit::Emacs => {
                    self.vi.msg = Some(help_text(self.edit));
                    return Step::Continue;
                }
                _ => {}
            }
        }
        // Shift-Enter — explicit newline without submitting (terminal
        // permitting: many terminals send a bare CR, indistinguishable from
        // Enter, so Ctrl-O is the reliable fallback). Shared across all edit
        // modes since this runs before the per-mode dispatch.
        //
        // Ctrl-Enter is deliberately NOT bound: on macOS terminals Ctrl-Return
        // is intercepted at the terminal/OS layer (it opens a popup) and never
        // reaches us cleanly, so it is unusable cross-platform. Ctrl-O is the
        // portable newline key.
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
            ta.insert_newline();
            return Step::Continue;
        }
        // Plain Enter: submit, unless mid-continuation, and not while a vi
        // `:`-command line is being typed (that Enter executes the command).
        if key.code == KeyCode::Enter && self.ex().is_none() {
            let body = ta.lines().join("\n");
            if footer_continues(&body) {
                ta.insert_newline();
                return Step::Continue;
            }
            return Step::Submit;
        }
        if self.edit.is_modeless() {
            // Emacs / nano: hand the key to tui-textarea's built-in bindings.
            ta.input(key);
            return Step::Continue;
        }
        self.vi.input(key, ta)
    }

    /// Short mode label — test-only mode introspection; the live status row
    /// shows the mode through its `❯`/`:` indicator + dim hint.
    #[cfg(test)]
    fn label(&self) -> &'static str {
        match self.edit {
            Edit::Emacs => "emacs",
            Edit::Nano => "nano",
            Edit::Vi => self.vi.mode_label(),
        }
    }

    fn ex(&self) -> Option<&str> {
        if self.edit == Edit::Vi {
            self.vi.ex.as_deref()
        } else {
            None
        }
    }

    /// In vi NORMAL mode the prompt indicator is a highlighted `:` instead of
    /// `❯`. False for emacs/nano and for vi INSERT.
    fn is_vi_normal(&self) -> bool {
        self.edit == Edit::Vi && self.vi.mode == Mode::Normal
    }

    /// The dim, clears-on-type hint shown on an empty line — the editor mode plus
    /// how to switch. vi is the default; `/nano` and `/emacs` are advertised.
    fn mode_hint(&self) -> &'static str {
        match self.edit {
            Edit::Vi => match self.vi.mode {
                Mode::Insert => {
                    "vi INSERT — Esc: NORMAL · :help · /nano /emacs · ^C interrupt · ^D exit"
                }
                Mode::Normal => {
                    "vi NORMAL — i: insert · :cmd · /nano /emacs · ^C interrupt · ^D exit"
                }
            },
            Edit::Emacs => "emacs — Enter sends · Ctrl-h help · /vi /nano · ^C interrupt · ^D exit",
            Edit::Nano => "nano — Enter sends · ^G help · /vi /emacs · ^C interrupt · ^D exit",
        }
    }

    /// The status-header mode word (issue #527): `vi --INSERT--` / `vi --NORMAL--`
    /// for vi, else the bare editor name. Reflects the LIVE mode — the surface
    /// redraws every frame, so the header tracks Esc/i with no extra machinery.
    fn header_mode(&self) -> &'static str {
        match self.edit {
            Edit::Emacs => "emacs",
            Edit::Nano => "nano",
            Edit::Vi => match self.vi.mode {
                Mode::Insert => "vi --INSERT--",
                Mode::Normal => "vi --NORMAL--",
            },
        }
    }

    /// The `[y/N]` question to render while a confirmation is pending (vi only),
    /// e.g. after `:wq`. `None` when nothing is awaiting an answer.
    fn confirm_prompt(&self) -> Option<&'static str> {
        if self.edit != Edit::Vi {
            return None;
        }
        match self.vi.confirm {
            Some(Confirm::SubmitQuit) => Some("send prompt then quit? [y/N] "),
            None => None,
        }
    }

    /// Take a one-shot message to print to scrollback (e.g. `:jumps` output).
    fn take_msg(&mut self) -> Option<String> {
        self.vi.msg.take()
    }
}

fn buffer_is_empty(ta: &TextArea) -> bool {
    ta.lines().iter().all(|l| l.is_empty())
}

fn make_terminal(height: u16) -> io::Result<Term> {
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
}

fn new_textarea(edit: Edit) -> TextArea<'static> {
    let mut ta = TextArea::default();
    // Mode-aware hint, including the mode-idiomatic help key. In vi, Ctrl-O is
    // reserved (jumplist / insert-normal), so we advertise vi-native `o`/`O`.
    let hint = match edit {
        Edit::Vi => "type…  (Esc=NORMAL · o/O open line · Enter submit · :help · :q quit)",
        Edit::Nano => "type…  (Enter submit · Ctrl-O/Shift-Enter newline · ^G help · ^X exit)",
        Edit::Emacs => {
            "type…  (Enter submit · Ctrl-O/Shift-Enter newline · Ctrl-h help · C-x C-c exit)"
        }
    };
    ta.set_placeholder_text(hint);
    // Block (reverse) cursor; no cursor-line underline.
    ta.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    ta.set_cursor_line_style(Style::default());
    ta
}

/// A fresh editor textarea pre-filled with `content`, cursor at the end — used
/// when recalling a history entry into the input.
fn textarea_with(edit: Edit, content: &str) -> TextArea<'static> {
    let mut ta = new_textarea(edit);
    ta.insert_str(content);
    ta
}

/// Pure ↑/↓ history step. `pos == len` is the fresh (un-recalled) line; `up`
/// walks toward older entries (index 0), `down` back toward the fresh line.
/// `None` when already at the edge (nothing to move to).
fn history_step(pos: usize, len: usize, up: bool) -> Option<usize> {
    if up {
        (pos > 0).then(|| pos - 1)
    } else {
        (pos < len).then(|| pos + 1)
    }
}

/// Load prior input lines for ↑/↓ recall: the on-disk history file (one entry
/// per line, oldest first), in the rustyline-compatible format `save_history`
/// writes. Blank lines are skipped. Missing/unreadable file → empty.
fn load_history(path: Option<&PathBuf>) -> Vec<String> {
    let Some(p) = path else {
        return Vec::new();
    };
    std::fs::read_to_string(p)
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The `[options]` block for the status row: session overrides the operator set
/// — `--loadout`/`NEWT_LOADOUT` and `/model`/`NEWT_DGX_MODEL`. `None` when none
/// is active, so the bracket is omitted entirely (no empty `[]` by default).
fn status_options() -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for var in ["NEWT_LOADOUT", "NEWT_DGX_MODEL"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                parts.push(v);
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// The rich surface's two-line live view (issue #527): a status HEADER row
/// (`[header_line`]) over an input-indicator row ([`prompt_line`]). The PS1 token
/// prompt (`[tui] prompt`) is the LEAN surface's job (it lands in logfiles); the
/// rich surface renders these instead.
///
/// The header is `[YYYY-MM-DD HH:MM:SS] vi --INSERT-- <model> @ <endpoint>` plus
/// an optional `[options]` session-override block. The clock + editor mode update
/// live (the event loop redraws every frame). `active` (the line has content)
/// brightens the mode word; colors favor light/high-luminance tones (the
/// accessibility default).
fn header_line(editor: &Editor, model: &str, endpoint: &str, active: bool) -> Line<'static> {
    let accent = Color::Rgb(255, 165, 90);
    let dim = Color::DarkGray;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut spans: Vec<Span> = vec![Span::styled(format!("[{stamp}]"), Style::default().fg(dim))];
    spans.push(Span::styled(
        format!(" {}", editor.header_mode()),
        Style::default().fg(if active { accent } else { dim }),
    ));
    if !model.is_empty() {
        let loc = if endpoint.is_empty() {
            model.to_string()
        } else {
            format!("{model} @ {endpoint}")
        };
        spans.push(Span::styled(format!(" {loc}"), Style::default().fg(dim)));
    }
    if let Some(opts) = status_options() {
        spans.push(Span::styled(format!(" [{opts}]"), Style::default().fg(dim)));
    }
    Line::from(spans)
}

/// The input-row indicator (below [`header_line`]): `❯ ` for input/INSERT, a bold
/// bright `: ` for vi NORMAL, `:cmd` for an inline ex-line, or the `[y/N]`
/// confirmation. `ex_inline` is true on a single-line buffer (the `:command`
/// shows here); on a multi-line buffer it's false and the command moves to its
/// own bottom row (#531/#540, see `draw`). The input begins right after the
/// marker, so the cursor anchors here and the dim mode hint (drawn by
/// `draw_overhang` on an empty line) sits after it.
fn prompt_line(editor: &Editor, ex_inline: bool) -> Line<'static> {
    if let Some(question) = editor.confirm_prompt() {
        // A pending [y/N] confirmation replaces the input-row prompt until answered.
        return Line::from(Span::styled(
            question.to_string(),
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let accent = Color::Rgb(255, 165, 90);
    let bold_hi = Style::default()
        .fg(Color::LightYellow)
        .add_modifier(Modifier::BOLD);
    // An open ex-line (`:command`) on a single-line buffer renders inline here
    // (the highlighted `:` IS the mode marker). On a multi-line buffer
    // `ex_inline` is false and the command moves to its own bottom row (#540).
    if ex_inline {
        if let Some(ex) = editor.ex() {
            return Line::from(Span::styled(format!(":{ex}"), bold_hi));
        }
    }
    if editor.is_vi_normal() {
        Line::from(Span::styled(": ", bold_hi))
    } else {
        Line::from(Span::styled("❯ ", Style::default().fg(accent)))
    }
}

/// The `:`-command text to render on a dedicated **bottom** row (vi-style) —
/// only when an ex command is open AND the buffer spans multiple lines (#540).
/// On a single line the inline chevron↔`:` swap is kept (see `prompt_line`).
fn ex_bottom_line(editor: &Editor, textarea: &TextArea) -> Option<String> {
    editor
        .ex()
        .filter(|_| textarea.lines().len() > 1)
        .map(str::to_string)
}

fn draw(
    f: &mut Frame,
    textarea: &TextArea,
    editor: &Editor,
    gutter: Option<u16>,
    model: &str,
    endpoint: &str,
) {
    let area = f.area();
    // "active" = the line has content, so the header mode word brightens and the
    // dim mode hint clears as you type. The hint shows only on an empty line.
    let empty = buffer_is_empty(textarea);
    // Two-line layout (#527): the status header on row 0, the input row(s) below.
    let [header_area, input_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    f.render_widget(
        Paragraph::new(header_line(editor, model, endpoint, !empty)),
        header_area,
    );
    let g = resolve_gutter(gutter, input_area.width);

    // #540: a `:`-command on a multi-line buffer renders on its own row at the
    // bottom of the input region, vi-style — not glued to the input row's
    // chevron. The message keeps its normal prompt; the cursor sits on the
    // command line.
    if let Some(ex) = ex_bottom_line(editor, textarea) {
        let [body_area, ex_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(input_area);
        let prompt = prompt_line(editor, false);
        if g >= GUTTER_W {
            let [gutter_area, input] =
                Layout::horizontal([Constraint::Length(g), Constraint::Min(1)]).areas(body_area);
            f.render_widget(Paragraph::new(prompt), gutter_area);
            f.render_widget(textarea, input);
        } else {
            draw_overhang(f, body_area, &prompt, textarea, g, None);
        }
        let bold_hi = Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(":{ex}"), bold_hi))),
            ex_area,
        );
        let cx = ex_area.x + 1 + ex.chars().count() as u16;
        if cx <= ex_area.right().saturating_sub(1) {
            f.set_cursor_position((cx, ex_area.y));
        }
        return;
    }

    let prompt = prompt_line(editor, true);
    let hint = empty.then(|| editor.mode_hint());
    if g >= GUTTER_W {
        // Wide gutter (opt-in): prompt in a fixed left column, input to its
        // right — continuation lines align under the input at column `g`.
        let [gutter_area, input] =
            Layout::horizontal([Constraint::Length(g), Constraint::Min(1)]).areas(input_area);
        f.render_widget(Paragraph::new(prompt), gutter_area);
        f.render_widget(textarea, input);
    } else {
        // Overhang (the default): the prompt prefixes the FIRST input row inline
        // (`❯ this`); continuation rows hang-indent by `g` columns (default 1).
        draw_overhang(f, input_area, &prompt, textarea, g, hint);
    }
}

/// Soft-wrap one logical line into visual segments that fit the input width.
/// `first_w` is the available text width for the first segment (after the prompt
/// or gutter indent), `cont_w` for each wrapped continuation. Breaks at the last
/// space within the width (word wrap), falling back to a hard mid-token break so
/// an unbreakable run still fits. Every char belongs to exactly one segment (the
/// breaking space ends its segment, nothing is dropped) so the cursor maps back
/// cleanly. Each entry is `(char_index_where_the_segment_starts, segment_text)`;
/// there is always at least one segment (empty for an empty line).
fn wrap_segments(text: &str, first_w: usize, cont_w: usize) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![(0, String::new())];
    }
    let mut segs: Vec<(usize, String)> = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let avail = if segs.is_empty() { first_w } else { cont_w }.max(1);
        if chars.len() - start <= avail {
            segs.push((start, chars[start..].iter().collect()));
            break;
        }
        let hard_end = start + avail;
        // Prefer breaking just after the last space in range (word wrap); else
        // hard-break at the width. Force ≥1 char of progress.
        let break_at = (start..hard_end)
            .rev()
            .find(|&i| chars[i] == ' ')
            .map(|i| i + 1)
            .unwrap_or(hard_end)
            .max(start + 1);
        segs.push((start, chars[start..break_at].iter().collect()));
        start = break_at;
    }
    segs
}

/// Build the wrapped visual rows for the overhang surface, plus the cursor's
/// position in full-row space `(col, row)`. Row 0 is prefixed by `prompt`;
/// wrapped continuations and later logical lines hang-indent by `g`. Pure
/// (no `Frame`) so the wrap + cursor math is unit-tested; `draw_overhang`
/// renders it (with vertical scroll) and `event_loop` uses the row count to
/// size the inline viewport.
fn overhang_rows(
    prompt: &Line<'static>,
    lines: &[String],
    cursor: (usize, usize),
    g: u16,
    width: u16,
    hint: Option<&str>,
) -> (Vec<Line<'static>>, u16, u16) {
    let pw = prompt.width() as u16;
    let (crow, ccol) = cursor;
    let cont_w = width.saturating_sub(g).max(1) as usize;
    let mut rows: Vec<Line> = Vec::new();
    let (mut cx, mut cy) = (pw, 0u16);
    for (i, text) in lines.iter().enumerate() {
        let first_indent = if i == 0 { pw } else { g };
        let first_w = width.saturating_sub(first_indent).max(1) as usize;
        let segs = wrap_segments(text, first_w, cont_w);
        let n = segs.len();
        for (s, (seg_start, seg_text)) in segs.into_iter().enumerate() {
            let indent = if s == 0 { first_indent } else { g };
            let line = if i == 0 && s == 0 {
                let mut spans = prompt.spans.clone();
                spans.push(Span::raw(seg_text.clone()));
                // The dim mode hint sits AFTER the input on the first row (the
                // line is empty when a hint is shown), so the block cursor —
                // anchored at the prompt end — lands ON the hint's first cell,
                // placeholder-style, instead of after the whole hint.
                if let Some(h) = hint {
                    spans.push(Span::styled(
                        h.to_string(),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                Line::from(spans)
            } else {
                Line::from(vec![
                    Span::raw(" ".repeat(indent as usize)),
                    Span::raw(seg_text.clone()),
                ])
            };
            if i == crow {
                let seg_end = seg_start + seg_text.chars().count();
                let last = s + 1 == n;
                if (ccol >= seg_start && ccol < seg_end) || (last && ccol >= seg_end) {
                    cy = rows.len() as u16;
                    cx = indent + (ccol.saturating_sub(seg_start)) as u16;
                }
            }
            rows.push(line);
        }
    }
    (rows, cx, cy)
}

/// Render the prompt as an inline prefix on row 0, the input flowing after it
/// and **soft-wrapping** within the terminal width; continuation/wrapped rows
/// hang-indent by `g`. tui-textarea can't vary indent per row or soft-wrap, so
/// we render the buffer ourselves as a `Paragraph` and place the block cursor by
/// hand. A vertical scroll keeps the cursor row visible when the wrapped input
/// is taller than the inline region.
fn draw_overhang(
    f: &mut Frame,
    area: Rect,
    prompt: &Line<'static>,
    textarea: &TextArea,
    g: u16,
    hint: Option<&str>,
) {
    let (rows, cx, cy) = overhang_rows(
        prompt,
        textarea.lines(),
        textarea.cursor(),
        g,
        area.width,
        hint,
    );

    // Vertical scroll so the cursor row stays visible past MAX_INPUT_ROWS.
    let last_row = area.height.saturating_sub(1);
    let scroll_y = cy.saturating_sub(last_row);
    f.render_widget(Paragraph::new(rows).scroll((scroll_y, 0)), area);

    // Place the REAL terminal cursor at the input position so it blinks (the
    // terminal's native cursor), sitting on the first hint cell when the line is
    // empty — a placeholder-style cursor rather than a static block tacked on at
    // the end of the dim hint.
    let cur_x = area.x + cx;
    let cur_y = area.y + cy - scroll_y;
    if cur_x <= area.right().saturating_sub(1) && cur_y <= area.bottom().saturating_sub(1) {
        f.set_cursor_position((cur_x, cur_y));
    }
}

/// Emit a submitted turn into real scrollback (above the inline region), so the
/// conversation log shows what the user typed — the inline widget itself is
/// cleared on submit. Continuation lines hang-indent by the SAME gutter the
/// input used (default 1), so a multi-line prompt reads back exactly as typed
/// rather than being re-flowed to a different indent on submit.
fn echo_submitted(terminal: &mut Term, body: &str, gutter: Option<u16>) -> io::Result<()> {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let width = crossterm::terminal::size().map(|(c, _)| c).unwrap_or(80);
    let hang = " ".repeat(resolve_gutter(gutter, width) as usize);
    let n = body.lines().count().max(1) as u16;
    let dim = Style::default().fg(Color::DarkGray);
    // Two-line committed form (#527): the full-datetime header on its own row,
    // then the body behind a DIMMED `›` — the live `❯` frozen into an at-rest
    // log marker. Header (1) + one row per body line.
    terminal.insert_before(n + 1, |buf| {
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(format!("[{stamp}]"), dim))];
        for (i, l) in body.lines().enumerate() {
            let prefix = if i == 0 {
                Span::styled("›  ", dim)
            } else {
                Span::raw(hang.clone())
            };
            lines.push(Line::from(vec![prefix, Span::raw(l.to_string())]));
        }
        Paragraph::new(lines).render(buf.area, buf);
    })
}

/// Print a note (one or more `\n`-separated lines, e.g. `:jumps`/`:help` output)
/// into scrollback above the input region.
fn echo_note(terminal: &mut Term, note: &str) -> io::Result<()> {
    let rows: Vec<&str> = note.lines().collect();
    let n = rows.len().max(1) as u16;
    terminal.insert_before(n, |buf| {
        let lines: Vec<Line> = rows
            .iter()
            .map(|r| {
                Line::from(Span::styled(
                    r.to_string(),
                    Style::default().fg(Color::Gray),
                ))
            })
            .collect();
        Paragraph::new(lines).render(buf.area, buf);
    })
}

/// The default input surface on a TTY when the `rich-tui` feature is compiled
/// in: a ratatui inline editor implementing [`InputSurface`].
pub(crate) struct RichSurface {
    edit: Edit,
    history_path: Option<PathBuf>,
    /// Entries submitted since the last `save_history`, appended on save.
    unsaved: Vec<String>,
    /// `[tui] gutter` setting: `None` = auto, `Some(0)` = off, `Some(n)` = an
    /// n-column input indent (see [`resolve_gutter`]).
    gutter: Option<u16>,
    /// Armed by a confirmed `:wq`: its turn submits as a normal `Line`, then on
    /// the NEXT `read_line` the surface returns [`ReadOutcome::EndAndQuit`] so
    /// the turn runs to completion before the session ends. A `Cell` because
    /// the event loop runs behind `&self`.
    pending_end_quit: Cell<bool>,
    /// The active model + endpoint shown in the status header (#527), refreshed
    /// each turn via [`InputSurface::set_runtime_context`] so a `/model` switch
    /// is reflected. Empty until the first turn sets them.
    model: String,
    endpoint: String,
}

impl RichSurface {
    pub(crate) fn new(history_path: Option<PathBuf>) -> anyhow::Result<Self> {
        Ok(Self {
            edit: current_edit(),
            history_path,
            unsaved: Vec::new(),
            gutter: crate::resolve_gutter_setting(),
            pending_end_quit: Cell::new(false),
            model: String::new(),
            endpoint: String::new(),
        })
    }

    /// Run the inline event loop for a single turn. Raw mode is enabled for the
    /// duration and disabled before returning, so model output between turns
    /// prints normally into scrollback.
    fn read_turn(&self) -> io::Result<ReadOutcome> {
        enable_raw_mode()?;
        // Bracketed paste: the terminal wraps a paste in escape markers and
        // delivers it as ONE `Event::Paste(text)` instead of a stream of key
        // presses. Without it, a multi-line paste arrives as Char…Enter…Char…
        // and every embedded Enter submits a line. See the `Event::Paste` arm.
        let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
        let outcome = self.event_loop();
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        outcome
    }

    fn event_loop(&self) -> io::Result<ReadOutcome> {
        let mut cur_h = 1u16;
        // A freshly built inline terminal has a blank back-buffer, so ratatui's
        // frame diff won't rewrite cells the new frame doesn't touch — stale
        // content from a prior turn (or the smaller pre-resize region) bleeds
        // through. `clear()` forces a full repaint of the region so every turn /
        // resize starts clean.
        let mut terminal = make_terminal(cur_h)?;
        terminal.clear()?;
        let mut textarea = new_textarea(self.edit);
        let mut editor = Editor::new(self.edit);
        // ↑/↓ history recall (the rustyline behavior the rich surface had
        // dropped): the on-disk history plus this session's not-yet-flushed
        // entries, oldest first. `hist_pos == len` means "the fresh line"; `↑`
        // walks backward into older entries, `↓` forward, restoring the stashed
        // in-progress line at the end.
        let mut history = load_history(self.history_path.as_ref());
        history.extend(self.unsaved.iter().cloned());
        let mut hist_pos = history.len();
        let mut stash = String::new();
        loop {
            // Grow/shrink the inline viewport to the input. The prompt is always
            // inline now — on the first row, either in a wide left gutter or as
            // an overhang prefix — so it never needs a row of its own. The
            // overhang path soft-wraps, so the height is the WRAPPED row count
            // (not the logical-line count); the wide-gutter widget path is one
            // row per logical line.
            let term_w = crossterm::terminal::size().map(|(c, _)| c).unwrap_or(80);
            // #531: a multi-line `:`-command reserves an extra bottom row.
            let ex_extra = u16::from(ex_bottom_line(&editor, &textarea).is_some());
            let rows = if resolve_gutter(self.gutter, term_w) >= GUTTER_W {
                textarea.lines().len() as u16
            } else {
                let empty = buffer_is_empty(&textarea);
                let prompt = prompt_line(&editor, ex_extra == 0);
                overhang_rows(
                    &prompt,
                    textarea.lines(),
                    textarea.cursor(),
                    resolve_gutter(self.gutter, term_w),
                    term_w,
                    empty.then(|| editor.mode_hint()),
                )
                .0
                .len() as u16
            };
            // +ex_extra for a multi-line `:`-command's own bottom row (#540);
            // +1 for the status header row (two-line layout, #527).
            let want = (rows + ex_extra).clamp(1, MAX_INPUT_ROWS) + 1;
            if want != cur_h {
                // Blank the CURRENT region before resizing. ratatui reserves
                // space for a taller inline viewport by scrolling whatever is on
                // screen up into scrollback — which was committing each shorter
                // render as a permanent "ghost" line. Clearing first means only
                // blank rows get scrolled up, so the region grows in place.
                terminal.clear()?;
                terminal = make_terminal(want)?;
                terminal.clear()?;
                cur_h = want;
            }
            terminal.draw(|f| {
                draw(
                    f,
                    &textarea,
                    &editor,
                    self.gutter,
                    &self.model,
                    &self.endpoint,
                );
            })?;

            // 250ms timeout drives the live clock when idle.
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let evt = event::read()?;
            // Bracketed paste: insert the whole block at the cursor — newlines
            // become real line breaks in the buffer, and NOTHING is submitted
            // (only an explicit Enter keypress submits). Normalize CRLF/CR so a
            // paste from any platform lands as clean `\n` lines.
            if let Event::Paste(text) = evt {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                textarea.insert_str(normalized);
                continue;
            }
            let Event::Key(key) = evt else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // History recall on ↑/↓ — but only at a vertical edge of the buffer
            // (top row for ↑, bottom row for ↓) so multi-line cursor movement
            // still works, and never while a `:` ex-line or `[y/N]` confirm is
            // open. Plain arrows only (modified arrows fall through to editing).
            if matches!(key.code, KeyCode::Up | KeyCode::Down)
                && key.modifiers.is_empty()
                && editor.ex().is_none()
                && editor.confirm_prompt().is_none()
                && !history.is_empty()
            {
                let (row, _) = textarea.cursor();
                let last_row = textarea.lines().len().saturating_sub(1);
                let at_edge = (key.code == KeyCode::Up && row == 0)
                    || (key.code == KeyCode::Down && row == last_row);
                if at_edge {
                    let up = key.code == KeyCode::Up;
                    if let Some(next) = history_step(hist_pos, history.len(), up) {
                        // Stash the in-progress line when first leaving it.
                        if hist_pos == history.len() {
                            stash = textarea.lines().join("\n");
                        }
                        hist_pos = next;
                        let content = if hist_pos == history.len() {
                            stash.clone()
                        } else {
                            history[hist_pos].clone()
                        };
                        textarea = textarea_with(self.edit, &content);
                    }
                    continue;
                }
            }
            let step = editor.input(key, &mut textarea);
            // A command (e.g. `:jumps`) may have queued a note to print above the
            // input region, into real scrollback.
            if let Some(note) = editor.take_msg() {
                echo_note(&mut terminal, &note)?;
            }
            match step {
                Step::Continue => {}
                Step::Submit => {
                    let body = textarea.lines().join("\n");
                    if body.trim().is_empty() {
                        continue;
                    }
                    echo_submitted(&mut terminal, &body, self.gutter)?;
                    return Ok(ReadOutcome::Line(body));
                }
                Step::SubmitQuit => {
                    let body = textarea.lines().join("\n");
                    // `:wq` on an empty buffer has nothing to send — treat it
                    // as a plain `:q` (end + quit, no turn).
                    if body.trim().is_empty() {
                        return Ok(ReadOutcome::EndAndQuit);
                    }
                    echo_submitted(&mut terminal, &body, self.gutter)?;
                    // Submit this turn now; the end-and-quit fires on the NEXT
                    // read once the turn has run to completion.
                    self.pending_end_quit.set(true);
                    return Ok(ReadOutcome::Line(body));
                }
                Step::Eof => return Ok(ReadOutcome::Eof),
            }
        }
    }
}

impl InputSurface for RichSurface {
    fn read_line(&mut self, _prompt: &str) -> anyhow::Result<ReadOutcome> {
        // A confirmed `:wq` submitted its turn last time; now that the turn has
        // run, end the conversation and exit before reading anything new.
        if self.pending_end_quit.replace(false) {
            return Ok(ReadOutcome::EndAndQuit);
        }
        // The rich surface always renders its native live status row — it ignores
        // the PS1 token prompt the caller passes (`_prompt`), which is the
        // RUSTYLINE surface's format (it lands in logfiles).
        Ok(self.read_turn()?)
    }

    fn add_history(&mut self, entry: &str) {
        self.unsaved.push(entry.to_string());
    }

    fn save_history(&mut self) {
        let Some(hp) = self.history_path.as_ref() else {
            return;
        };
        if self.unsaved.is_empty() {
            return;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(hp)
        {
            for e in &self.unsaved {
                // Flatten multi-line entries so each history line is one entry,
                // staying compatible with the rustyline history file format.
                let _ = writeln!(f, "{}", e.replace('\n', " "));
            }
        }
        self.unsaved.clear();
    }

    fn reload(&mut self) -> anyhow::Result<()> {
        // A `/vi` · `/emacs` · `/nano` switch changed NEWT_EDIT_MODE; pick it up
        // (and re-read the gutter setting in case it changed too).
        self.edit = current_edit();
        self.gutter = crate::resolve_gutter_setting();
        Ok(())
    }

    fn set_runtime_context(&mut self, model: &str, endpoint: &str) {
        // Refresh the status-header model @ endpoint each turn (#527) so a
        // mid-session `/model` switch shows up on the next prompt.
        self.model = model.to_string();
        self.endpoint = endpoint.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_segments_empty_and_fitting() {
        assert_eq!(wrap_segments("", 10, 10), vec![(0, String::new())]);
        assert_eq!(wrap_segments("hi", 10, 10), vec![(0, "hi".to_string())]);
    }

    #[test]
    fn wrap_segments_breaks_at_spaces_word_wrap() {
        // first_w = cont_w = 8. The breaking space ends its segment (no char
        // dropped — every index has a home for cursor mapping).
        let segs = wrap_segments("hello world foo", 8, 8);
        assert_eq!(
            segs,
            vec![
                (0, "hello ".to_string()),
                (6, "world ".to_string()),
                (12, "foo".to_string()),
            ]
        );
        // Concatenating the segments reproduces the line exactly.
        let joined: String = segs.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(joined, "hello world foo");
    }

    #[test]
    fn wrap_segments_hard_breaks_an_unbreakable_run() {
        assert_eq!(
            wrap_segments("abcdefghij", 4, 4),
            vec![
                (0, "abcd".to_string()),
                (4, "efgh".to_string()),
                (8, "ij".to_string()),
            ]
        );
    }

    #[test]
    fn wrap_segments_honors_a_narrower_first_width() {
        // First segment fits 3 (after a wide prompt), continuations fit 6.
        let segs = wrap_segments("abcdefghi", 3, 6);
        assert_eq!(segs[0], (0, "abc".to_string()));
        assert_eq!(segs[1], (3, "defghi".to_string()));
    }

    #[test]
    fn overhang_rows_wraps_a_long_line_and_tracks_the_cursor() {
        let prompt = Line::from("❯ "); // width 2
        let lines = vec!["hello world foo".to_string()];
        // width 8 → row0 text width = 8-2 = 6; continuations 8-1 = 7 (g=1).
        // "hello " (6, after the 2-col prompt), then "world f" (7), then "oo".
        let (rows, cx, cy) = overhang_rows(&prompt, &lines, (0, 15), 1, 8, None);
        assert!(rows.len() >= 2, "the long line wrapped to multiple rows");
        // Cursor at end (col 15) lands on the last wrapped row.
        assert_eq!(cy as usize, rows.len() - 1);
        assert!(cx >= 1, "cursor is indented on the continuation row");
    }

    #[test]
    fn overhang_rows_short_line_is_one_row_after_the_prompt() {
        let prompt = Line::from("❯ "); // width 2
        let (rows, cx, cy) = overhang_rows(&prompt, &["hi".to_string()], (0, 2), 1, 80, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(cy, 0);
        assert_eq!(cx, 2 + 2, "prompt width (2) + cursor col (2)");
    }

    #[test]
    fn overhang_rows_cursor_sits_on_the_hint_not_after_it() {
        let prompt = Line::from("❯ "); // width 2
                                       // Empty line with a dim hint: the cursor anchors at the prompt end (col
                                       // 2) — ON the hint's first cell — NOT after the whole hint string.
        let (rows, cx, cy) = overhang_rows(
            &prompt,
            &[String::new()],
            (0, 0),
            1,
            80,
            Some("vi INSERT — type…"),
        );
        assert_eq!((cx, cy), (2, 0), "cursor at prompt end, on the hint");
        let text: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("vi INSERT"),
            "hint rendered after the cursor: {text:?}"
        );
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn special(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn vi_editor() -> Editor {
        Editor::new(Edit::Vi)
    }
    fn emacs_editor() -> Editor {
        Editor::new(Edit::Emacs)
    }

    /// Drive a sequence of chars (in NORMAL-friendly contexts) and return lines.
    fn type_chars(ed: &mut Editor, ta: &mut TextArea, s: &str) {
        for c in s.chars() {
            ed.input(key(c), ta);
        }
    }

    fn nano_editor() -> Editor {
        Editor::new(Edit::Nano)
    }

    /// Drive `:`-command `cmd` from NORMAL and return the Enter step.
    fn run_ex(ed: &mut Editor, ta: &mut TextArea, cmd: &str) -> Step {
        ed.input(special(KeyCode::Esc), ta); // INSERT → NORMAL
        ed.input(key(':'), ta);
        for c in cmd.chars() {
            ed.input(key(c), ta);
        }
        ed.input(special(KeyCode::Enter), ta)
    }

    #[test]
    fn vi_unbound_normal_key_emits_a_hint() {
        // #530: an unbound NORMAL key gives feedback instead of silently
        // swallowing the keypress.
        let mut ed = vi_editor();
        let mut ta = new_textarea(Edit::Vi);
        type_chars(&mut ed, &mut ta, "hi");
        ed.input(special(KeyCode::Esc), &mut ta); // → NORMAL
        let _ = ed.take_msg(); // drain anything prior
        ed.input(key('q'), &mut ta); // unbound in NORMAL
        let msg = ed
            .take_msg()
            .expect("an unbound NORMAL key should surface a hint");
        assert!(msg.contains("insert"), "hint nudges toward insert: {msg:?}");
        assert_eq!(ta.lines(), &["hi"], "`q` still types nothing in NORMAL");
    }

    #[test]
    fn vi_ex_command_renders_on_a_bottom_row_when_multiline() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        // #531: a `:`-command on a multi-line buffer belongs on its own bottom
        // row, vi-style — not glued to the first row's prompt.
        let mut ed = vi_editor();
        let mut ta = TextArea::new(vec!["hello".to_string(), "world".to_string()]);
        ed.input(special(KeyCode::Esc), &mut ta); // INSERT → NORMAL
        ed.input(key(':'), &mut ta); // open the ex line
        type_chars(&mut ed, &mut ta, "wq");
        assert!(
            ex_bottom_line(&ed, &ta).is_some(),
            "multi-line ex → bottom row"
        );

        let mut term = Terminal::new(TestBackend::new(40, 3)).unwrap();
        term.draw(|f| draw(f, &ta, &ed, Some(1), "", "")).unwrap();
        let buf = term.backend().buffer();
        let row = |y: u16| -> String {
            (0..40)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
        };
        // Row 0 is the status header (#527); the message rides the input row
        // below it, and the `:`-command lands on its own bottom row.
        assert!(
            row(2).starts_with(":wq"),
            "command on the bottom row: {:?}",
            row(2)
        );
        assert!(
            row(1).contains("hello"),
            "message rides the input row below the header: {:?}",
            row(1)
        );
        assert!(
            !row(1).contains(":wq"),
            "command must NOT be glued to the message row: {:?}",
            row(1)
        );
    }

    #[test]
    fn vi_ex_command_stays_inline_on_a_single_line() {
        // Single line keeps the inline chevron↔`:` swap (the part the user likes).
        let mut ed = vi_editor();
        let mut ta = TextArea::new(vec!["hi".to_string()]);
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key(':'), &mut ta);
        type_chars(&mut ed, &mut ta, "wq");
        assert!(
            ex_bottom_line(&ed, &ta).is_none(),
            "single-line stays inline"
        );
        let line = prompt_line(&ed, true);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains(":wq"), "single-line ex is inline: {text:?}");
    }

    #[test]
    fn vi_ex_command_bottom_row_in_wide_gutter_mode() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        // Same as the overhang case but through the wide-gutter render path
        // (gutter >= GUTTER_W) — the `:command` still lands on the bottom row.
        let mut ed = vi_editor();
        let mut ta = TextArea::new(vec!["hello".to_string(), "world".to_string()]);
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key(':'), &mut ta);
        type_chars(&mut ed, &mut ta, "wq");
        let mut term = Terminal::new(TestBackend::new(80, 3)).unwrap();
        term.draw(|f| draw(f, &ta, &ed, Some(25), "", "")).unwrap(); // 25 >= GUTTER_W (19)
        let buf = term.backend().buffer();
        let row2: String = (0..80)
            .map(|x| buf.cell((x, 2)).unwrap().symbol().to_string())
            .collect();
        assert!(
            row2.starts_with(":wq"),
            "command on the bottom row (wide gutter): {row2:?}"
        );
    }

    #[test]
    fn overhang_prompt_is_inline_with_one_space_hanging_continuation() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let editor = vi_editor();
        // Two input lines (as if a `o`/newline added a continuation).
        let ta = TextArea::new(vec!["this".to_string(), "more".to_string()]);
        // Width 80 so the full status header fits (it clips on a narrow term);
        // height 3: row 0 = status header (#527), rows 1-2 = the input.
        let mut term = Terminal::new(TestBackend::new(80, 3)).unwrap();
        // gutter = 1 → the overhang layout (the default).
        term.draw(|f| draw(f, &ta, &editor, Some(1), "m", "http://e:1"))
            .unwrap();
        let buf = term.backend().buffer();
        let row = |y: u16| -> String {
            (0..80)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
        };
        // Row 0: the status header (two-line layout, #527).
        assert!(
            row(0).contains("vi --INSERT--") && row(0).contains("m @ http://e:1"),
            "header row carries mode + model @ endpoint: {:?}",
            row(0)
        );
        // Row 1: the prompt prefixes the first input line inline (`❯ this`).
        assert!(
            row(1).contains("❯ this"),
            "first input line rides on the prompt row: {:?}",
            row(1)
        );
        // Row 2: continuation hangs by exactly one space, not the prompt width.
        assert!(
            row(2).starts_with(" more"),
            "continuation is 1-space hang-indented: {:?}",
            row(2)
        );
    }

    fn row_text(editor: &Editor) -> String {
        prompt_line(editor, true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn header_text(editor: &Editor, model: &str, endpoint: &str) -> String {
        header_line(editor, model, endpoint, true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn native_status_row_shows_the_insert_indicator() {
        // The input row carries the `❯` indicator; the header carries the clock,
        // mode word, and model @ endpoint (two-line layout, #527).
        let editor = vi_editor();
        assert!(row_text(&editor).contains('❯'), "insert indicator");
    }

    #[test]
    fn header_shows_datetime_mode_and_model_endpoint() {
        let insert = vi_editor(); // starts in INSERT
        let h = header_text(&insert, "nemotron-3-nano:30b", "http://dgx1.home.lab:11434");
        assert!(h.starts_with('['), "datetime stamp: {h:?}");
        assert!(h.contains("vi --INSERT--"), "{h:?}");
        assert!(
            h.contains("nemotron-3-nano:30b @ http://dgx1.home.lab:11434"),
            "{h:?}"
        );
        // NORMAL flips the mode word live.
        let mut normal = vi_editor();
        let mut ta = TextArea::default();
        normal.input(special(KeyCode::Esc), &mut ta);
        assert!(header_text(&normal, "m", "e").contains("vi --NORMAL--"));
        // emacs/nano show the bare editor name; empty model omits the `@`.
        assert!(header_text(&emacs_editor(), "m", "e").contains("emacs"));
        assert!(!header_text(&insert, "", "").contains('@'));
    }

    #[test]
    fn mode_hint_advertises_the_other_editor_modes() {
        assert!(vi_editor().mode_hint().contains("INSERT"));
        assert!(vi_editor().mode_hint().contains("/nano"));
        assert!(vi_editor().mode_hint().contains("/emacs"));
    }

    #[test]
    fn native_status_row_uses_colon_for_vi_normal_not_arrow() {
        let mut normal = vi_editor();
        let mut ta = TextArea::default();
        normal.input(special(KeyCode::Esc), &mut ta); // INSERT → NORMAL
        let row = row_text(&normal);
        assert!(!row.contains('❯'), "NORMAL drops the ❯ for `:`: {row:?}");
        // An open ex-line shows the typed command (still no ❯).
        let mut ex = vi_editor();
        ex.input(special(KeyCode::Esc), &mut ta);
        ex.input(key(':'), &mut ta);
        ex.input(key('w'), &mut ta);
        let exrow = row_text(&ex);
        assert!(exrow.contains(":w"), "ex line shows the command: {exrow:?}");
        assert!(!exrow.contains('❯'));
    }

    #[test]
    fn history_step_walks_older_then_back_to_the_fresh_line() {
        // Three entries; pos 3 == the fresh line.
        let len = 3;
        // ↑ from fresh walks back through 2,1,0 then stops.
        assert_eq!(history_step(3, len, true), Some(2));
        assert_eq!(history_step(2, len, true), Some(1));
        assert_eq!(history_step(1, len, true), Some(0));
        assert_eq!(history_step(0, len, true), None, "oldest: nowhere up");
        // ↓ walks forward and back onto the fresh line, then stops.
        assert_eq!(history_step(0, len, false), Some(1));
        assert_eq!(history_step(2, len, false), Some(3));
        assert_eq!(history_step(3, len, false), None, "fresh: nowhere down");
        // Empty history never moves.
        assert_eq!(history_step(0, 0, true), None);
        assert_eq!(history_step(0, 0, false), None);
    }

    #[test]
    fn load_history_reads_nonblank_lines_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        std::fs::write(&path, "first\n\nsecond\n  \nthird\n").unwrap();
        assert_eq!(
            load_history(Some(&path)),
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );
        // Missing file / no path → empty, never an error.
        assert!(load_history(Some(&dir.path().join("nope"))).is_empty());
        assert!(load_history(None).is_empty());
    }

    #[test]
    fn textarea_with_prefills_content_for_recall() {
        let ta = textarea_with(Edit::Vi, "recalled prompt");
        assert_eq!(ta.lines(), &["recalled prompt".to_string()]);
    }

    #[test]
    fn vi_w_submits_like_enter() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "hello");
        // `:w` = write = submit, no confirm.
        assert_eq!(run_ex(&mut ed, &mut ta, "w"), Step::Submit);
        assert!(ed.confirm_prompt().is_none());
    }

    #[test]
    fn vi_wq_confirms_then_y_submit_quits() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "ship it");
        // `:wq` arms a [y/N] confirm rather than submitting outright.
        assert_eq!(
            run_ex(&mut ed, &mut ta, "wq"),
            Step::Continue,
            ":wq must not submit until confirmed"
        );
        assert!(
            ed.confirm_prompt().is_some(),
            "the [y/N] question is showing"
        );
        // `y` commits → submit-then-end-and-quit.
        assert_eq!(ed.input(key('y'), &mut ta), Step::SubmitQuit);
        assert!(
            ed.confirm_prompt().is_none(),
            "confirm cleared after answer"
        );
    }

    #[test]
    fn vi_wq_confirm_cancels_on_n_or_enter() {
        for answer in [KeyCode::Char('n'), KeyCode::Enter, KeyCode::Esc] {
            let mut ed = vi_editor();
            let mut ta = TextArea::default();
            type_chars(&mut ed, &mut ta, "keep editing");
            assert_eq!(run_ex(&mut ed, &mut ta, "wq"), Step::Continue);
            // Anything but y/Y dumps the user back into editing — no submit.
            assert_eq!(
                ed.input(special(answer), &mut ta),
                Step::Continue,
                "{answer:?} cancels the confirm"
            );
            assert!(ed.confirm_prompt().is_none(), "confirm cleared on cancel");
            // The buffer survived the aborted quit.
            assert_eq!(ta.lines(), &["keep editing".to_string()]);
        }
    }

    #[test]
    fn vi_wq_bang_forces_without_confirm() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "no prompt");
        // The `!` form means "I'm sure" — straight to SubmitQuit.
        assert_eq!(run_ex(&mut ed, &mut ta, "wq!"), Step::SubmitQuit);
        assert!(ed.confirm_prompt().is_none());
    }

    #[test]
    fn vi_q_quits_without_sending() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "discard me");
        assert_eq!(run_ex(&mut ed, &mut ta, "q"), Step::Eof);
    }

    #[test]
    fn nano_is_modeless_and_labeled() {
        let mut ed = nano_editor();
        let mut ta = TextArea::default();
        assert_eq!(ed.label(), "nano");
        // Modeless like emacs: typing inserts text, no NORMAL mode.
        type_chars(&mut ed, &mut ta, "plain text");
        assert_eq!(ed.label(), "nano", "no mode flip");
        assert_eq!(ta.lines(), &["plain text".to_string()]);
        // Enter still submits; Ctrl-O still newlines (shared handling).
        assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);
        assert!(Edit::Nano.is_modeless() && Edit::Emacs.is_modeless());
        assert!(!Edit::Vi.is_modeless());
    }

    #[test]
    fn resolve_gutter_auto_off_and_fixed() {
        // auto (None): prompt-width gutter when it fits, else 0.
        assert_eq!(
            resolve_gutter(None, 80),
            GUTTER_W,
            "auto wide → inline gutter"
        );
        assert_eq!(resolve_gutter(None, 50), 0, "auto squished → stacked (0)");
        // off (Some(0)): always 0.
        assert_eq!(resolve_gutter(Some(0), 80), 0);
        // fixed N: exactly N, clamped to the usable width.
        assert_eq!(resolve_gutter(Some(3), 80), 3, "3-space indent");
        assert_eq!(
            resolve_gutter(Some(25), 80),
            25,
            "wide enough to hold the prompt"
        );
        assert_eq!(resolve_gutter(Some(200), 80), 79, "clamped to width-1");
    }

    #[test]
    fn use_gutter_drops_when_over_a_third() {
        // Keep the gutter while GUTTER_W (19) <= 0.33*width, i.e. width >= ~58.
        assert!(use_gutter(80), "gutter fits at 80 cols");
        assert!(use_gutter(58), "19 <= 0.33*58 (19.14) → gutter stays on");
        assert!(!use_gutter(57), "19 > 0.33*57 (18.81) → drop the gutter");
        assert!(!use_gutter(40), "way too narrow → drop the gutter");
        assert!(!use_gutter(0), "zero width never uses a gutter");
    }

    #[test]
    fn emacs_enter_submits_and_ctrl_o_inserts_newline() {
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "hello");
        // Ctrl-O adds a line without submitting.
        assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
        type_chars(&mut ed, &mut ta, "world");
        assert_eq!(ta.lines().len(), 2, "two lines after Ctrl-O");
        // Plain Enter submits.
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);
    }

    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let nl = || KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "line one");
        assert_eq!(
            ed.input(nl(), &mut ta),
            Step::Continue,
            "Shift-Enter newline"
        );
        type_chars(&mut ed, &mut ta, "line two");
        assert_eq!(ta.lines().len(), 2, "Shift-Enter added a line");

        // Same in vi INSERT mode (shared handling runs before mode dispatch).
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "vi line");
        assert_eq!(ed.input(nl(), &mut ta), Step::Continue);
        assert_eq!(ta.lines().len(), 2, "Shift-Enter newline in vi too");

        // Ctrl-Enter is NOT bound (macOS intercepts it at the terminal layer);
        // a plain Enter with no continuation still submits, unaffected.
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "x");
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);
    }

    #[test]
    fn ctrl_o_is_newline_in_modeless_but_reserved_in_vi() {
        // Emacs / nano: Ctrl-O inserts a newline (idiomatic open-line).
        for ed_factory in [emacs_editor as fn() -> Editor, nano_editor] {
            let mut ed = ed_factory();
            let mut ta = TextArea::default();
            type_chars(&mut ed, &mut ta, "a");
            assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
            assert_eq!(ta.lines().len(), 2, "Ctrl-O newline in modeless mode");
        }
        // Vi: Ctrl-O is reserved (jumplist / insert-normal) — it must NOT insert
        // a newline. In INSERT it is currently a no-op (a documented gap), so the
        // buffer stays a single line; vi users open lines with `o`/`O`.
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "vi");
        assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
        assert_eq!(ta.lines().len(), 1, "Ctrl-O does NOT newline in vi");
    }

    #[test]
    fn enter_continues_an_open_bang_line() {
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        // A `! …\` host-shell line is mid-continuation → Enter adds a line.
        type_chars(&mut ed, &mut ta, "! ls \\");
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Continue);
        assert_eq!(ta.lines().len(), 2, "Enter continued the bang line");
    }

    #[test]
    fn ctrl_c_abandons_line_and_ctrl_d_empty_is_eof() {
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        // Ctrl-C abandons the current line (clears it) and stays in the session.
        type_chars(&mut ed, &mut ta, "throwaway");
        assert_eq!(
            ed.input(ctrl('c'), &mut ta),
            Step::Continue,
            "Ctrl-C does not exit"
        );
        assert!(buffer_is_empty(&ta), "Ctrl-C cleared the buffer");
        // Ctrl-D on the now-empty buffer is EOF (exit).
        assert_eq!(
            ed.input(ctrl('d'), &mut ta),
            Step::Eof,
            "Ctrl-D empty → EOF"
        );
        // Ctrl-D with content submits instead.
        type_chars(&mut ed, &mut ta, "x");
        assert_eq!(ed.input(ctrl('d'), &mut ta), Step::Submit);
    }

    #[test]
    fn vi_o_opens_line_below_and_enters_insert() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        // Start in INSERT (vi default), type, Esc to NORMAL.
        type_chars(&mut ed, &mut ta, "first");
        ed.input(special(KeyCode::Esc), &mut ta);
        assert_eq!(ed.label(), "vi N");
        // `o` opens a line below and returns to INSERT.
        ed.input(key('o'), &mut ta);
        assert_eq!(ed.label(), "vi I");
        type_chars(&mut ed, &mut ta, "second");
        assert_eq!(ta.lines(), &["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn vi_uppercase_o_opens_line_above() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "below");
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key('O'), &mut ta);
        type_chars(&mut ed, &mut ta, "above");
        assert_eq!(ta.lines(), &["above".to_string(), "below".to_string()]);
    }

    #[test]
    fn vi_dd_deletes_the_line() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "doomed");
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key('d'), &mut ta);
        ed.input(key('d'), &mut ta);
        assert_eq!(ta.lines(), &[String::new()], "dd cleared the only line");
    }

    #[test]
    fn vi_x_with_count_deletes_n_chars() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "abcdef");
        ed.input(special(KeyCode::Esc), &mut ta); // NORMAL, cursor on 'f'
        ed.input(key('0'), &mut ta); // head
        type_chars(&mut ed, &mut ta, "3x"); // delete 3 chars
        assert_eq!(ta.lines(), &["def".to_string()]);
    }

    #[test]
    fn vi_ex_wq_submits_and_q_is_eof() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "payload");
        ed.input(special(KeyCode::Esc), &mut ta);
        // `:wq` arms the send-then-end-and-quit confirm (see the dedicated
        // confirm tests); it does NOT submit outright.
        ed.input(key(':'), &mut ta);
        assert_eq!(ed.ex(), Some(""), "ex line is active");
        ed.input(key('w'), &mut ta);
        ed.input(key('q'), &mut ta);
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Continue);
        assert!(ed.confirm_prompt().is_some());

        // `:q` → EOF.
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key(':'), &mut ta);
        ed.input(key('q'), &mut ta);
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Eof);
    }

    /// Build a multi-line buffer in vi: type lines separated by Shift-Enter
    /// (which inserts a newline in every mode), then Esc to NORMAL at the top.
    fn vi_buffer(lines: &[&str]) -> (Editor, TextArea<'static>) {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                ed.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &mut ta);
            }
            type_chars(&mut ed, &mut ta, l);
        }
        ed.input(special(KeyCode::Esc), &mut ta); // NORMAL
        ed.input(key('g'), &mut ta);
        ed.input(key('g'), &mut ta); // top
        (ed, ta)
    }

    #[test]
    fn vi_uppercase_j_joins_line_below() {
        let (mut ed, mut ta) = vi_buffer(&["foo", "bar"]);
        ed.input(key('J'), &mut ta);
        assert_eq!(ta.lines(), &["foo bar".to_string()], "J joins with a space");
        // J on the only remaining line is a no-op (nothing below).
        ed.input(key('J'), &mut ta);
        assert_eq!(ta.lines(), &["foo bar".to_string()]);
    }

    #[test]
    fn vi_count_j_joins_multiple_lines() {
        let (mut ed, mut ta) = vi_buffer(&["a", "b", "c"]);
        // 3J joins this line + 2 below → one line.
        ed.input(key('3'), &mut ta);
        ed.input(key('J'), &mut ta);
        assert_eq!(ta.lines(), &["a b c".to_string()]);
    }

    #[test]
    fn vi_insert_normal_ctrl_o_runs_one_command() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "hello"); // INSERT, cursor at end
                                               // i_CTRL-O: one Normal command (`0` → head) then back to INSERT.
        ed.input(ctrl('o'), &mut ta);
        assert_eq!(ed.label(), "vi N", "Ctrl-O drops to NORMAL");
        ed.input(key('0'), &mut ta);
        assert_eq!(ed.label(), "vi I", "resumes INSERT after one command");
        type_chars(&mut ed, &mut ta, "X");
        assert_eq!(ta.lines(), &["Xhello".to_string()], "inserted at head");
    }

    #[test]
    fn vi_esc_cancels_incomplete_command() {
        // A pending operator is cancelled by Esc — the next key is a fresh
        // command, not the operator's motion.
        let (mut ed, mut ta) = vi_line("hello world");
        ed.input(key('d'), &mut ta); // pending d
        ed.input(special(KeyCode::Esc), &mut ta); // cancel
        ed.input(key('w'), &mut ta); // plain motion now, not `dw`
        assert_eq!(
            ta.lines(),
            &["hello world".to_string()],
            "Esc cancelled the d operator"
        );

        // A building count is cancelled by Esc.
        let (mut ed, mut ta) = vi_line("abcdef");
        ed.input(key('3'), &mut ta); // count = 3
        ed.input(special(KeyCode::Esc), &mut ta); // cancel count
        ed.input(key('x'), &mut ta); // deletes 1, not 3
        assert_eq!(
            ta.lines(),
            &["bcdef".to_string()],
            "Esc cancelled the count"
        );
    }

    #[test]
    fn vi_jumplist_back_and_forward() {
        let (mut ed, mut ta) = vi_buffer(&["one", "two", "three"]);
        // We're at the top (gg recorded a jump from the bottom line).
        assert_eq!(ta.cursor().0, 0, "gg → row 0");
        // Ctrl-O jumps back to the pre-gg position (the last line).
        ed.input(ctrl('o'), &mut ta);
        assert_eq!(ta.cursor().0, 2, "Ctrl-O → back to row 2");
        // Ctrl-I (Tab) jumps forward again to the top.
        ed.input(special(KeyCode::Tab), &mut ta);
        assert_eq!(ta.cursor().0, 0, "Ctrl-I → forward to row 0");
    }

    /// A single-line vi buffer at NORMAL, cursor at head.
    fn vi_line(s: &str) -> (Editor, TextArea<'static>) {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, s);
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key('0'), &mut ta); // head
        (ed, ta)
    }

    #[test]
    fn vi_f_and_semicolon_and_comma_char_search() {
        let (mut ed, mut ta) = vi_line("a.b.c.d");
        // f. → first dot (col 1).
        ed.input(key('f'), &mut ta);
        ed.input(key('.'), &mut ta);
        assert_eq!(ta.cursor().1, 1, "f. → first dot");
        // ; → next dot (col 3).
        ed.input(key(';'), &mut ta);
        assert_eq!(ta.cursor().1, 3, "; → next dot");
        // , → previous dot (col 1).
        ed.input(key(','), &mut ta);
        assert_eq!(ta.cursor().1, 1, ", → previous dot");
    }

    #[test]
    fn vi_t_and_capital_f_char_search() {
        let (mut ed, mut ta) = vi_line("abcXdef");
        // tX → just before X (col 2).
        ed.input(key('t'), &mut ta);
        ed.input(key('X'), &mut ta);
        assert_eq!(ta.cursor().1, 2, "tX → col before X");
        // Move to end, then FX → back onto X (col 3).
        ed.input(key('$'), &mut ta);
        ed.input(key('F'), &mut ta);
        ed.input(key('X'), &mut ta);
        assert_eq!(ta.cursor().1, 3, "FX → onto X");
    }

    #[test]
    fn vi_operator_motions_dw_d_dollar_d0_yy() {
        // dw deletes to the start of the next word.
        let (mut ed, mut ta) = vi_line("foo bar baz");
        ed.input(key('d'), &mut ta);
        ed.input(key('w'), &mut ta);
        assert_eq!(ta.lines(), &["bar baz".to_string()], "dw");

        // d$ deletes to end of line.
        let (mut ed, mut ta) = vi_line("keep DROP this");
        ed.input(key('f'), &mut ta);
        ed.input(key('D'), &mut ta); // f D → cursor on the 'D' of DROP
        ed.input(key('d'), &mut ta);
        ed.input(key('$'), &mut ta);
        assert_eq!(ta.lines(), &["keep ".to_string()], "d$");

        // d0 deletes from the cursor back to the beginning of the line.
        let (mut ed, mut ta) = vi_line("alpha beta");
        ed.input(key('f'), &mut ta);
        ed.input(key('b'), &mut ta); // f b → col 6 ('b' of "beta")
        ed.input(key('d'), &mut ta);
        ed.input(key('0'), &mut ta);
        assert_eq!(ta.lines(), &["beta".to_string()], "d0 deletes to BOL");

        // yy then p duplicates the line.
        let (mut ed, mut ta) = vi_line("dup");
        ed.input(key('y'), &mut ta);
        ed.input(key('y'), &mut ta);
        ed.input(key('p'), &mut ta);
        assert!(
            ta.lines().iter().filter(|l| l.contains("dup")).count() >= 1,
            "yy+p yanks and pastes the line"
        );
    }

    #[test]
    fn mode_idiomatic_exit_keys() {
        // emacs: C-x C-c quits.
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        assert_eq!(
            ed.input(ctrl('x'), &mut ta),
            Step::Continue,
            "C-x arms prefix"
        );
        assert_eq!(ed.input(ctrl('c'), &mut ta), Step::Eof, "C-x C-c → exit");
        // emacs: C-x then a non-C-c key cancels the prefix (no exit); a bare
        // Ctrl-C afterwards abandons the line (not exit), not part of a sequence.
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        ed.input(ctrl('x'), &mut ta);
        type_chars(&mut ed, &mut ta, "a"); // cancels the prefix, inserts 'a'
        assert_eq!(ta.lines(), &["a".to_string()]);
        assert_eq!(
            ed.input(ctrl('c'), &mut ta),
            Step::Continue,
            "bare C-c abandons the line, does not exit"
        );
        assert!(buffer_is_empty(&ta), "bare C-c cleared the line");
        // nano: ^X exits directly.
        let mut ed = nano_editor();
        let mut ta = TextArea::default();
        assert_eq!(ed.input(ctrl('x'), &mut ta), Step::Eof, "nano ^X → exit");
        // vi: C-x is not an exit key (uses :q); it does nothing special here.
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        assert_eq!(
            ed.input(ctrl('x'), &mut ta),
            Step::Continue,
            "vi C-x → no exit"
        );
    }

    #[test]
    fn mode_idiomatic_help_keys_queue_a_cheatsheet() {
        // nano: Ctrl-G.
        let mut ed = nano_editor();
        let mut ta = TextArea::default();
        assert_eq!(ed.input(ctrl('g'), &mut ta), Step::Continue);
        assert!(ed.take_msg().unwrap().starts_with("nano"));
        // emacs: Ctrl-h.
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        assert_eq!(ed.input(ctrl('h'), &mut ta), Step::Continue);
        assert!(ed.take_msg().unwrap().starts_with("emacs"));
        // vi: `:help`.
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key(':'), &mut ta);
        for c in "help".chars() {
            ed.input(key(c), &mut ta);
        }
        ed.input(special(KeyCode::Enter), &mut ta);
        assert!(ed.take_msg().unwrap().starts_with("vi"));
        // The help key in the wrong mode does nothing special: Ctrl-G in emacs
        // is not help (no message queued).
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        ed.input(ctrl('g'), &mut ta);
        assert!(ed.take_msg().is_none());
    }

    #[test]
    fn vi_colon_jumps_queues_a_note() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key(':'), &mut ta);
        for c in "jumps".chars() {
            ed.input(key(c), &mut ta);
        }
        ed.input(special(KeyCode::Enter), &mut ta);
        assert!(ed.take_msg().is_some(), ":jumps queued a scrollback note");
        assert!(ed.take_msg().is_none(), "note is one-shot");
    }

    #[test]
    fn history_appends_unsaved_entries_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let hp = dir.path().join("history");
        let mut s = RichSurface::new(Some(hp.clone())).unwrap();
        s.add_history("alpha");
        s.add_history("multi\nline");
        s.save_history();
        let contents = std::fs::read_to_string(&hp).unwrap();
        assert!(contents.contains("alpha"));
        assert!(
            contents.contains("multi line"),
            "newlines flattened to keep one entry per line"
        );
        // Second save with nothing new is a no-op (no duplicate append).
        s.save_history();
        assert_eq!(std::fs::read_to_string(&hp).unwrap(), contents);
    }

    #[test]
    fn history_without_path_is_a_noop() {
        let mut s = RichSurface::new(None).unwrap();
        s.add_history("ephemeral");
        s.save_history(); // must not panic
    }
}

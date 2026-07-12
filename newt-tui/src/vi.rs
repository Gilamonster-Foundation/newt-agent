//! The **vi** editing mode — the modal `Vi` state machine, extracted from
//! `rich_input.rs` (#1096, functional-cohesion pass). NORMAL/INSERT, motions,
//! `d/c/y` operators, `f/F/t/T` search, counts, the `:` ex-line (`:w`/`:wq`/
//! `:q` + the `[y/N]` confirm gate), and the Ctrl-O/Ctrl-I jumplist — a faithful
//! subset of rustyline's `vi_command` ported onto tui-textarea.
//!
//! It's the one modal editor (emacs & nano are the modeless default path in
//! `rich_input`). The rich editor composes it: `rich_input::Editor` owns a
//! `Vi` and dispatches to it when the active `Edit` kind is `Vi`. This is the
//! big, self-contained chunk someone opens to understand "how does vi mode
//! work" — hence its own file.

use tui_textarea::{CursorMove, TextArea};

use crate::rich_input::{char_search, help_text, reverse_find, Edit, Step};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A pending `[y/N]` confirmation in the vi `:`-line. Today only `:wq`/`:x`
/// arms one ("send prompt then quit?"); modeled as an enum so other
/// destructive ex-commands can reuse the same gate.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Confirm {
    /// `:wq` / `:x` — submit + end-conversation + quit, pending a `y`.
    SubmitQuit,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Normal,
    Insert,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Pending {
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
pub(crate) struct Vi {
    pub(crate) mode: Mode,
    pending: Pending,
    count: usize,
    /// `:`-command line buffer (`:wq`, `:q`, …); `Some` while active.
    pub(crate) ex: Option<String>,
    /// A pending `[y/N]` confirmation (e.g. `:wq` → "send prompt then quit?").
    /// While `Some`, the next key is the answer — `y`/`Y` confirms, anything
    /// else cancels back to NORMAL editing.
    pub(crate) confirm: Option<Confirm>,
    /// i_CTRL-O: run exactly one Normal command from INSERT, then resume INSERT.
    insert_normal: bool,
    /// Jumplist: positions we jumped *from*, older toward the front of `jback`;
    /// `jfwd` holds positions undone by Ctrl-O so Ctrl-I can redo them. Browser
    /// back/forward model. Each entry is a `(row, col)` cursor position.
    jback: Vec<(usize, usize)>,
    jfwd: Vec<(usize, usize)>,
    /// A one-shot message to print to scrollback (e.g. `:jumps` output).
    pub(crate) msg: Option<String>,
    /// Last `f`/`F`/`t`/`T` search as `(kind, target)`, for `;` (repeat) and
    /// `,` (repeat reversed).
    last_find: Option<(char, char)>,
}

impl Vi {
    pub(crate) fn new() -> Self {
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

    pub(crate) fn input(&mut self, key: KeyEvent, ta: &mut TextArea) -> Step {
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
    pub(crate) fn mode_label(&self) -> &'static str {
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

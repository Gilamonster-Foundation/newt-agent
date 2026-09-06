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
//! ## There's a nice crate hiding in here
//!
//! This module + its sibling [`vi`](crate::vi) are, between them, a complete
//! multi-mode line editor over `tui-textarea`: a modeless emacs/nano path (here)
//! and a faithful modal vi state machine (in `vi.rs`). None of it is tied to
//! newt's chat loop except through the [`InputSurface`] seam. Lifted out, it
//! would be a reusable, separately branded editor crate — the vi/emacs/nano
//! bindings are the interesting, portable part. Kept here for now; the split
//! into `vi.rs` (#1096 functional-cohesion pass) is the first cut along that
//! seam.
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
//! - Harness background jobs share a final liveness row below the input. The
//!   surface owns its rendering; workers publish state but never terminal bytes.
//! - The per-turn event loop (`read_line`) needs a real TTY and is exercised by
//!   creature-testing, not unit tests; the editing/state logic below is fully
//!   unit-tested.

use std::cell::Cell;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers,
};
use newt_core::tty::raw_mode::RawModeGuard;
use newt_core::tty::str_width;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};
use ratatui::Frame;
use tui_textarea::{CursorMove, TextArea};

use crate::chat::BackgroundJob;
use crate::palette::{palette_lines, PaletteState};
use crate::{footer_continues, InputSurface, ReadOutcome};

mod command;
use command::{
    bang_view, cancel_hidden_bang_selection, command_background, command_line,
    command_line_with_focus, is_bang_escape, CommandKind, COMMAND_BG,
};

mod geometry;
use geometry::{draw_overhang, overhang_rows, wrap_segments};

mod gutter;
use gutter::{resolve_gutter, GUTTER_W};

mod mounted;
pub(crate) use mounted::{EditorOutcome, MountedEditor};

const MAX_INPUT_ROWS: u16 = 8;
type Term = crate::inline_viewport::InlineTerm;

/// One step of the editor: what the loop should do after handling a key.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Step {
    /// #1669 16.3: the operator asked for a tab action from the KEYBOARD.
    ///
    /// The terminal recognises the gesture; the session applies it — tabs are
    /// session state. Carrying the same `TabAction` the `/tab` text engine
    /// parses is deliberate: one vocabulary, so a key and a slash command
    /// cannot drift into meaning different things.
    ///
    /// `Step` gave up `Copy` for this (`TabAction::Rename` owns a `String`).
    /// That cost nothing — no call site depended on it.
    Tab(crate::tabs::TabAction),
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

// The vi state machine (`Vi`, `Mode`, `Pending`, `Confirm`, motions) lives in
// `vi.rs` — the one modal editor, big enough to warrant its own file. Emacs &
// nano are the modeless default path handled inline below. There's a clean,
// reusable line-editor crate hiding in this pair of files.
use crate::vi::{Confirm, Mode, Vi};

/// Move the cursor by an `f`/`F`/`t`/`T` char-search on the current line:
/// `f` lands on the next `target`, `t` just before it; `F` lands on the previous
/// `target`, `T` just after it. No move if the target isn't found on the line.
pub(crate) fn char_search(ta: &mut TextArea, kind: char, target: char) {
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
pub(crate) fn reverse_find(kind: char) -> char {
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
pub(crate) fn help_text(edit: Edit) -> String {
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
pub(crate) enum Edit {
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

    /// The line is over — sent, or abandoned with Ctrl-C. Drops the
    /// mid-sequence scratch of BOTH modal layers (emacs' armed `C-x` prefix,
    /// vi's pending operator/count/ex) and keeps everything that is session
    /// state; see [`Vi::reset_for_new_line`] for what that is and why.
    fn reset_for_new_line(&mut self) {
        self.cx_pending = false;
        self.vi.reset_for_new_line();
    }

    // `unix` with the ladder it feeds (`lib.rs` `mod esc_ladder`): the only
    // non-test consumer is the cockpit presenter, whose live half is unix-only,
    // so on Windows this accessor would compile with no caller and `-D
    // warnings` would fail the build on the dead code.
    #[cfg(unix)]
    /// Register this editor's Esc-ladder claims (#2005).
    ///
    /// **The `edit` gate is mandatory, not defensive.** `Editor` carries a
    /// `Vi` in ALL modes — `Editor::new` builds one unconditionally and
    /// `Vi::new` starts in `Mode::Insert` — and `Editor::input` gates every vi
    /// dispatch on `self.edit == Edit::Vi`. Without the same gate here,
    /// `vi-insert` would claim Esc permanently under emacs and nano, where the
    /// key is a silent no-op and rung 7 is exactly what should own it.
    ///
    /// emacs' armed `C-x` prefix needs no rung: the only key that completes it
    /// is Ctrl-C, which the hatch reserves, so the ladder preserves today's
    /// behaviour (the presenter's interrupt already preempted `C-x C-c`
    /// mid-turn) without a row.
    fn claims(&self, c: &mut precedence_ladder::ClaimSet) {
        if self.edit == Edit::Vi {
            self.vi.claims(c);
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
                    self.reset_for_new_line();
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

    /// The dim, clears-on-type hint shown on an empty line — the editor mode plus
    /// how to switch. vi is the default; `/nano` and `/emacs` are advertised.
    ///
    /// The `^C` half is **turn-conditional** (#2006, contract doc §3 item 4):
    /// Ctrl-C only interrupts while a turn is running, and at an idle prompt it
    /// clears the draft (see the `KeyCode::Char('c')` arm in [`Editor::input`]).
    /// The hint used to promise `^C interrupt` at both, so the affordance and
    /// the behavior now share one condition instead of drifting apart.
    ///
    /// The `^D` half is **idle-only** by the same rule (#2010): the session
    /// reads a line only between turns, so mid-turn Ctrl-D exits nothing —
    /// it is answered with a note instead (see [`MountedEditor::on_event`]).
    /// The hint used to promise `^D exit` during a turn too.
    fn mode_hint(&self, turn_running: bool) -> String {
        let head = match self.edit {
            Edit::Vi => match self.vi.mode {
                Mode::Insert => "vi INSERT — Esc: NORMAL · :help · /nano /emacs",
                Mode::Normal => "vi NORMAL — i: insert · :cmd · /nano /emacs",
            },
            Edit::Emacs => "emacs — Enter sends · Ctrl-h help · /vi /nano",
            Edit::Nano => "nano — Enter sends · ^G help · /vi /emacs",
        };
        let keys = if turn_running {
            "^C interrupt"
        } else {
            "^C clear · ^D exit"
        };
        format!("{head} · {keys}")
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

/// #1950: through the ONE inline constructor. The rich input surface is the
/// prompt itself — if it refuses to open because the terminal stayed quiet,
/// there is nothing left to type into.
/// #1979: leases its rows with [`OnCollision::Refuse`]. The prompt is the
/// incumbent bottom-pinned surface — it is normally first, and when it is not,
/// taking rows an open panel owns would be #1977 with the roles swapped. A
/// refusal degrades to the lean input rather than painting through somebody.
fn make_terminal(height: u16) -> io::Result<Term> {
    let lease =
        crate::inline_viewport::lease_bottom_rows(height, newt_core::tty::OnCollision::Refuse)?;
    crate::inline_viewport::inline_terminal(lease)
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

/// The rich surface's live view, in **three regions plus an activity row**.
///
/// ```text
/// ⠋ thinking                                   activity — only while something runs
/// [session] a conversation                     HEADER: which conversation this is
/// ❯ what you are typing                        TEXTAREA
/// [18:43:48] vi --INSERT-- model @ ep  28k/28k FOOTER: what the machine is doing
/// ```
///
/// # Why the split, and why the footer is at the BOTTOM
///
/// These were one row. It carried the timestamp, the mode word, the session
/// name, the model, the gauge — and an echo of the draft, directly above the
/// line already showing that draft. Fusing identity with machine state gave a
/// row with no single owner, which is how it grew an echo nobody would have
/// added on purpose.
///
/// Split by ownership: the header says WHICH CONVERSATION THIS IS, the footer
/// says WHAT THE MACHINE IS DOING, and the draft appears once — on the line you
/// are typing it.
///
/// The footer sits BELOW the input because an English reader's eye returns to
/// the lower left. Status anchored there is found without hunting; the same
/// information floated into another corner is a radiator you have to go and
/// look at, which is the worst place for something that changes constantly. (A
/// right-to-left locale would anchor lower-right — the rule is "where the eye
/// lands", not "left".)
///
/// # And no horizontal rules
///
/// No region is separated by a `─────` run. A full-width rule is a word-wrap
/// hazard at every terminal width and buys nothing adjacency does not already
/// give: the rows are next to each other, which is what makes them regions.
fn header_line(session: &str, headline: &str) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    // #1671: the session name is ALWAYS visible — a mid-luminance grey so it
    // reads without shouting (accessibility default: no saturated darks).
    if !session.is_empty() {
        spans.push(Span::styled(
            format!("[{session}]"),
            Style::default().fg(crate::theme::color(crate::theme::Role::Muted)),
        ));
    }
    if !headline.is_empty() {
        let gap = if spans.is_empty() { "" } else { " " };
        spans.push(Span::styled(
            format!("{gap}{headline}"),
            Style::default().fg(crate::theme::color(crate::theme::Role::Text)),
        ));
    }
    Line::from(spans)
}

/// How many rows the header needs at this width.
///
/// Wrapped, not truncated: a long conversation name is worth reading, and a
/// header that silently cut one off would be the same lie a fold without a
/// count tells. One row minimum, so the region never collapses to nothing and
/// take the input's place with it.
fn header_height(session: &str, headline: &str, cols: u16) -> u16 {
    let width = usize::from(cols.max(1));
    let text: String = header_line(session, headline)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    u16::try_from(newt_core::tty::wrap_line(&text, width).len())
        .unwrap_or(1)
        .max(1)
}

/// How many rows the modal slot needs — zero when there is no modal.
///
/// Zero rows means the layout is byte-identical to a session that never opened
/// one, which is what keeps "a modal has a fixed home" from costing a row to
/// everyone who is not looking at a modal.
fn modal_height(modal: Option<&[Line<'static>]>, cols: u16) -> u16 {
    let width = usize::from(cols.max(1));
    modal.map_or(0, |lines| {
        let rows: usize = lines
            .iter()
            .map(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                newt_core::tty::wrap_line(&text, width).len().max(1)
            })
            .sum();
        u16::try_from(rows).unwrap_or(u16::MAX)
    })
}

/// The footer: what the machine is doing, anchored at the lower left.
///
/// `active` is "this row has content AND chat owns the keyboard". Both halves
/// matter: a blocking modal leaves the mounted footer visible underneath it,
/// and a mode word still burning in the live accent there is a second thing on
/// screen claiming the keyboard. The chevron recedes
/// ([`prompt_line_with_focus`]); so must this.
fn footer_line(
    editor: &Editor,
    model: &str,
    endpoint: &str,
    gauge: Option<(u32, u32)>,
    active: bool,
) -> Line<'static> {
    // ONE accent, shared with the chevron: two constants for one signal is how
    // they drift apart.
    let accent = crate::theme::color(crate::theme::Role::Accent);
    let dim = crate::theme::color(crate::theme::Role::Dim);
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
    // Step 24.6 (#559): the context-budget gauge — `used/budget` (e.g.
    // `899k/1024k`) colored by fill, so the operator sees context pressure
    // BEFORE compression fires. Hidden until the budget is known.
    if let Some((used, budget)) = gauge {
        if budget > 0 {
            let g = newt_core::agentic::fmt_token_gauge(used, budget);
            let c = match newt_core::agentic::gauge_level(used, budget) {
                newt_core::agentic::GaugeLevel::Ok => {
                    crate::theme::color(crate::theme::Role::GaugeOk)
                }
                newt_core::agentic::GaugeLevel::Warn => {
                    crate::theme::color(crate::theme::Role::GaugeWarn)
                }
                newt_core::agentic::GaugeLevel::Critical => {
                    crate::theme::color(crate::theme::Role::GaugeCritical)
                }
            };
            spans.push(Span::styled(format!("  {g}"), Style::default().fg(c)));
        }
    }
    Line::from(spans)
}

/// The input-row indicator (below [`header_line`]): `❯ ` for the input line in
/// either vi mode, `:cmd` for an OPEN ex-line, or the `[y/N]` confirmation. The
/// input begins right after it, so the cursor anchors here and the dim mode
/// hint (drawn by `draw_overhang` on an empty line) sits after it.
///
/// The `:` is reserved for a command line the operator actually opened. See
/// [`prompt_line_with_focus`].
fn prompt_line(editor: &Editor, ex_inline: bool) -> Line<'static> {
    prompt_line_with_focus(editor, ex_inline, true)
}

/// The input-row indicator with explicit keyboard focus. A blocking modal
/// temporarily leaves the mounted chat editor visible underneath it; dimming
/// this marker makes the modal's accented chevron the only active prompt.
fn prompt_line_with_focus(editor: &Editor, ex_inline: bool, focused: bool) -> Line<'static> {
    if let Some(question) = editor.confirm_prompt() {
        // A pending [y/N] confirmation replaces the input-row prompt until answered.
        return Line::from(Span::styled(
            question.to_string(),
            Style::default()
                .fg(if focused {
                    Color::LightYellow
                } else {
                    Color::DarkGray
                })
                .add_modifier(Modifier::BOLD),
        ));
    }
    let accent = if focused {
        Color::from(newt_core::tty::ACTIVE_INPUT_CT)
    } else {
        Color::DarkGray
    };
    // An open ex-line is inline only on a single-line buffer; a multi-line
    // `:`-command moves to its own bottom row (#531; see `draw`).
    if ex_inline {
        if let Some(ex) = editor.ex() {
            return command_line_with_focus(CommandKind::Ex, ex, focused);
        }
    }
    // **NORMAL is not command-line mode.** The `:` belongs to an OPEN ex line
    // and nothing else — in vi you press `:` to get one, and until you do the
    // buffer looks exactly as it does in INSERT. Painting `:` for the whole of
    // NORMAL said "you are typing a command" to an operator who was not, and
    // the way out of it looked like backing out of a command rather than
    // pressing `i`. The mode is carried where vi carries it: the status line
    // (`vi --NORMAL--` in the header) and the mode hint.
    Line::from(Span::styled("❯ ", Style::default().fg(accent)))
}

/// The `:`-command text to render on a dedicated **bottom** row (vi-style) —
/// only when an ex command is open AND the buffer spans multiple lines (#531).
/// On a single line the inline chevron↔`:` swap is kept (see `prompt_line`).
fn ex_bottom_line(editor: &Editor, textarea: &TextArea) -> Option<String> {
    editor
        .ex()
        .filter(|_| textarea.lines().len() > 1)
        .map(str::to_string)
}

/// One surface-owned row for every currently live harness task. The shared
/// spinner alphabet is advanced by the rich input loop's existing 250 ms
/// repaint; no worker thread writes terminal bytes.
fn background_line(jobs: &[BackgroundJob], frame: usize) -> Option<Line<'static>> {
    let active = jobs
        .iter()
        .filter(|job| job.is_running())
        .map(BackgroundJob::label)
        .collect::<Vec<_>>();
    if active.is_empty() {
        return None;
    }
    let count = if active.len() == 1 {
        String::new()
    } else {
        format!(" ({})", active.len())
    };
    let labels = active.join(", ");
    let spinner = newt_core::tty::SPINNER_FRAMES[frame % newt_core::tty::SPINNER_FRAMES.len()];
    Some(Line::from(vec![
        Span::styled(
            format!("{spinner} background{count}"),
            Style::default().fg(crate::theme::color(crate::theme::Role::Thinking)),
        ),
        Span::styled(
            format!(" · {labels}"),
            Style::default().fg(crate::theme::color(crate::theme::Role::Dim)),
        ),
    ]))
}

fn background_frame() -> usize {
    chrono::Local::now().timestamp_subsec_millis() as usize / 100
}

#[derive(Default)]
struct RichStatus<'a> {
    model: &'a str,
    endpoint: &'a str,
    gauge: Option<(u32, u32)>,
    /// #1671: the conversation's display name — title, `#shortid`, or
    /// "ephemeral". Empty hides the span.
    session: &'a str,
    /// The modal's rendered lines, or `None` when none is open.
    ///
    /// Lines rather than a widget: the surface OWNS where a modal goes, and a
    /// caller that could hand over a widget could hand over one that positions
    /// itself — which is the freedom this design exists to remove.
    modal: Option<&'a [Line<'static>]>,
    /// The header's free text beside the name: what this conversation IS.
    ///
    /// Separate from `session` because they answer different questions — the
    /// name is an identifier the operator can type at `/tab`, the headline is
    /// prose. Empty renders nothing rather than an empty bracket.
    headline: &'a str,
    background_jobs: &'a [BackgroundJob],
    /// The slash-command palette (#1674), rendered above the input row while
    /// open. `None`/closed → the layout is byte-identical to the pre-palette
    /// surface.
    palette: Option<&'a PaletteState>,
    /// #1669 PR-B: the open tabs. Fewer than two → **no bar row at all**, so a
    /// single-conversation session is byte-identical to the pre-bar surface.
    tabs: &'a [crate::tab_bar::TabCell],
    /// A blocking modal owns the keyboard while the mounted chat editor stays
    /// visible underneath it. Only the marker recedes; the draft remains.
    chat_inactive: bool,
    /// #2006: whether a turn is running, which is what decides whether the
    /// mode hint may advertise `^C interrupt`.
    turn_running: bool,
}

fn draw(
    f: &mut Frame,
    textarea: &TextArea,
    editor: &Editor,
    gutter: Option<u16>,
    status: RichStatus<'_>,
) {
    let area = f.area();
    let input_focused = !status.chat_inactive;
    // A wide-gutter TextArea paints its own reversed cursor cell. Neutralize
    // that projection while a modal owns the keyboard, just as the overhang
    // path below suppresses the real terminal cursor.
    let inactive_textarea = status.chat_inactive.then(|| {
        let mut view = textarea.clone();
        view.set_cursor_style(Style::default());
        view
    });
    let textarea = inactive_textarea.as_ref().unwrap_or(textarea);
    // "active" = the line has content, so the header mode word brightens and the
    // dim mode hint clears as you type. The hint shows only on an empty line.
    let empty = buffer_is_empty(textarea);
    // SIX rows, top to bottom: activity, header, textarea, modal, footer, tabs.
    //
    //   ⠋ thinking                              only while something runs
    //   [session] a conversation                identity
    //   ❯ what you are typing                   the textarea
    //     a modal, when one is open             beneath the prompt
    //   [18:43] vi --INSERT-- model  28k/28k    machine state, lower-left
    //   [1 chat][2 review]                      only with two or more tabs
    //
    // The activity row leads because it is the thing that APPEARS and
    // disappears; putting a row that comes and goes between the input and its
    // footer would shift both under the operator's hands. The footer anchors
    // last because an English reader's eye returns to the lower left.
    let activity = background_line(status.background_jobs, background_frame());
    // #1669 PR-B: the tab bar is the OUTERMOST row — below the clock, at the
    // physical bottom edge. The frame nests outward: body, prompt, modal,
    // this-tab's machine state, then the set of tabs. Everything above the bar
    // belongs to the selected tab, so the bar is the container and sits
    // outside its contents. Zero rows for fewer than two tabs, unchanged, so a
    // single-conversation session still ends at the clock.
    let tab_rows = crate::tab_bar::bar_rows(status.tabs);
    // The header and the modal slot GROW, the way the input already does. The
    // clock does not: it is the anchor, always the last row, which is what
    // makes the whole block readable from one fixed place.
    let header_rows = header_height(status.session, status.headline, area.width);
    let modal_rows = modal_height(status.modal, area.width);
    let rows = [
        Constraint::Length(u16::from(activity.is_some())),
        Constraint::Length(header_rows),
        Constraint::Min(1),
        Constraint::Length(modal_rows),
        Constraint::Length(1),
        Constraint::Length(tab_rows),
    ];
    let [activity_area, header_area, body_area, modal_area, footer_area, tab_area] =
        Layout::vertical(rows).areas(area);
    // **A modal appears in ONE place, every time.** Beneath the prompt, above
    // the clock — it never floats, never centres, never follows the cursor.
    // A dialog that moves is one the operator has to FIND, and having to hunt
    // for the thing demanding an answer is the whole complaint against
    // editors that place them dynamically.
    //
    // It grows downward from the input and pushes the block's top into
    // scrollback, so the prompt stays where the hands are.
    if modal_rows > 0 {
        if let Some(lines) = status.modal {
            f.render_widget(
                Paragraph::new(lines.to_vec()).wrap(ratatui::widgets::Wrap { trim: false }),
                modal_area,
            );
        }
    }
    if tab_rows > 0 {
        if let Some(line) = crate::tab_bar::layout_tab_cells(status.tabs, tab_area.width) {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    line,
                    Style::default().fg(crate::theme::color(crate::theme::Role::Dim)),
                ))),
                tab_area,
            );
        }
    }
    if let Some(line) = activity {
        f.render_widget(Paragraph::new(line), activity_area);
    }
    f.render_widget(
        Paragraph::new(header_line(status.session, status.headline))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        header_area,
    );
    f.render_widget(
        Paragraph::new(footer_line(
            editor,
            status.model,
            status.endpoint,
            status.gauge,
            !empty && input_focused,
        )),
        footer_area,
    );
    // The slash-command palette (#1674) sits directly above the input row,
    // inside the same inline region — no second surface, no second event loop.
    let body_area = match status.palette.filter(|p| p.is_open()) {
        Some(p) if p.viewport() > 0 => {
            let rows = (p.viewport() as u16).min(body_area.height.saturating_sub(1));
            let [palette_area, rest] =
                Layout::vertical([Constraint::Length(rows), Constraint::Min(1)]).areas(body_area);
            f.render_widget(Paragraph::new(palette_lines(p)), palette_area);
            rest
        }
        _ => body_area,
    };
    // The activity row and the tab bar are their own regions above, so the
    // input keeps the whole remainder rather than giving up its last line.
    let input_area = body_area;
    let g = resolve_gutter(gutter, input_area.width);

    // #531: a `:`-command on a multi-line buffer renders on its own row at the
    // bottom of the input area, vi-style — not glued to the first row's chevron.
    // The message above keeps its normal prompt; the real cursor sits on the
    // command line.
    if let Some(ex) = ex_bottom_line(editor, textarea) {
        let [msg_area, ex_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(input_area);
        let prompt = prompt_line_with_focus(editor, false, input_focused);
        if g >= GUTTER_W {
            let [gutter_area, input] =
                Layout::horizontal([Constraint::Length(g), Constraint::Min(1)]).areas(msg_area);
            f.render_widget(Paragraph::new(prompt), gutter_area);
            f.render_widget(textarea, input);
        } else {
            draw_overhang(f, msg_area, &prompt, textarea, g, None, input_focused);
        }
        f.render_widget(
            Block::default().style(Style::default().bg(command_background(input_focused))),
            ex_area,
        );
        f.render_widget(
            Paragraph::new(command_line_with_focus(CommandKind::Ex, &ex, input_focused)),
            ex_area,
        );
        let tail_width = u16::try_from(str_width(&ex)).unwrap_or(u16::MAX);
        let cx = ex_area.x.saturating_add(1).saturating_add(tail_width);
        if input_focused && cx <= ex_area.right().saturating_sub(1) {
            f.set_cursor_position((cx, ex_area.y));
        }
        return;
    }

    // A single-line ex command owns the input row. The draft is still held in
    // the textarea for `:w`, but it must not be concatenated after `:cmd`.
    if let Some(ex) = editor.ex() {
        f.render_widget(
            Block::default().style(Style::default().bg(command_background(input_focused))),
            input_area,
        );
        f.render_widget(
            Paragraph::new(command_line_with_focus(CommandKind::Ex, ex, input_focused)),
            input_area,
        );
        let tail_width = u16::try_from(str_width(ex)).unwrap_or(u16::MAX);
        let cx = input_area.x.saturating_add(1).saturating_add(tail_width);
        if input_focused && cx <= input_area.right().saturating_sub(1) {
            f.set_cursor_position((cx, input_area.y));
        }
        return;
    }

    // `!` is a prompt marker while the line is a real shell escape, not a chat
    // character after `❯`. The render-only view removes it from the textarea so
    // it appears exactly once; the original buffer is what dispatch receives.
    let bang = bang_view(textarea);
    let shown = bang.as_ref().map_or(textarea, |view| &view.textarea);
    if let Some(bang) = bang.as_ref() {
        f.render_widget(
            Block::default().style(Style::default().bg(command_background(input_focused))),
            input_area,
        );
        let prompt = command_line_with_focus(CommandKind::Bang, "", input_focused);
        // Command syntax owns the row, so it never inherits the chat surface's
        // optional 19-column gutter. The marker and body stay adjacent at every
        // configured gutter width.
        draw_overhang(
            f,
            input_area,
            &prompt,
            shown,
            1,
            None,
            input_focused && !bang.cursor_on_marker,
        );
        if input_focused && bang.cursor_on_marker && input_area.width > 0 && input_area.height > 0 {
            f.set_cursor_position((input_area.x, input_area.y));
        }
        return;
    }
    let prompt = prompt_line_with_focus(editor, true, input_focused);
    let hint = empty.then(|| editor.mode_hint(status.turn_running));
    if g >= GUTTER_W {
        // Wide gutter (opt-in): prompt in a fixed left column, input to its
        // right — continuation lines align under the input at column `g`.
        let [gutter_area, input] =
            Layout::horizontal([Constraint::Length(g), Constraint::Min(1)]).areas(input_area);
        f.render_widget(Paragraph::new(prompt), gutter_area);
        f.render_widget(shown, input);
    } else {
        // Overhang (the default): the prompt prefixes the FIRST input row inline
        // (`❯ this`); continuation rows hang-indent by `g` columns (default 1).
        draw_overhang(
            f,
            input_area,
            &prompt,
            shown,
            g,
            hint.as_deref(),
            input_focused,
        );
    }
}

/// The dim chevron marking a committed prompt in scrollback: `›` + two spaces.
const ECHO_CHEVRON: &str = "›  ";

/// One physical row of a committed prompt echo. `lead` marks the single row that
/// carries the `›` chevron (the first row of the first logical line); every other
/// row hangs under it.
#[derive(Debug, PartialEq, Eq)]
struct EchoRow {
    lead: bool,
    text: String,
}

/// Build the body rows for a committed prompt echo, wrapped to the terminal
/// `width` so a line longer than the terminal is CARRIED onto continuation rows
/// rather than clipped at the right edge by ratatui's fixed-height paint (the
/// reported truncation bug). `hang` is the continuation indent in columns. Pure
/// so the unit tier proves no content is dropped without a terminal.
fn echo_body_rows(body: &str, hang: usize, width: usize) -> Vec<EchoRow> {
    // Mirror the interactive surface's geometry (`overhang_rows`): the first
    // logical line's first row wears the `›` chevron; later logical lines and
    // wrapped continuations hang-indent by `hang`. Reserving that marker from the
    // wrap width guarantees no rendered row overruns the terminal.
    let chevron = str_width(ECHO_CHEVRON);
    let cont_w = width.saturating_sub(hang).max(1);
    let mut rows = Vec::new();
    for (li, logical) in body.lines().enumerate() {
        let first_indent = if li == 0 { chevron } else { hang };
        let first_w = width.saturating_sub(first_indent).max(1);
        for (si, (_, seg)) in wrap_segments(logical, first_w, cont_w)
            .into_iter()
            .enumerate()
        {
            rows.push(EchoRow {
                lead: li == 0 && si == 0,
                text: seg,
            });
        }
    }
    if rows.is_empty() {
        rows.push(EchoRow {
            lead: true,
            text: String::new(),
        });
    }
    rows
}

/// Wrap the text after a command marker. Unlike a chat echo, the marker itself
/// owns the first cell; continuation rows hang two cells beneath the command
/// body. `command` may retain leading whitespace from the editor — dispatch
/// trims it, so the projection does too.
fn command_body_rows(command: &str, kind: CommandKind, width: usize) -> Vec<EchoRow> {
    let shown = command.trim_start();
    let tail = shown.strip_prefix(kind.marker()).unwrap_or(shown);
    let first_w = width.saturating_sub(1).max(1);
    let cont_w = width.saturating_sub(2).max(1);
    let mut rows = Vec::new();
    for (li, logical) in tail.lines().enumerate() {
        let line_first_w = if li == 0 { first_w } else { cont_w };
        for (si, (_, seg)) in wrap_segments(logical, line_first_w, cont_w)
            .into_iter()
            .enumerate()
        {
            rows.push(EchoRow {
                lead: li == 0 && si == 0,
                text: seg,
            });
        }
    }
    if rows.is_empty() {
        rows.push(EchoRow {
            lead: true,
            text: String::new(),
        });
    }
    rows
}

fn command_echo_lines(command: &str, kind: CommandKind, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let dim = Style::default().fg(Color::DarkGray);
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut lines = vec![Line::from(Span::styled(format!("[{stamp}]"), dim))];
    for row in command_body_rows(command, kind, width) {
        let prefix = if row.lead {
            Span::styled(
                kind.marker().to_string(),
                kind.marker_style().bg(COMMAND_BG),
            )
        } else {
            Span::styled("  ", Style::default().bg(COMMAND_BG))
        };
        let used = if row.lead { 1 } else { 2 } + str_width(&row.text);
        lines.push(Line::from(vec![
            prefix,
            Span::styled(row.text, Style::default().fg(Color::White).bg(COMMAND_BG)),
            Span::styled(
                " ".repeat(width.saturating_sub(used)),
                Style::default().bg(COMMAND_BG),
            ),
        ]));
    }
    lines
}

fn echo_command(sink: &mut dyn ScrollbackSink, command: &str, kind: CommandKind) -> io::Result<()> {
    let width = crossterm::terminal::size().map(|(c, _)| c).unwrap_or(80) as usize;
    sink.insert(command_echo_lines(command, kind, width))
}

/// Wrap a scrollback note (`:command` output) to `width`, carrying long lines
/// onto continuation rows instead of clipping them. Always returns at least one
/// row (empty for empty input), preserving blank lines. Pure for the unit tier.
fn note_rows(note: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    for line in note.lines() {
        for (_, seg) in wrap_segments(line, width, width) {
            rows.push(seg);
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// Emit a submitted turn into real scrollback (above the inline region), so the
/// conversation log shows what the user typed — the inline widget itself is
/// cleared on submit. Continuation lines hang-indent by the SAME gutter the
/// input used (default 1), so a multi-line prompt reads back exactly as typed
/// rather than being re-flowed to a different indent on submit.
fn echo_submitted(
    sink: &mut dyn ScrollbackSink,
    body: &str,
    gutter: Option<u16>,
) -> io::Result<()> {
    if is_bang_escape(body) {
        return echo_command(sink, body, CommandKind::Bang);
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let width = crossterm::terminal::size().map(|(c, _)| c).unwrap_or(80);
    let hang_cols = resolve_gutter(gutter, width) as usize;
    let hang = " ".repeat(hang_cols);
    let dim = Style::default().fg(Color::DarkGray);
    // Two-line committed form (#527): the full-datetime header on its own row,
    // then the body behind a DIMMED `›` — the live `❯` frozen into an at-rest
    // log marker. The body is WRAPPED to the terminal width (via `echo_body_rows`,
    // the same `wrap_segments` the live input surface uses), so a line wider than
    // the terminal is carried onto hang-indented continuation rows instead of
    // being clipped at the right edge. Height = header (1) + the wrapped rows.
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(format!("[{stamp}]"), dim))];
    for row in echo_body_rows(body, hang_cols, width as usize) {
        let prefix = if row.lead {
            Span::styled(ECHO_CHEVRON, dim)
        } else {
            Span::raw(hang.clone())
        };
        lines.push(Line::from(vec![prefix, Span::raw(row.text)]));
    }
    sink.insert(lines)
}

/// Print a note (one or more `\n`-separated lines, e.g. `:jumps`/`:help` output)
/// into scrollback above the input region. Each line is WRAPPED to the terminal
/// width (via `note_rows`) so a long note — e.g. a capability-denied diagnostic —
/// carries onto continuation rows instead of being clipped at the right edge.
fn echo_note(sink: &mut dyn ScrollbackSink, note: &str) -> io::Result<()> {
    let width = crossterm::terminal::size().map(|(c, _)| c).unwrap_or(80) as usize;
    let gray = Style::default().fg(Color::Gray);
    let lines: Vec<Line> = note_rows(note, width)
        .into_iter()
        .map(|seg| Line::from(Span::styled(seg, gray)))
        .collect();
    sink.insert(lines)
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
    /// Context-budget gauge `(used, budget)` for the header (Step 24.6, #559).
    gauge: Option<(u32, u32)>,
    /// Harness tasks rendered by this surface while their shared state is live.
    background_jobs: Vec<BackgroundJob>,
    /// #1669 PR-B: the open tabs, refreshed each loop head. Fewer than two →
    /// the bar renders no row.
    tabs: Vec<crate::tab_bar::TabCell>,
    /// #1671: the session display name shown in the header, refreshed per turn.
    session: String,
    /// #2006: the vi session state, parked between reads.
    ///
    /// The classic driver rebuilds its whole editor once per read *by design*,
    /// so the mode/jumplist/`;`-target cannot live on a long-lived editor the
    /// way they do under the cockpit — the surface holds them instead, and
    /// [`RichSurface::event_loop`] hands them to each mount and takes them
    /// back. `Cell` for the same reason `pending_end_quit` is one: the event
    /// loop runs behind `&self`. `Cell::take` needs only `Option`'s `Default`,
    /// so `Vi` stays non-`Clone`.
    vi: Cell<Option<Vi>>,
}

/// RAII owner of the rich surface's terminal modes: raw mode **and** bracketed
/// paste.
///
/// **#1898 — this site was recorded as SAFE and was not.** #1411 enumerated the
/// crate's raw-mode pairs and cleared `read_turn` because it "avoids `?` around
/// its event loop". That is true, and it covers the ERROR path only: the loop's
/// result was bound to a variable, so no `?` could jump the teardown. A **panic**
/// inside `event_loop` unwound straight past both restores, leaving the operator
/// raw with bracketed paste still on. A site that has been checked and cleared is
/// worse than one nobody looked at, because nobody looks twice.
///
/// # Teardown is the exact reverse of setup, and that is not incidental
///
/// Enter takes raw mode, then bracketed paste. Drop releases bracketed paste,
/// then raw mode. Mirroring is the rule a nested pair of guards would give for
/// free, and it is written out here rather than left to drop order because the
/// two modes are not independent: bracketed paste is a terminal INPUT mode, so
/// releasing raw mode first would hand line discipline back while paste markers
/// were still armed, and a paste landing in that window would deliver a literal
/// `ESC[200~` into whatever reads next. `the_guard_releases_paste_before_raw_mode`
/// pins the order against a future edit that reshuffles the Drop body.
///
/// The guard binds BEFORE the fallible call — the ordering `AltScreenGuard::enter`
/// pays for, `InlineGuard::enter` repeats and `PanelRawGuard::enter` repeats
/// again: from that point the restore is owed regardless of what the next line
/// does. `disable_raw_mode` on a terminal that never went raw is a no-op, which
/// is the cheap half of that trade.
///
/// `pub(crate)` solely for `rich_input_pty_test`, which drops it mid-unwind in a
/// child process to prove the claim above against a real tty.
pub(crate) struct RawPasteGuard {
    /// **A FIELD, and that is the ordering mechanism (#1905).** Rust runs a
    /// struct's own `Drop::drop` body BEFORE dropping its fields, so releasing
    /// bracketed paste in the body below and holding raw mode here gives
    /// paste-then-raw structurally — the order this type's doc argues for, now
    /// enforced by the language instead of by two adjacent statements someone
    /// could reorder while tidying.
    _raw: RawModeGuard,
}

impl RawPasteGuard {
    pub(crate) fn enter() -> io::Result<Self> {
        let raw = RawModeGuard::enter()?;
        // Bracketed paste: the terminal wraps a paste in escape markers and
        // delivers it as ONE `Event::Paste(text)` instead of a stream of key
        // presses. Without it, a multi-line paste arrives as Char…Enter…Char…
        // and every embedded Enter submits a line. See the `Event::Paste` arm.
        //
        // Best-effort exactly as before: a terminal that does not support it
        // is not a reason to refuse the turn.
        let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
        Ok(Self { _raw: raw })
    }
}

impl Drop for RawPasteGuard {
    fn drop(&mut self) {
        // Paste only. Raw mode is released by `_raw` AFTER this body runs —
        // see the field's doc. Releasing raw HERE instead would run first and
        // re-create the bug the order exists to prevent, which is why
        // `the_guard_releases_paste_before_raw_mode` asserts this body carries
        // no raw release at all.
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    }
}

impl RichSurface {
    pub(crate) fn new(history_path: Option<PathBuf>) -> anyhow::Result<Self> {
        // Say once, at startup, what was wrong with `NEWT_THEME`. A theme that
        // silently half-applies looks like a rendering bug everywhere except
        // the one place that would explain it — and the operator who set the
        // variable is the only person who can fix it.
        for complaint in crate::theme::complaints() {
            eprintln!("⚠ theme: {complaint}");
        }
        Ok(Self {
            edit: current_edit(),
            history_path,
            unsaved: Vec::new(),
            gutter: crate::resolve_gutter_setting(),
            pending_end_quit: Cell::new(false),
            model: String::new(),
            endpoint: String::new(),
            gauge: None,
            background_jobs: Vec::new(),
            tabs: Vec::new(),
            session: String::new(),
            vi: Cell::new(None),
        })
    }

    /// Run the inline event loop for a single turn. The terminal is taken for
    /// the duration and handed back before returning, so model output between
    /// turns prints normally into scrollback — and so `newt crew --edit` and
    /// the slash commands run against a cooked terminal (see
    /// `commands/crew.rs` and `commands/setup.rs`, which say so).
    fn read_turn(&self) -> io::Result<ReadOutcome> {
        let _guard = RawPasteGuard::enter()?;
        self.event_loop()
    }

    /// The chrome the four regions draw from — everything the surface knows
    /// that is not the editor's own state.
    pub(crate) fn chrome(&self) -> Chrome<'_> {
        Chrome {
            model: &self.model,
            endpoint: &self.endpoint,
            gauge: self.gauge,
            session: &self.session,
            // Empty for now: this change is the LAYOUT. Giving the header
            // prose in the same commit would make a re-arrangement and a new
            // content source one diff, and the second one invisible.
            headline: "",
            // The presenter still reserves its own rows above the block;
            // moving it into this slot is the next step, and doing both in one
            // change would put a layout and a modal rewrite in one diff.
            modal: None,
            background_jobs: &self.background_jobs,
            tabs: &self.tabs,
        }
    }

    /// The history the editor recalls on ↑/↓: on disk plus this session's
    /// not-yet-flushed entries, oldest first.
    pub(crate) fn history(&self) -> Vec<String> {
        let mut history = load_history(self.history_path.as_ref());
        history.extend(self.unsaved.iter().cloned());
        history
    }

    /// The editor mode, for the cockpit's persistently-mounted editor. The
    /// classic per-turn `event_loop` reads `self.edit` directly, so this
    /// accessor exists only for the unix cockpit — hence the `cfg`, without
    /// which it is dead code on the Windows (classic-only) build.
    #[cfg(unix)]
    pub(crate) fn edit(&self) -> Edit {
        self.edit
    }

    /// The gutter setting, for the cockpit (see [`RichSurface::edit`]).
    #[cfg(unix)]
    pub(crate) fn gutter(&self) -> Option<u16> {
        self.gutter
    }

    /// The `:wq` arm: the surface remembers to end-and-quit on the NEXT read.
    pub(crate) fn arm_end_quit(&self) {
        self.pending_end_quit.set(true);
    }

    /// Consume the armed `:wq` (see `arm_end_quit`). The cockpit's `ReadLine`
    /// handler checks this between turns; the classic surface consumes
    /// `pending_end_quit` inline in its own `read`, so this accessor is
    /// cockpit-only and would be dead code on the Windows classic-only build.
    #[cfg(unix)]
    pub(crate) fn take_end_quit(&self) -> bool {
        self.pending_end_quit.replace(false)
    }

    /// The classic per-turn driver: an inline viewport that lives for ONE
    /// read and is torn down on submit. The editor state it drives is the
    /// same [`MountedEditor`] the cockpit keeps mounted across turns — this
    /// is the pre-cockpit path (Windows, or a cockpit that failed to open),
    /// kept as a thin loop around it rather than a second copy of it.
    fn event_loop(&self) -> io::Result<ReadOutcome> {
        let mut cur_h = 1u16;
        // A freshly built inline terminal has a blank back-buffer, so ratatui's
        // frame diff won't rewrite cells the new frame doesn't touch — stale
        // content from a prior turn (or the smaller pre-resize region) bleeds
        // through. `clear()` forces a full repaint of the region so every turn /
        // resize starts clean.
        let mut terminal = make_terminal(cur_h)?;
        terminal.clear()?;
        // Persistent-prompt phase 1: pre-fill anything typed while the last
        // turn ran (captured as type-ahead by the keyboard watcher) so nothing
        // the user typed is lost.
        let typed_ahead = crate::type_ahead::take();
        let mut me = MountedEditor::new(
            self.edit,
            self.gutter,
            self.history(),
            typed_ahead.trim_end_matches('\n'),
        );
        // #2006: this mount is one read old, but the operator's vi mode,
        // jumplist and `;`-target are not. Adopt what the last read parked.
        if let Some(vi) = self.vi.take() {
            me.adopt_vi(vi);
        }
        let outcome = loop {
            let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
            let want = me.wanted_rows(term_w, term_h, &self.chrome());
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
            terminal.draw(|f| me.draw(f, self.chrome(), false))?;

            // 250ms timeout drives the live clock when idle.
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let evt = event::read()?;
            match me.on_event(evt, &mut terminal)? {
                None => {}
                Some(EditorOutcome::Line(body)) => break ReadOutcome::Line(body),
                Some(EditorOutcome::LineThenQuit(body)) => {
                    // Submit this turn now; the end-and-quit fires on the NEXT
                    // read once the turn has run to completion.
                    self.arm_end_quit();
                    break ReadOutcome::Line(body);
                }
                Some(EditorOutcome::EndAndQuit) => break ReadOutcome::EndAndQuit,
                Some(EditorOutcome::Tab(action)) => break ReadOutcome::Tab(action),
                Some(EditorOutcome::Eof) => break ReadOutcome::Eof,
            }
        };
        // Park the vi state for the next read (#2006). The arms above `break`
        // rather than `return` so there is exactly ONE way out of the loop and
        // no arm can be added that forgets to hand the state on.
        self.vi.set(Some(me.take_vi()));
        Ok(outcome)
    }
}

/// Everything the header, tab bar and background row draw from. Borrowed from
/// the surface per frame; the editor never stores it.
#[derive(Clone, Copy)]
pub(crate) struct Chrome<'a> {
    pub(crate) model: &'a str,
    pub(crate) endpoint: &'a str,
    pub(crate) gauge: Option<(u32, u32)>,
    pub(crate) session: &'a str,
    /// The header's prose beside the name. Empty until a caller supplies one,
    /// which keeps this change to LAYOUT rather than smuggling in new content.
    pub(crate) headline: &'a str,
    /// A modal's lines, rendered beneath the prompt and above the clock.
    pub(crate) modal: Option<&'a [Line<'static>]>,
    pub(crate) background_jobs: &'a [BackgroundJob],
    pub(crate) tabs: &'a [crate::tab_bar::TabCell],
}

/// Where the editor's committed lines go — the scrollback above the input.
///
/// The classic inline viewport implements this with ratatui's `insert_before`;
/// the cockpit implements it with its own insert, because `insert_before` is a
/// **silent no-op** on the `Fixed` viewport the cockpit uses (ratatui returns
/// `Ok(())` for anything but `Inline`) — a "compiles clean and does nothing"
/// trap this seam exists to keep out of the call sites.
pub(crate) trait ScrollbackSink {
    fn insert(&mut self, lines: Vec<Line<'static>>) -> io::Result<()>;
}

impl ScrollbackSink for Term {
    fn insert(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        let height = lines.len() as u16;
        self.insert_before(height, move |buf| {
            Paragraph::new(lines).render(buf.area, buf);
        })
    }
}

impl InputSurface for RichSurface {
    /// **The terminal adapter** (C1, #1862), rendering natively (C2, #1876).
    ///
    /// This surface owns the terminal, so it — and not the session —
    /// acquires the sealed `PromptWindow`. Suspending first is load bearing
    /// rather than tidy: it erases every registered ephemeral writer and
    /// takes stdin, so the inline frame is not drawn under a live spinner.
    ///
    /// The transient frame is erased before the canonical text is committed,
    /// which is the plain-scroller contract for every TTY-only projection —
    /// scrollback ends up holding the same canonical projection a piped run
    /// would have printed, on either surface.
    ///
    /// A rich draw that fails falls back to the plain path rather than
    /// failing the interaction: the operator is mid-turn and owed a prompt,
    /// and `plain::render` is the conformance baseline C0b established.
    fn present_interaction(
        &mut self,
        interaction: &newt_core::interaction_surface::SurfaceInteraction,
    ) -> newt_core::HumanQuestionOutcome {
        let window = newt_core::tty::Terminal::suspend_for_prompt(
            newt_core::tty::TerminalTaker::RichSurfaceModal,
        );
        let (outcome, canonical) = match crate::interaction_view::present(interaction) {
            Ok(pair) => pair,
            Err(_) => return crate::permissions::present_on_terminal(&window, interaction),
        };
        // The frame is gone by now (the guard erased it on drop); commit the
        // canonical projection so scrollback holds what a piped run would.
        let _ = window.notice(&canonical);
        outcome
    }

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

    fn set_runtime_context(
        &mut self,
        model: &str,
        endpoint: &str,
        gauge: Option<(u32, u32)>,
        session: &str,
    ) {
        // Refresh the status-header model @ endpoint each turn (#527) so a
        // mid-session `/model` switch shows up on the next prompt. The
        // context-budget gauge (24.6) and the session name (#1671) ride the
        // same per-turn refresh.
        self.model = model.to_string();
        self.endpoint = endpoint.to_string();
        self.gauge = gauge;
        self.session = session.to_string();
    }

    fn set_background_jobs(&mut self, jobs: Vec<BackgroundJob>) {
        self.background_jobs = jobs;
    }

    fn set_tabs(&mut self, tabs: Vec<crate::tab_bar::TabCell>) {
        self.tabs = tabs;
    }
}

#[cfg(test)]
#[path = "rich_input_tests/support.rs"]
mod test_support;

#[cfg(test)]
#[path = "rich_input_tests/terminal_guard.rs"]
mod terminal_guard_tests;

#[cfg(test)]
#[path = "rich_input_tests/command_rendering.rs"]
mod command_rendering_tests;

#[cfg(test)]
#[path = "rich_input_tests/geometry.rs"]
mod geometry_tests;

#[cfg(all(test, unix))]
#[path = "rich_input_tests/esc_ladder.rs"]
mod esc_ladder_tests;

#[cfg(test)]
#[path = "rich_input_tests/layout.rs"]
mod layout_tests;

#[cfg(test)]
#[path = "rich_input_tests/input_actions.rs"]
mod input_actions_tests;

#[cfg(test)]
#[path = "rich_input_tests/vi_motions.rs"]
mod vi_motions_tests;

#[cfg(test)]
#[path = "rich_input_tests/history.rs"]
mod history_tests;

#[cfg(test)]
#[path = "rich_input_tests/mounted_state.rs"]
mod mounted_state_tests;

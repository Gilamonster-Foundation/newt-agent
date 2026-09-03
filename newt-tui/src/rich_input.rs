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
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use newt_core::tty::raw_mode::RawModeGuard;
use newt_core::tty::str_width;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};
use ratatui::Frame;
use tui_textarea::{CursorMove, TextArea};

use crate::chat::BackgroundJob;
use crate::palette::{palette_lines, palette_step, PaletteState, PaletteStep};
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

type Term = crate::inline_viewport::InlineTerm;

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

/// Command rows use the same high-contrast dark slab live and in scrollback.
/// The marker color carries the command family; the body stays neutral so a
/// shell command remains easy to audit character-for-character.
const COMMAND_BG: Color = Color::Rgb(82, 82, 82);
/// A command draft stays recognizable behind a blocking modal, but no longer
/// competes with the modal for visual focus.
const INACTIVE_COMMAND_BG: Color = Color::Rgb(45, 45, 45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandKind {
    Bang,
    Ex,
}

impl CommandKind {
    fn marker(self) -> char {
        match self {
            Self::Bang => '!',
            Self::Ex => ':',
        }
    }

    fn marker_style(self) -> Style {
        self.marker_style_with_focus(true)
    }

    fn marker_style_with_focus(self, focused: bool) -> Style {
        let color = if focused {
            match self {
                Self::Bang => crate::theme::color(crate::theme::Role::CommandBang),
                Self::Ex => crate::theme::color(crate::theme::Role::CommandEx),
            }
        } else {
            Color::DarkGray
        };
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    }
}

fn command_line(kind: CommandKind, tail: &str) -> Line<'static> {
    command_line_with_focus(kind, tail, true)
}

fn command_line_with_focus(kind: CommandKind, tail: &str, focused: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            kind.marker().to_string(),
            kind.marker_style_with_focus(focused),
        ),
        Span::styled(
            tail.to_string(),
            Style::default().fg(if focused {
                Color::White
            } else {
                Color::DarkGray
            }),
        ),
    ])
}

fn command_background(focused: bool) -> Color {
    if focused {
        COMMAND_BG
    } else {
        INACTIVE_COMMAND_BG
    }
}

fn is_bang_escape(body: &str) -> bool {
    // Keep this display classifier on the exact dispatch rule. In particular,
    // bare `!` is ordinary model text and a colon is never promoted here.
    crate::bang_command(body.trim()).is_some()
}

/// Number of characters hidden behind the live `!` prompt marker. The chat
/// dispatcher trims before applying `bang_command`, so leading whitespace is
/// projected away here too; the underlying editor buffer remains untouched.
fn bang_prefix_chars(body: &str) -> Option<usize> {
    if !is_bang_escape(body) {
        return None;
    }
    let marker = body.chars().position(|c| !c.is_whitespace())?;
    (body.chars().nth(marker) == Some('!')).then_some(marker + 1)
}

fn cursor_offset(lines: &[String], cursor: (usize, usize)) -> usize {
    lines
        .iter()
        .take(cursor.0)
        .map(|line| line.chars().count() + 1)
        .sum::<usize>()
        + cursor.1
}

fn cursor_at_offset(lines: &[String], mut offset: usize) -> (usize, usize) {
    for (row, line) in lines.iter().enumerate() {
        let width = line.chars().count();
        if offset <= width {
            return (row, offset);
        }
        offset = offset.saturating_sub(width + 1);
    }
    let row = lines.len().saturating_sub(1);
    (row, lines.get(row).map_or(0, |line| line.chars().count()))
}

/// A render-only textarea with the bang (and any dispatch-trimmed leading
/// whitespace) removed. Editing and submission still use the original buffer;
/// only the projection changes, so shell routing and exact command bytes do not.
struct BangView<'a> {
    textarea: TextArea<'a>,
    /// The source caret is on whitespace or `!` hidden by this projection.
    /// Render it on the visible marker instead of after that marker.
    cursor_on_marker: bool,
}

fn bang_view<'a>(textarea: &TextArea<'a>) -> Option<BangView<'a>> {
    let body = textarea.lines().join("\n");
    let hidden = bang_prefix_chars(&body)?;
    // The projection below cannot faithfully show a selection that crosses
    // characters hidden behind the marker. Production input normalizes that
    // state before and after every edit; refuse to fabricate a deselected
    // clone if a caller violates the invariant.
    if textarea.is_selecting() {
        return None;
    }
    let original_offset = cursor_offset(textarea.lines(), textarea.cursor());
    let mut view = textarea.clone();
    view.move_cursor(CursorMove::Jump(0, 0));
    for _ in 0..hidden {
        view.delete_next_char();
    }
    let cursor = cursor_at_offset(view.lines(), original_offset.saturating_sub(hidden));
    view.move_cursor(CursorMove::Jump(cursor.0 as u16, cursor.1 as u16));
    Some(BangView {
        textarea: view,
        cursor_on_marker: original_offset < hidden,
    })
}

/// A bang row is rendered manually after hiding its source `!`, so it cannot
/// expose `tui-textarea`'s selection highlight. Do not leave an invisible
/// selection armed: typing into one would otherwise replace text the operator
/// cannot see as selected. Keeping both the source and projection unselected
/// is the smallest behavior that makes displayed and editing state agree.
fn cancel_hidden_bang_selection(textarea: &mut TextArea<'_>) {
    if textarea.is_selecting() && is_bang_escape(&textarea.lines().join("\n")) {
        textarea.cancel_selection();
    }
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
        let mut hard_end = start;
        while hard_end < chars.len() {
            // Width is contextual for emoji presentation selectors and ZWJ
            // sequences, so summing scalar widths is not equivalent to the
            // terminal width of the candidate substring.
            let candidate: String = chars[start..=hard_end].iter().collect();
            if str_width(&candidate) > avail && hard_end > start {
                break;
            }
            // Always take at least one scalar so a glyph wider than a
            // one-column budget cannot stall the wrapper. Zero-width combining
            // marks naturally stay with the preceding cell.
            hard_end += 1;
        }
        if hard_end == chars.len() {
            segs.push((start, chars[start..].iter().collect()));
            break;
        }
        // Prefer breaking just after the last space in range (word wrap); else
        // hard-break at the cell budget. Force ≥1 char of progress.
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
                    let cursor_chars = ccol.saturating_sub(seg_start);
                    let cursor_prefix: String = seg_text.chars().take(cursor_chars).collect();
                    let cursor_cells = str_width(&cursor_prefix);
                    cx = indent + cursor_cells as u16;
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
    show_cursor: bool,
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
    if show_cursor
        && cur_x <= area.right().saturating_sub(1)
        && cur_y <= area.bottom().saturating_sub(1)
    {
        f.set_cursor_position((cur_x, cur_y));
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

/// The note an exit key earns while a turn is running (#2010): the key is
/// inert here, and silence was the defect.
const TURN_RUNNING_EXIT_NOTE: &str =
    "turn running — Ctrl-C interrupts it · Ctrl-D exits at the prompt";

/// What one event did, when it did something the driver must act on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EditorOutcome {
    /// A submitted line.
    Line(String),
    /// `:wq`: submit this line, and end-and-quit on the read after it.
    LineThenQuit(String),
    /// `:wq` on an empty buffer — nothing to send; end and quit now.
    EndAndQuit,
    /// A tab motion (`gt` …) — the session owns what happens next.
    Tab(crate::tabs::TabAction),
    /// Ctrl-D on an empty buffer, or the mode-idiomatic exit.
    Eof,
}

/// The editor as it stands between events — what the per-turn loop used to
/// keep as locals, lifted so it can stay MOUNTED across turns (#1669: the
/// cockpit) as well as be driven for one read at a time.
///
/// Owns no terminal and no chrome. It computes the rows it needs, draws into
/// whatever frame it is handed, and turns events into [`EditorOutcome`]s; the
/// two drivers differ only in what they wrap around those three calls.
pub(crate) struct MountedEditor {
    edit: Edit,
    gutter: Option<u16>,
    textarea: TextArea<'static>,
    editor: Editor,
    /// The exact `:wq`/`:x` spelling held while its confirmation is visible.
    /// The vi state machine consumes the ex line before the answer arrives, but
    /// durable scrollback must commit it only after an affirmative answer.
    pending_ex_echo: Option<String>,
    /// ↑/↓ history recall (the rustyline behavior the rich surface had
    /// dropped): oldest first. `hist_pos == len` means "the fresh line"; `↑`
    /// walks backward into older entries, `↓` forward, restoring the stashed
    /// in-progress line at the end.
    history: Vec<String>,
    hist_pos: usize,
    stash: String,
    /// The slash-command palette (#1674): fed by buffer edits, rendered above
    /// the input row by `draw`. Opens on `/` at an empty prompt; filters as
    /// you type; ↑/↓ (C-p/C-n) move; Tab/Enter complete (never submit); Esc
    /// closes.
    palette: PaletteState,
    /// Whether a harness turn is running right now — the one condition the
    /// mode hint's `^C` half is allowed to depend on (#2006). The cockpit
    /// mirrors its `turn` here; the classic driver leaves it `false`, which is
    /// correct there because that driver's editor only exists between turns.
    turn_running: bool,
}

impl MountedEditor {
    pub(crate) fn new(
        edit: Edit,
        gutter: Option<u16>,
        history: Vec<String>,
        prefill: &str,
    ) -> Self {
        let textarea = if prefill.is_empty() {
            new_textarea(edit)
        } else {
            textarea_with(edit, prefill)
        };
        let hist_pos = history.len();
        let mut palette = PaletteState::from_corpus();
        // A `/` that arrived as a PREFILL (typed while the last turn ran) must
        // open the palette exactly as a live keypress would — run the same
        // buffer sync over it (review of #1674). A longer prefilled `/cmd…`
        // line follows the same rule as any non-`/` edit: it does not open.
        palette.on_buffer_change("", &textarea.lines().join("\n"));
        Self {
            edit,
            gutter,
            textarea,
            editor: Editor::new(edit),
            pending_ex_echo: None,
            history,
            hist_pos,
            stash: String::new(),
            palette,
            turn_running: false,
        }
    }

    /// Adopt the vi state the mount this one replaces was carrying (#2006).
    ///
    /// Two drivers rebuild a live editor and must not cost the operator their
    /// mode, jumplist or `;`/`,` target while doing it: the classic
    /// per-read loop, which is torn down and rebuilt every turn *by design*,
    /// and `SurfaceRequest::Reload`, which already carries the draft across
    /// the same seam. The cockpit's steady state needs neither — its editor
    /// stays mounted.
    pub(crate) fn adopt_vi(&mut self, vi: Vi) {
        self.editor.vi = vi;
    }

    /// Hand this mount's vi state to the mount replacing it; see
    /// [`MountedEditor::adopt_vi`]. What is left behind is a fresh `Vi`, so a
    /// spent mount cannot keep driving stale state.
    pub(crate) fn take_vi(&mut self) -> Vi {
        std::mem::replace(&mut self.editor.vi, Vi::new())
    }

    /// Which Esc-ladder claimants this mount has live right now (#2005).
    ///
    /// The registration point for every Esc consumer inside the editor, and
    /// the reason the presenter's arm is one predicate instead of a call
    /// ordering across five files. A new surface that wants Esc — PR8's rich
    /// `/settings` shell is the next one — adds a row to
    /// `assets/esc_ladder.toml` and a line here, rather than a sixth
    /// independent `event::read()` loop; the conformance test below fails the
    /// PR if it adds the row and forgets the line.
    ///
    /// Cockpit-only, hence the `cfg`: the classic driver's editor does not
    /// exist while a turn runs, so it has nothing to rank.
    #[cfg(unix)]
    // `unix` with the ladder it feeds (`lib.rs` `mod esc_ladder`): the only
    // non-test consumer is the cockpit presenter, whose live half is unix-only,
    // so on Windows this accessor would compile with no caller and `-D
    // warnings` would fail the build on the dead code.
    #[cfg(unix)]
    pub(crate) fn claim_set(&self) -> precedence_ladder::ClaimSet {
        let mut c = precedence_ladder::ClaimSet::default();
        if self.palette.is_open() {
            c.claiming("palette");
        }
        self.editor.claims(&mut c);
        c
    }

    /// Tell the editor whether a turn is running, for the `^C` half of the
    /// mode hint (#2006). Cockpit-only: the classic driver's editor is not
    /// alive during a turn, so its `false` is already the truth — hence the
    /// `cfg`, without which this is dead code on the Windows classic build.
    #[cfg(unix)]
    pub(crate) fn set_turn_running(&mut self, running: bool) {
        self.turn_running = running;
    }

    /// Replace the history (a `/vi`·`/emacs` reload rebuilds the editor; the
    /// cockpit refreshes after each `add_history`). Only the cockpit keeps an
    /// editor mounted long enough to mutate its history in place — the classic
    /// loop rebuilds per turn — so this is `cfg(unix)` to stay off the dead-code
    /// list on the Windows classic-only build.
    #[cfg(unix)]
    pub(crate) fn set_history(&mut self, history: Vec<String>) {
        self.hist_pos = history.len();
        self.history = history;
    }

    /// The current draft, for a driver that must keep it across a rebuild.
    /// Cockpit-only (the classic loop never rebuilds a live editor); see
    /// [`MountedEditor::set_history`].
    #[cfg(unix)]
    pub(crate) fn draft(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// The rows the whole inline region needs at this size: the input rows,
    /// the ex-line row, the header, an optional background row, the tab bar
    /// and the palette viewport — clamped to the terminal height, because an
    /// inline region taller than the screen scrolls the whole surface into
    /// scrollback on every redraw. Also sizes the palette viewport.
    pub(crate) fn wanted_rows(&mut self, term_w: u16, term_h: u16, chrome: &Chrome<'_>) -> u16 {
        // The prompt is always inline — on the first row, either in a wide
        // left gutter or as an overhang prefix — so it never needs a row of
        // its own. The overhang path soft-wraps, so the height is the WRAPPED
        // row count (not the logical-line count); the wide-gutter widget path
        // is one row per logical line.
        // #531: a multi-line `:`-command reserves an extra bottom row.
        let ex_extra = u16::from(ex_bottom_line(&self.editor, &self.textarea).is_some());
        let single_line_ex = self.editor.ex().is_some() && ex_extra == 0;
        let bang = self
            .editor
            .ex()
            .is_none()
            .then(|| bang_view(&self.textarea))
            .flatten();
        let shown = bang.as_ref().map_or(&self.textarea, |view| &view.textarea);
        let rows = if single_line_ex {
            1
        } else if bang.is_some() {
            overhang_rows(
                &command_line(CommandKind::Bang, ""),
                shown.lines(),
                shown.cursor(),
                1,
                term_w,
                None,
            )
            .0
            .len() as u16
        } else if resolve_gutter(self.gutter, term_w) >= GUTTER_W {
            shown.lines().len() as u16
        } else {
            let empty = buffer_is_empty(shown);
            let prompt = prompt_line(&self.editor, ex_extra == 0);
            overhang_rows(
                &prompt,
                shown.lines(),
                shown.cursor(),
                resolve_gutter(self.gutter, term_w),
                term_w,
                empty
                    .then(|| self.editor.mode_hint(self.turn_running))
                    .as_deref(),
            )
            .0
            .len() as u16
        };
        // #531 ex-bottom row + the two chrome rows (header above the input,
        // footer below it) + an optional activity row all contribute to the
        // inline viewport.
        let background_extra =
            u16::from(chrome.background_jobs.iter().any(BackgroundJob::is_running));
        // #1669 PR-B: the tab bar is the LAST row of the inline region —
        // bottom-anchored, below the background row. 0 rows for fewer than
        // two tabs, which is what keeps the single-conversation surface
        // byte-identical.
        let tab_extra = crate::tab_bar::bar_rows(chrome.tabs);
        // `+ 2`, not `+ 1`: the header and the footer are separate regions
        // now. Counting one would let the footer eat the input's last row on
        // a tight terminal — which is precisely how the layout change first
        // showed up in the tests.
        let base = (rows + ex_extra).clamp(1, MAX_INPUT_ROWS) + 2 + background_extra + tab_extra;
        // #1674: the palette viewport gets what the terminal can spare
        // above the input (capped inside `viewport_rows`), never squeezing
        // the input's own rows. 0 while closed → the height math (and the
        // whole surface) is exactly the pre-palette shape.
        let pal_rows = self
            .palette
            .viewport_rows(term_h.saturating_sub(base + 1) as usize);
        self.palette.set_viewport(pal_rows);
        (base + pal_rows as u16).min(term_h.max(1))
    }

    pub(crate) fn draw(&self, f: &mut Frame, chrome: Chrome<'_>, chat_inactive: bool) {
        draw(
            f,
            &self.textarea,
            &self.editor,
            self.gutter,
            RichStatus {
                tabs: chrome.tabs,
                model: chrome.model,
                endpoint: chrome.endpoint,
                gauge: chrome.gauge,
                session: chrome.session,
                headline: chrome.headline,
                modal: chrome.modal,
                background_jobs: chrome.background_jobs,
                palette: Some(&self.palette),
                chat_inactive,
                turn_running: self.turn_running,
            },
        );
    }

    /// Feed one terminal event. `Some` when the driver must act; `None` when
    /// the editor absorbed it (a redraw is always due afterwards).
    pub(crate) fn on_event(
        &mut self,
        evt: Event,
        sink: &mut dyn ScrollbackSink,
    ) -> io::Result<Option<EditorOutcome>> {
        // Bracketed paste: insert the whole block at the cursor — newlines
        // become real line breaks in the buffer, and NOTHING is submitted
        // (only an explicit Enter keypress submits). Normalize CRLF/CR so a
        // paste from any platform lands as clean `\n` lines.
        if let Event::Paste(text) = evt {
            let before = self.textarea.lines().join("\n");
            cancel_hidden_bang_selection(&mut self.textarea);
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            self.textarea.insert_str(normalized);
            cancel_hidden_bang_selection(&mut self.textarea);
            // A paste can open the palette only as a literal lone `/`;
            // pasting anything into an open palette re-filters or (multi-
            // line / slash gone) closes it.
            self.palette
                .on_buffer_change(&before, &self.textarea.lines().join("\n"));
            return Ok(None);
        }
        let Event::Key(key) = evt else {
            return Ok(None);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(None);
        }
        // A bang row has no visible selection projection. Clear any lingering
        // selection before the key can destructively replace hidden text, then
        // normalize again after Shift-based motions below.
        cancel_hidden_bang_selection(&mut self.textarea);
        // #1674: the palette sees every key FIRST (before history recall,
        // so ↑/↓ move the highlight, not the history). The decision is
        // the pure `palette_step` — the loop only acts on its verdict, so
        // the interception contracts are unit-tested in palette.rs.
        match palette_step(&mut self.palette, &key) {
            PaletteStep::Swallowed => return Ok(None),
            PaletteStep::CompleteTo(text) => {
                // A COMPLETION into the prompt — never a submit.
                self.textarea = textarea_with(self.edit, &text);
                return Ok(None);
            }
            PaletteStep::PassThrough => {}
        }
        // History recall on ↑/↓ — but only at a vertical edge of the buffer
        // (top row for ↑, bottom row for ↓) so multi-line cursor movement
        // still works, and never while a `:` ex-line or `[y/N]` confirm is
        // open. Plain arrows only (modified arrows fall through to editing).
        if matches!(key.code, KeyCode::Up | KeyCode::Down)
            && key.modifiers.is_empty()
            && self.editor.ex().is_none()
            && self.editor.confirm_prompt().is_none()
            && !self.history.is_empty()
        {
            let (row, _) = self.textarea.cursor();
            let last_row = self.textarea.lines().len().saturating_sub(1);
            let at_edge = (key.code == KeyCode::Up && row == 0)
                || (key.code == KeyCode::Down && row == last_row);
            if at_edge {
                let up = key.code == KeyCode::Up;
                if let Some(next) = history_step(self.hist_pos, self.history.len(), up) {
                    // Stash the in-progress line when first leaving it.
                    if self.hist_pos == self.history.len() {
                        self.stash = self.textarea.lines().join("\n");
                    }
                    self.hist_pos = next;
                    let content = if self.hist_pos == self.history.len() {
                        self.stash.clone()
                    } else {
                        self.history[self.hist_pos].clone()
                    };
                    self.textarea = textarea_with(self.edit, &content);
                }
                return Ok(None);
            }
        }
        let before = self.textarea.lines().join("\n");
        // Capture the ex-line before `Enter` consumes it. It is UI control, not
        // model text, so commit it with command chrome before any resulting
        // note or submitted draft. Colon text typed in INSERT never populates
        // `editor.ex()` and therefore cannot enter this path.
        let executed_ex = (key.code == KeyCode::Enter
            && !key.modifiers.contains(KeyModifiers::SHIFT))
        .then(|| self.editor.ex().map(str::to_string))
        .flatten();
        let confirmation_was_pending = self.editor.confirm_prompt().is_some();
        let step = match self.editor.input(key, &mut self.textarea) {
            // #2010: an exit key (Ctrl-D, nano `^X`, emacs `C-x C-c`, vi
            // `:q`) while a turn runs has nobody to deliver an EOF to — the
            // session is not reading — and used to be dropped on the floor.
            // Answer it at press time, through the note channel, with where
            // exit and interrupt actually live. Whether it should instead
            // escalate to an interrupt is the operator's call (#2010 item 3),
            // so this pins only that the press is heard.
            Step::Eof if self.turn_running => {
                self.editor.vi.msg = Some(TURN_RUNNING_EXIT_NOTE.to_string());
                Step::Continue
            }
            step => step,
        };
        cancel_hidden_bang_selection(&mut self.textarea);
        if let Some(ex) = executed_ex {
            // `:wq`/`:x` has only requested confirmation at this point; it has
            // not executed yet. Do not put an unconfirmed command in durable
            // scrollback. Immediate ex commands (including help, write and
            // quit) are committed before any note or submitted draft.
            if self.editor.confirm_prompt().is_none() {
                echo_command(sink, &format!(":{ex}"), CommandKind::Ex)?;
            } else {
                self.pending_ex_echo = Some(format!(":{ex}"));
            }
        }
        if confirmation_was_pending && self.editor.confirm_prompt().is_none() {
            let pending = self.pending_ex_echo.take();
            if step == Step::SubmitQuit {
                if let Some(ex) = pending {
                    echo_command(sink, &ex, CommandKind::Ex)?;
                }
            }
        }
        // A command (e.g. `:jumps`) may have queued a note to print above the
        // input region, into real scrollback.
        if let Some(note) = self.editor.take_msg() {
            echo_note(sink, &note)?;
        }
        Ok(match step {
            Step::Continue => {
                // #1674: track the edit — `/` typed at an empty prompt
                // opens the palette; edits re-filter it; backspacing the
                // leading `/` (or clearing the line) closes it.
                self.palette
                    .on_buffer_change(&before, &self.textarea.lines().join("\n"));
                None
            }
            Step::Submit => {
                let body = self.textarea.lines().join("\n");
                if body.trim().is_empty() {
                    return Ok(None);
                }
                echo_submitted(sink, &body, self.gutter)?;
                self.reset_after_submit();
                Some(EditorOutcome::Line(body))
            }
            Step::SubmitQuit => {
                let body = self.textarea.lines().join("\n");
                // `:wq` on an empty buffer has nothing to send — treat it
                // as a plain `:q` (end + quit, no turn).
                if body.trim().is_empty() {
                    return Ok(Some(EditorOutcome::EndAndQuit));
                }
                echo_submitted(sink, &body, self.gutter)?;
                self.reset_after_submit();
                Some(EditorOutcome::LineThenQuit(body))
            }
            // #1669 16.3: a tab motion leaves the editor immediately —
            // it is not an edit, and the session owns what happens next.
            // The buffer is left intact so the draft survives the switch.
            Step::Tab(action) => Some(EditorOutcome::Tab(action)),
            Step::Eof => Some(EditorOutcome::Eof),
        })
    }

    /// A submitted line is committed to scrollback; the editor starts fresh
    /// for the next one. The classic driver tears the whole editor down here
    /// anyway; the cockpit keeps it mounted, so it must reset explicitly.
    ///
    /// #2006: this used to be `self.editor = Editor::new(self.edit)`, which
    /// reset the vi mode, jumplist and `;`/`,` target as debris from a rebuild
    /// rather than as a decision. What a submit ends is now enumerated in one
    /// place — [`Editor::reset_for_new_line`].
    fn reset_after_submit(&mut self) {
        self.textarea = new_textarea(self.edit);
        self.editor.reset_for_new_line();
        self.pending_ex_echo = None;
        self.hist_pos = self.history.len();
        self.stash.clear();
        self.palette = PaletteState::from_corpus();
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
mod tests {
    /// PRODUCTION source of this file — see [`crate::production_source`] for
    /// why the cut is at the test MODULE and why a missing marker panics.
    fn production() -> &'static str {
        crate::production_source(include_str!("rich_input.rs"))
    }

    /// **The structural half of #1898.** The PTY test proves the guard
    /// restores; it cannot prove `read_turn` uses it, because driving the
    /// event loop needs a real interactive turn. A guard that is correct and
    /// unused is exactly the state this file was in — the restore existed, it
    /// just was not owed from anywhere a panic respects.
    #[test]
    fn raw_and_paste_are_owned_by_one_guard() {
        let src = production();
        // #1905 subsumed the raw half onto `RawModeGuard`, so this file
        // reaches crossterm's process-global not at all. The count that used
        // to be "exactly one" is now "none": the ONE nesting-aware owner is in
        // newt-core, and a bare call reappearing here would be a second owner
        // restoring to a fixed state instead of to what it found.
        assert_eq!(
            src.matches("enable_raw_mode()").count(),
            0,
            "raw mode comes from RawModeGuard, never from crossterm directly"
        );
        assert_eq!(
            src.matches("disable_raw_mode();").count(),
            0,
            "…and is released by the field, never by a statement here"
        );
        assert!(
            src.contains("_raw: RawModeGuard"),
            "RawPasteGuard must HOLD a RawModeGuard — composition, not a \
             reimplementation"
        );
        assert_eq!(
            src.matches("EnableBracketedPaste)").count(),
            1,
            "bracketed paste is enabled in exactly one place"
        );
        assert_eq!(
            src.matches("DisableBracketedPaste)").count(),
            1,
            "…and disabled in exactly one place"
        );
        assert!(
            src.contains("impl Drop for RawPasteGuard"),
            "the restore must be a Drop obligation, not a method someone has \
             to remember to call — which is what #1411 cleared this site for"
        );
    }

    /// **The ordering the issue asks to settle explicitly.** Teardown mirrors
    /// setup: paste off, then raw off. Bracketed paste is a terminal INPUT
    /// mode, so releasing raw mode first would hand line discipline back with
    /// paste markers still armed. Pinned here because a Drop body is exactly
    /// the kind of two-line block someone reorders while tidying.
    #[test]
    fn the_guard_releases_paste_before_raw_mode() {
        let src = production();
        let drop_impl = src
            .split_once("impl Drop for RawPasteGuard")
            .expect("the guard must restore from Drop")
            .1;
        let body = &drop_impl[..drop_impl.find("\n}").unwrap_or(drop_impl.len())];
        // THE MECHANISM CHANGED, THE CONTRACT DID NOT (#1905). Raw mode is no
        // longer released by a statement in this body; it is released by the
        // `_raw: RawModeGuard` field, which Rust drops AFTER this body runs.
        // So the assertion is structural: paste here, raw as a field, and NO
        // raw release in the body — a `disable_raw_mode()` back in here would
        // run FIRST and invert the order.
        assert!(
            body.contains("DisableBracketedPaste"),
            "Drop must release bracketed paste in its own body"
        );
        assert!(
            !body.contains("disable_raw_mode();"),
            "releasing raw mode in the body would run BEFORE the field drops, \
             handing line discipline back with paste markers still armed"
        );
        assert!(
            src.contains("_raw: RawModeGuard"),
            "raw mode must be a FIELD, so it drops after the body"
        );
    }

    /// **The language rule the ordering now rests on** (#1905).
    ///
    /// The test above asserts a STRUCTURE — paste in the Drop body, raw mode
    /// in a field — and that only implies the right order if a struct's own
    /// `Drop::drop` runs before its fields drop. It does; this pins it here
    /// rather than leaving the contract resting on a fact nobody in this repo
    /// has checked.
    #[test]
    fn a_drop_body_runs_before_its_fields_drop() {
        use std::cell::RefCell;
        thread_local! {
            static ORDER: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
        }
        struct Field;
        impl Drop for Field {
            fn drop(&mut self) {
                ORDER.with(|o| o.borrow_mut().push("field"));
            }
        }
        struct Outer {
            _f: Field,
        }
        impl Drop for Outer {
            fn drop(&mut self) {
                ORDER.with(|o| o.borrow_mut().push("body"));
            }
        }
        drop(Outer { _f: Field });
        ORDER.with(|o| assert_eq!(*o.borrow(), ["body", "field"]));
    }

    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        batches: Vec<Vec<Line<'static>>>,
    }

    impl ScrollbackSink for RecordingSink {
        fn insert(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
            self.batches.push(lines);
            Ok(())
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn rendered_row(
        textarea: &TextArea,
        editor: &Editor,
        width: u16,
        height: u16,
        row: u16,
    ) -> (String, Vec<Style>) {
        rendered_row_with(textarea, editor, width, height, row, Some(1), false)
    }

    fn rendered_row_with(
        textarea: &TextArea,
        editor: &Editor,
        width: u16,
        height: u16,
        row: u16,
        gutter: Option<u16>,
        chat_inactive: bool,
    ) -> (String, Vec<Style>) {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| {
            draw(
                f,
                textarea,
                editor,
                gutter,
                RichStatus {
                    chat_inactive,
                    ..RichStatus::default()
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let text = (0..width)
            .map(|x| buf.cell((x, row)).unwrap().symbol().to_string())
            .collect::<String>();
        let styles = (0..width)
            .map(|x| buf.cell((x, row)).unwrap().style())
            .collect();
        (text, styles)
    }

    fn rendered_cursor_with(
        textarea: &TextArea,
        editor: &Editor,
        width: u16,
        height: u16,
        gutter: Option<u16>,
    ) -> (u16, u16) {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| {
            draw(f, textarea, editor, gutter, RichStatus::default());
        })
        .unwrap();
        let cursor = term.get_cursor_position().unwrap();
        (cursor.x, cursor.y)
    }

    #[test]
    fn special_command_rendering_live_bang_replaces_the_chat_chevron() {
        let editor = emacs_editor();
        let textarea = TextArea::new(vec!["! date".to_string()]);

        let (row, styles) = rendered_row(&textarea, &editor, 40, 3, 1);

        assert!(
            row.starts_with("! date"),
            "bang owns the prompt cell: {row:?}"
        );
        assert!(
            !row.contains('❯'),
            "no chat chevron on a shell escape: {row:?}"
        );
        assert!(
            styles[0].bg.is_some(),
            "the live command row is visually distinct from chat"
        );
    }

    #[test]
    fn special_command_rendering_bang_never_inherits_the_chat_gutter() {
        let editor = emacs_editor();
        let textarea = TextArea::new(vec!["! date".to_string()]);

        for gutter in [None, Some(30)] {
            let (row, _) = rendered_row_with(&textarea, &editor, 80, 3, 1, gutter, false);
            assert!(
                row.starts_with("! date"),
                "gutter {gutter:?} split the marker from its command: {row:?}"
            );
        }
    }

    #[test]
    fn special_command_rendering_bang_height_uses_command_geometry_for_every_gutter() {
        let chrome = Chrome {
            headline: "",
            modal: None,
            model: "",
            endpoint: "",
            gauge: None,
            session: "",
            background_jobs: &[],
            tabs: &[],
        };

        // 153 exposes a non-1 continuation indent; 159 exposes gutter=0.
        // Both expose the wide-gutter logical-line shortcut at 80 columns.
        for tail_len in [153, 159] {
            for gutter in [None, Some(0), Some(1), Some(7), Some(30)] {
                let body = format!("!{}", "x".repeat(tail_len));
                let mut mounted = MountedEditor::new(Edit::Emacs, gutter, Vec::new(), &body);
                let shown = bang_view(&mounted.textarea).expect("a real bang escape");
                let prompt = command_line(CommandKind::Bang, "");
                let drawn_rows = overhang_rows(
                    &prompt,
                    shown.textarea.lines(),
                    shown.textarea.cursor(),
                    1,
                    80,
                    None,
                )
                .0
                .len() as u16;
                // `+ 2`: the header above the input and the footer below it.
                let expected = drawn_rows.clamp(1, MAX_INPUT_ROWS) + 2;

                assert_eq!(
                    mounted.wanted_rows(80, 30, &chrome),
                    expected,
                    "tail={tail_len}, gutter={gutter:?}: allocation must match draw_overhang(g=1)"
                );
            }
        }
    }

    #[test]
    fn special_command_rendering_bang_cursor_uses_the_marker_for_hidden_prefix() {
        let editor = emacs_editor();
        let mut textarea = TextArea::new(vec!["  ! date".to_string()]);

        for hidden_col in 0..3 {
            textarea.move_cursor(CursorMove::Jump(0, hidden_col));
            assert_eq!(
                rendered_cursor_with(&textarea, &editor, 40, 3, Some(30)),
                (0, 1),
                "cursor on hidden prefix column {hidden_col} belongs on the visible ! marker"
            );
        }
        textarea.move_cursor(CursorMove::Jump(0, 3));
        assert_eq!(
            rendered_cursor_with(&textarea, &editor, 40, 3, Some(30)),
            (1, 1),
            "cursor immediately after the source ! belongs after the visible marker"
        );
    }

    #[test]
    fn special_command_rendering_recedes_bang_and_ex_behind_a_modal() {
        let bang_editor = emacs_editor();
        let bang_textarea = TextArea::new(vec!["! date".to_string()]);
        let (_, bang_styles) =
            rendered_row_with(&bang_textarea, &bang_editor, 40, 3, 1, Some(1), true);
        assert_eq!(bang_styles[0].fg, Some(Color::DarkGray));
        assert_eq!(bang_styles[0].bg, Some(INACTIVE_COMMAND_BG));

        let mut ex_editor = vi_editor();
        let mut ex_textarea = TextArea::default();
        ex_editor.input(special(KeyCode::Esc), &mut ex_textarea);
        ex_editor.input(key(':'), &mut ex_textarea);
        type_chars(&mut ex_editor, &mut ex_textarea, "help");
        let (_, ex_styles) = rendered_row_with(&ex_textarea, &ex_editor, 40, 3, 1, Some(1), true);
        assert_eq!(ex_styles[0].fg, Some(Color::DarkGray));
        assert_eq!(ex_styles[0].bg, Some(INACTIVE_COMMAND_BG));
    }

    #[test]
    fn special_command_rendering_committed_bang_uses_a_command_marker() {
        let mut sink = RecordingSink::default();
        echo_submitted(&mut sink, "! date", Some(1)).unwrap();

        let command = &sink.batches[0][1];
        let text = line_text(command);
        assert!(text.starts_with("! date"), "command echo: {text:?}");
        assert!(
            !text.contains(ECHO_CHEVRON),
            "a shell escape is not a chat turn: {text:?}"
        );
        assert!(
            command.spans.iter().all(|span| span.style.bg.is_some()),
            "the whole committed command row carries command chrome"
        );
    }

    #[test]
    fn special_command_rendering_wide_echoes_fit_cells_without_losing_text() {
        let width = 10;
        for (kind, command, tail) in [
            (CommandKind::Bang, "!日本語版確認用", "日本語版確認用"),
            (CommandKind::Ex, ":日本語版確認用", "日本語版確認用"),
        ] {
            let rows = command_body_rows(command, kind, width);
            for row in &rows {
                let prefix = if row.lead { 1 } else { 2 };
                assert!(
                    prefix + str_width(&row.text) <= width,
                    "{kind:?} row escaped {width} terminal cells: {row:?}"
                );
            }
            assert_eq!(
                rows.iter().map(|row| row.text.as_str()).collect::<String>(),
                tail,
                "{kind:?} wrapping must preserve every source character"
            );

            let lines = command_echo_lines(command, kind, width);
            for line in &lines[1..] {
                assert_eq!(
                    line.width(),
                    width,
                    "{kind:?} slab must be padded to exactly {width} cells: {line:?}"
                );
            }
        }
    }

    #[test]
    fn special_command_rendering_contextual_emoji_echoes_fit_cells() {
        for command in ["!❤️a", "!👩\u{200D}💻a"] {
            let rows = command_body_rows(command, CommandKind::Bang, 3);
            for row in &rows {
                let prefix = if row.lead { 1 } else { 2 };
                assert!(
                    prefix + str_width(&row.text) <= 3,
                    "emoji row escaped three terminal cells: {row:?}"
                );
            }
            assert_eq!(
                rows.iter().map(|row| row.text.as_str()).collect::<String>(),
                command.trim_start_matches('!'),
                "emoji wrapping must preserve every source scalar"
            );
        }
    }

    #[test]
    fn special_command_rendering_cancels_an_invisible_bang_selection_before_editing() {
        let mut mounted = MountedEditor::new(Edit::Emacs, Some(1), Vec::new(), "! date");
        let mut sink = RecordingSink::default();
        mounted.textarea.move_cursor(CursorMove::Jump(0, 2));
        mounted.textarea.start_selection();
        mounted.textarea.move_cursor(CursorMove::End);
        assert!(mounted.textarea.is_selecting());
        assert!(
            bang_view(&mounted.textarea).is_none(),
            "the renderer must not fabricate an unselected bang projection"
        );

        mounted.on_event(Event::Key(key('X')), &mut sink).unwrap();

        assert_eq!(mounted.textarea.lines(), ["! dateX"]);
        assert!(
            !mounted.textarea.is_selecting(),
            "display and editing state both remain unselected"
        );
        assert!(
            bang_view(&mounted.textarea).is_some(),
            "normal bang chrome resumes after selection normalization"
        );
    }

    #[test]
    fn special_command_rendering_vi_ex_echoes_before_its_output() {
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
        let mut sink = RecordingSink::default();
        for code in [
            KeyCode::Esc,
            KeyCode::Char(':'),
            KeyCode::Char('h'),
            KeyCode::Char('e'),
            KeyCode::Char('l'),
            KeyCode::Char('p'),
            KeyCode::Enter,
        ] {
            mounted
                .on_event(Event::Key(special(code)), &mut sink)
                .unwrap();
        }

        assert_eq!(sink.batches.len(), 2, "command, then its output");
        let command = line_text(&sink.batches[0][1]);
        let output: Vec<String> = sink.batches[1].iter().map(line_text).collect();
        assert!(
            command.trim_end().starts_with(":help"),
            "the executed ex command is committed first: {command:?}"
        );
        assert!(
            output.iter().any(|line| line.contains("vi  Esc=NORMAL")),
            "the command output follows: {output:?}"
        );
        assert!(
            !command.contains(ECHO_CHEVRON),
            "true ex command has no chat chevron: {command:?}"
        );
    }

    #[test]
    fn special_command_rendering_shift_enter_does_not_echo_an_open_ex_line() {
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
        let mut sink = RecordingSink::default();
        for code in [
            KeyCode::Esc,
            KeyCode::Char(':'),
            KeyCode::Char('h'),
            KeyCode::Char('e'),
            KeyCode::Char('l'),
            KeyCode::Char('p'),
        ] {
            mounted
                .on_event(Event::Key(special(code)), &mut sink)
                .unwrap();
        }

        mounted
            .on_event(
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
                &mut sink,
            )
            .unwrap();

        assert!(sink.batches.is_empty(), "Shift-Enter executes nothing");
        assert_eq!(mounted.editor.ex(), Some("help"));
        assert_eq!(mounted.textarea.lines().len(), 2, "it inserts a newline");
    }

    #[test]
    fn special_command_rendering_does_not_commit_an_unconfirmed_ex_command() {
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "draft");
        let mut sink = RecordingSink::default();
        for code in [
            KeyCode::Esc,
            KeyCode::Char(':'),
            KeyCode::Char('w'),
            KeyCode::Char('q'),
            KeyCode::Enter,
        ] {
            mounted
                .on_event(Event::Key(special(code)), &mut sink)
                .unwrap();
        }

        assert!(mounted.editor.confirm_prompt().is_some());
        assert!(
            sink.batches.is_empty(),
            "requesting confirmation is not executing :wq"
        );
        mounted
            .on_event(Event::Key(special(KeyCode::Char('n'))), &mut sink)
            .unwrap();
        assert!(sink.batches.is_empty(), "a cancelled :wq stays ephemeral");
    }

    #[test]
    fn special_command_rendering_confirmed_ex_preserves_and_echoes_its_spelling() {
        for command in ["wq", "x"] {
            let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "draft");
            let mut sink = RecordingSink::default();
            mounted
                .on_event(Event::Key(special(KeyCode::Esc)), &mut sink)
                .unwrap();
            mounted
                .on_event(Event::Key(special(KeyCode::Char(':'))), &mut sink)
                .unwrap();
            for c in command.chars() {
                mounted
                    .on_event(Event::Key(special(KeyCode::Char(c))), &mut sink)
                    .unwrap();
            }
            mounted
                .on_event(Event::Key(special(KeyCode::Enter)), &mut sink)
                .unwrap();

            assert!(mounted.editor.confirm_prompt().is_some());
            assert!(sink.batches.is_empty(), "confirmation is still ephemeral");
            assert_eq!(
                mounted
                    .on_event(Event::Key(special(KeyCode::Char('y'))), &mut sink)
                    .unwrap(),
                Some(EditorOutcome::LineThenQuit("draft".to_string()))
            );

            assert_eq!(
                sink.batches.len(),
                2,
                "confirmed command, then submitted draft"
            );
            let command_echo = line_text(&sink.batches[0][1]);
            assert!(
                command_echo.trim_end().starts_with(&format!(":{command}")),
                "confirmed spelling survives until execution: {command_echo:?}"
            );
            assert!(
                line_text(&sink.batches[1][1]).starts_with(ECHO_CHEVRON),
                "the submitted draft remains an ordinary model turn"
            );
        }
    }

    #[test]
    fn special_command_rendering_insert_colon_remains_chat() {
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
        let mut sink = RecordingSink::default();
        for code in [
            KeyCode::Char(':'),
            KeyCode::Char('h'),
            KeyCode::Char('e'),
            KeyCode::Char('l'),
            KeyCode::Char('p'),
        ] {
            assert_eq!(
                mounted
                    .on_event(Event::Key(special(code)), &mut sink)
                    .unwrap(),
                None
            );
        }
        let outcome = mounted
            .on_event(Event::Key(special(KeyCode::Enter)), &mut sink)
            .unwrap();

        assert_eq!(outcome, Some(EditorOutcome::Line(":help".to_string())));
        let text = line_text(&sink.batches[0][1]);
        assert!(
            text.starts_with(ECHO_CHEVRON),
            "INSERT-mode colon text stays a model turn: {text:?}"
        );
    }

    #[test]
    fn special_command_rendering_single_line_ex_hides_the_draft() {
        let mut editor = vi_editor();
        let mut textarea = TextArea::new(vec!["draft".to_string()]);
        editor.input(special(KeyCode::Esc), &mut textarea);
        editor.input(key(':'), &mut textarea);
        type_chars(&mut editor, &mut textarea, "help");

        let (row, styles) = rendered_row(&textarea, &editor, 40, 3, 1);
        assert!(row.starts_with(":help"), "ex command owns the row: {row:?}");
        assert!(
            !row.contains("draft"),
            "the hidden draft is not concatenated to the command: {row:?}"
        );
        assert!(
            styles[0].bg.is_some(),
            "the live ex row is visually distinct from chat"
        );
    }

    #[test]
    fn special_command_rendering_vi_ex_cursor_uses_terminal_cells() {
        for (lines, height, expected_y) in [
            // height, expected cursor row. Both grow by one for the footer,
            // and the row itself is offset by the header above the input.
            (vec!["draft".to_string()], 3, 1),
            (vec!["draft".to_string(), "second".to_string()], 4, 2),
        ] {
            let mut editor = vi_editor();
            let mut textarea = TextArea::new(lines);
            editor.input(special(KeyCode::Esc), &mut textarea);
            editor.input(key(':'), &mut textarea);
            type_chars(&mut editor, &mut textarea, "日本");

            assert_eq!(
                rendered_cursor_with(&textarea, &editor, 40, height, Some(1)),
                (5, expected_y),
                "the ':' marker plus two double-cell characters place the cursor at column five"
            );
        }
    }

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
    fn wrap_segments_uses_contextual_emoji_widths() {
        for text in ["❤️a", "👩\u{200D}💻a"] {
            assert_eq!(str_width(text), 3, "fixture must occupy three cells");
            let segs = wrap_segments(text, 2, 2);
            assert_eq!(segs.len(), 2, "{text:?} must wrap before the trailing a");
            assert_eq!(segs[1].1, "a");
            assert!(
                segs.iter().all(|(_, segment)| str_width(segment) <= 2),
                "every contextual-width segment must fit its two-cell budget: {segs:?}"
            );
            assert_eq!(
                segs.iter()
                    .map(|(_, segment)| segment.as_str())
                    .collect::<String>(),
                text,
                "wrapping must preserve the presentation and joiner scalars"
            );
        }
    }

    #[test]
    fn overhang_rows_cursor_uses_contextual_emoji_widths() {
        let prompt = Line::from("!");
        for (text, cursor_col) in [("❤️a", 2), ("👩\u{200D}💻a", 3)] {
            let (_, cx, cy) =
                overhang_rows(&prompt, &[text.to_string()], (0, cursor_col), 1, 4, None);
            assert_eq!((cx, cy), (3, 0), "cursor follows the two-cell emoji");
        }
    }

    #[test]
    fn committed_prompt_echo_wraps_a_long_line_without_clipping() {
        // The reported bug: a committed `› <prompt>` line wider than the
        // terminal was CLIPPED at the right edge (its tail lost). The echo must
        // instead carry the overflow onto continuation rows, exactly as the
        // interactive input surface already does via `wrap_segments`.
        let width = 24;
        let hang = 1;
        let body = "the quick brown fox jumps over the lazy dog and keeps on running east";
        let rows = echo_body_rows(body, hang, width);
        // Every row fits within the terminal once its leading marker is added,
        // so ratatui's fixed-height paint never truncates it.
        for r in &rows {
            let marker = if r.lead {
                ECHO_CHEVRON.chars().count()
            } else {
                hang
            };
            assert!(
                marker + r.text.chars().count() <= width,
                "row overruns width {width} (lead={}): {:?}",
                r.lead,
                r.text
            );
        }
        // Nothing is dropped: the wrapped segments reassemble the whole prompt.
        let joined: String = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(joined, body, "the full prompt survives the wrap");
        assert!(
            rows.len() > 1,
            "a line wider than the terminal actually wraps"
        );
        assert!(rows[0].lead, "the first row carries the chevron");
        assert!(
            rows[1..].iter().all(|r| !r.lead),
            "continuations hang under it — no second chevron"
        );
    }

    #[test]
    fn committed_prompt_echo_is_unchanged_when_the_line_fits() {
        // A prompt that fits the width is a single lead row carrying the whole
        // text — byte-identical to the pre-fix single-row form (0.7.x preserved).
        let rows = echo_body_rows("ship it", 1, 40);
        assert_eq!(
            rows,
            vec![EchoRow {
                lead: true,
                text: "ship it".to_string(),
            }]
        );
    }

    #[test]
    fn committed_prompt_echo_preserves_multiline_input() {
        // Multi-line input keeps one lead row (chevron) then hang rows —
        // unchanged from the historical per-line layout when nothing overflows.
        let rows = echo_body_rows("alpha\nbeta", 1, 40);
        assert_eq!(
            rows,
            vec![
                EchoRow {
                    lead: true,
                    text: "alpha".to_string(),
                },
                EchoRow {
                    lead: false,
                    text: "beta".to_string(),
                },
            ]
        );
    }

    #[test]
    fn committed_note_wraps_long_lines_without_clipping() {
        // The sibling emitter: a `:command` note (e.g. a capability-denied
        // diagnostic) wider than the terminal must wrap, not clip.
        let width = 16;
        let note = "capability denied: fs_write does not permit '/etc/hosts'";
        let rows = note_rows(note, width);
        for r in &rows {
            assert!(
                r.chars().count() <= width,
                "note row overruns width {width}: {r:?}"
            );
        }
        assert_eq!(rows.join(""), note, "the full note survives the wrap");
        assert!(rows.len() > 1, "a long note line actually wraps");
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

    /// G3(a), the registration conformance test (#2005): every claimant the
    /// shipped table names must be REACHABLE from a real key sequence through
    /// the editor's own accessors.
    ///
    /// Reachability, not spelling, is the point. A test that asserted the five
    /// names appear somewhere in the source would pass on a `claims` that can
    /// never fire; this one types the keys an operator types and reads the
    /// claim set back, so a rung with no accessor — or an accessor guarded on
    /// a condition that is never true — fails the PR that adds it.
    #[cfg(unix)]
    #[test]
    fn every_ladder_claimant_is_reachable_from_the_editors_own_state() {
        // The key sequence that puts a fresh vi mount into each claiming
        // state, straight out of `esc_and_vi_contract.md` §4.
        let reach: &[(&str, &[KeyEvent])] = &[
            // A fresh vi mount IS in INSERT, so no keys at all.
            ("vi-insert", &[]),
            ("palette", &[key('/')]),
            ("vi-pending", &[special(KeyCode::Esc), key('d')]),
            ("vi-ex", &[special(KeyCode::Esc), key(':')]),
            (
                "vi-confirm",
                &[
                    special(KeyCode::Esc),
                    key(':'),
                    key('w'),
                    key('q'),
                    special(KeyCode::Enter),
                ],
            ),
        ];
        for (claimant, keys) in reach {
            let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
            let mut sink = RecordingSink::default();
            for k in *keys {
                mounted.on_event(Event::Key(*k), &mut sink).unwrap();
            }
            assert!(
                mounted.claim_set().is_live(claimant),
                "`{claimant}` is a rung in assets/esc_ladder.toml but nothing \
                 in Vi::claims / Editor::claims / MountedEditor::claim_set \
                 reports it — the rung can never fire"
            );
        }

        // Both directions. The loop above proves every listed name is
        // reachable; this proves the LIST is the table, so adding a rung
        // without an accessor (or an accessor without a rung) is a red PR
        // rather than a dead row nobody notices.
        let mut reachable: Vec<&str> = reach.iter().map(|(name, _)| *name).collect();
        reachable.sort_unstable();
        let mut table: Vec<&str> = crate::esc_ladder::ESC_LADDER.claimants().collect();
        table.sort_unstable();
        assert_eq!(
            reachable, table,
            "the ladder's claimants and the states this test can reach have \
             drifted apart"
        );

        // ANTI-VACUOUS TWIN: an idle vi mount in NORMAL claims NOTHING, so the
        // assertions above cannot be passing on a `claim_set` that names
        // everything unconditionally — which would swallow the interrupt at
        // every rung and reproduce exactly the defect #2005 fixes.
        let mut normal = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
        let mut sink = RecordingSink::default();
        normal
            .on_event(Event::Key(special(KeyCode::Esc)), &mut sink)
            .unwrap();
        assert_eq!(
            normal.claim_set().names().collect::<Vec<_>>(),
            Vec::<&str>::new(),
            "vi NORMAL with nothing pending must decline Esc — that decline IS \
             rung 7"
        );

        // ANTI-VACUOUS TWIN, second half: the `edit == Edit::Vi` gate in
        // `Editor::claims`. `Editor` carries a `Vi` in every mode and it
        // starts in INSERT, so without the gate an emacs mount would claim
        // `vi-insert` forever and Esc would never reach the hatch there.
        for (name, edit) in [("emacs", Edit::Emacs), ("nano", Edit::Nano)] {
            let mounted = MountedEditor::new(edit, Some(1), Vec::new(), "");
            assert_eq!(
                mounted.claim_set().names().collect::<Vec<_>>(),
                Vec::<&str>::new(),
                "{name} carries a Vi in INSERT; it must not claim Esc"
            );
        }
    }

    /// `vi-pending` is an OR of three separate fields, and the conformance
    /// test above only reaches one of them — so dropping either of the other
    /// two would go unnoticed there. Each is a live operator sequence: type a
    /// count and press Esc, or use i_CTRL-O and press Esc, and rung 6 must
    /// still outrank the interrupt. Without this, a mutation removing
    /// `count > 0` means typing `2` mid-turn and pressing Esc kills the turn
    /// instead of cancelling the count.
    #[cfg(unix)]
    #[test]
    fn every_vi_pending_contributor_claims_esc() {
        for (what, keys) in [
            ("a pending operator", vec![special(KeyCode::Esc), key('d')]),
            ("a building count", vec![special(KeyCode::Esc), key('2')]),
            // i_CTRL-O leaves mode == Normal with the one-shot armed, so it
            // must land on `vi-pending` and NOT on `vi-insert`.
            ("i_CTRL-O", vec![ctrl('o')]),
        ] {
            let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
            let mut sink = RecordingSink::default();
            for k in keys {
                mounted.on_event(Event::Key(k), &mut sink).unwrap();
            }
            assert_eq!(
                mounted.claim_set().names().collect::<Vec<_>>(),
                vec!["vi-pending"],
                "{what} must claim rung 6 alone — not nothing (the turn dies \
                 mid-sequence) and not vi-insert as well"
            );
        }
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

    /// #1669 PR-B, the load-bearing invariant: with fewer than two tabs the
    /// frame is **byte-identical** to the pre-bar surface.
    ///
    /// Not "an empty row" and not "a row of spaces" — no row at all. Almost
    /// every session is single-conversation, and the bar is not worth a
    /// permanent row of their terminal. Comparing the whole rendered buffer
    /// rather than eyeballing one row is what makes that a guarantee.
    #[test]
    fn a_single_tab_frame_is_byte_identical_to_no_bar() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let ed = emacs_editor();
        let ta = TextArea::new(vec!["hello".to_string()]);

        let render = |tabs: &[crate::tab_bar::TabCell]| -> Vec<String> {
            let mut term = Terminal::new(TestBackend::new(40, 5)).unwrap();
            term.draw(|f| {
                draw(
                    f,
                    &ta,
                    &ed,
                    Some(1),
                    RichStatus {
                        tabs,
                        ..RichStatus::default()
                    },
                );
            })
            .unwrap();
            let buf = term.backend().buffer();
            (0..5)
                .map(|y| {
                    (0..40)
                        .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                        .collect::<String>()
                })
                .collect()
        };

        let none = render(&[]);
        let one = render(&[crate::tab_bar::TabCell {
            number: 1,
            label: "solo".into(),
            active: true,
            degraded: false,
            pending: false,
        }]);
        assert_eq!(none, one, "one tab must render exactly like no tabs");
        assert!(
            !one.iter().any(|r| r.contains("solo")),
            "the single tab's label appears nowhere: {one:?}"
        );
    }

    /// Two tabs claim exactly one row, at the BOTTOM of the region, and the
    /// rows above are untouched.
    #[test]
    fn two_tabs_add_one_row_below_the_clock() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let ed = emacs_editor();
        let ta = TextArea::new(vec!["hello".to_string()]);
        let cell = |n: usize, l: &str, a: bool| crate::tab_bar::TabCell {
            number: n,
            label: l.into(),
            active: a,
            degraded: false,
            pending: false,
        };
        let render = |tabs: &[crate::tab_bar::TabCell]| -> Vec<String> {
            let mut term = Terminal::new(TestBackend::new(40, 5)).unwrap();
            term.draw(|f| {
                draw(
                    f,
                    &ta,
                    &ed,
                    Some(1),
                    RichStatus {
                        tabs,
                        ..RichStatus::default()
                    },
                );
            })
            .unwrap();
            let buf = term.backend().buffer();
            (0..5)
                .map(|y| {
                    (0..40)
                        .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                        .collect::<String>()
                })
                .collect()
        };

        let none = render(&[]);
        let two = render(&[cell(1, "build", true), cell(2, "deploy", false)]);
        // The bar is the OUTERMOST row: everything above it belongs to the
        // selected tab, so the container sits outside its contents. The clock
        // stays directly above it — the last row of the tab's own frame.
        assert!(
            two[4].contains("1:build") && two[4].contains("2:deploy"),
            "the bar is the bottom-most row: {:?}",
            two[4]
        );
        assert!(
            two[3].contains("emacs"),
            "the clock sits directly above the bar: {:?}",
            two[3]
        );
        assert!(
            none[4].contains("emacs"),
            "with one tab the clock is the last row: {:?}",
            none[4]
        );
        assert!(
            !none.iter().any(|row| row.contains("1:build")),
            "sanity: the no-tab frame has no bar"
        );
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

        // Two-line layout (#527): row 0 is the status header; the message renders
        // below it, and the `:`-command on the last (bottom) row.
        let mut term = Terminal::new(TestBackend::new(40, 4)).unwrap();
        term.draw(|f| draw(f, &ta, &ed, Some(1), RichStatus::default()))
            .unwrap();
        let buf = term.backend().buffer();
        let row = |y: u16| -> String {
            (0..40)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
        };
        // Bottom of the INPUT region — which is one row above the footer.
        assert!(
            row(2).starts_with(":wq"),
            "command on the input region's bottom row: {:?}",
            row(2)
        );
        assert!(
            row(1).contains("hello"),
            "message renders below the header: {:?}",
            row(1)
        );
        assert!(
            !row(0).contains(":wq") && !row(1).contains(":wq"),
            "command must NOT be glued to the input rows"
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
        let mut term = Terminal::new(TestBackend::new(80, 4)).unwrap();
        term.draw(|f| draw(f, &ta, &ed, Some(25), RichStatus::default()))
            .unwrap(); // 25 >= GUTTER_W (19)
        let buf = term.backend().buffer();
        // Bottom of the INPUT region — one row above the footer.
        let last: String = (0..80)
            .map(|x| buf.cell((x, 2)).unwrap().symbol().to_string())
            .collect();
        assert!(
            last.starts_with(":wq"),
            "command on the input region's bottom row (wide gutter): {last:?}"
        );
    }

    #[test]
    fn slash_palette_renders_above_the_input_row() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        // #1674: the palette renders INSIDE the existing inline draw path —
        // between the status header and the input row — through the same
        // frame as the editor. No second surface, no second event loop.
        let mut palette = PaletteState::from_corpus();
        palette.on_buffer_change("", "/");
        palette.on_buffer_change("/", "/model");
        let rows = palette.viewport_rows(8);
        assert!(rows >= 2, "the /model filter keeps several corpus entries");
        palette.set_viewport(rows);
        let editor = nano_editor();
        let ta = TextArea::new(vec!["/model".to_string()]);
        let h = 1 + rows as u16 + 1; // header + palette + input
        let mut term = Terminal::new(TestBackend::new(100, h)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &ta,
                &editor,
                Some(1),
                RichStatus {
                    palette: Some(&palette),
                    ..RichStatus::default()
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row = |y: u16| -> String {
            (0..100)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect()
        };
        // Directly under the header: the highlighted first prefix match, with
        // its corpus description beside it.
        assert!(
            row(1).starts_with("❯ /models"),
            "highlight on the first match: {:?}",
            row(1)
        );
        assert!(
            row(1).contains("list models"),
            "description rides beside the command: {:?}",
            row(1)
        );
        // The input still shows the typed line — one row above the footer,
        // which is now what occupies the bottom.
        assert!(
            row(h - 2).contains("❯ /model"),
            "input row intact below the palette: {:?}",
            row(h - 2)
        );
        assert!(
            row(h - 1).contains("nano"),
            "and the footer is the last row: {:?}",
            row(h - 1)
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
        // Header, two input rows, footer.
        let mut term = Terminal::new(TestBackend::new(80, 4)).unwrap();
        // gutter = 1 → the overhang layout (the default).
        term.draw(|f| {
            draw(
                f,
                &ta,
                &editor,
                Some(1),
                RichStatus {
                    model: "m",
                    endpoint: "http://e:1",
                    ..RichStatus::default()
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row = |y: u16| -> String {
            (0..80)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
        };
        // Machine state is the FOOTER's, on the last row — it used to lead.
        assert!(
            row(3).contains("vi --INSERT--") && row(3).contains("m @ http://e:1"),
            "footer row carries mode + model @ endpoint: {:?}",
            row(3)
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

    #[test]
    fn a_running_job_leads_the_layout_rather_than_shifting_the_input() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let editor = vi_editor();
        let textarea = TextArea::default();
        let job = BackgroundJob::start("indexing repository");
        // Four regions when a job runs: activity, header, input, footer.
        let mut term = Terminal::new(TestBackend::new(80, 4)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &textarea,
                &editor,
                Some(1),
                RichStatus {
                    model: "m",
                    endpoint: "http://e:1",
                    background_jobs: std::slice::from_ref(&job),
                    ..RichStatus::default()
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row = |y: u16| -> String {
            (0..80)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
        };

        // The activity row LEADS the layout now. It was bottom-anchored, which
        // put a row that appears and disappears directly under the input —
        // shifting the input and its footer under the operator's hands every
        // time a job started or finished. At the top it displaces nothing they
        // are looking at.
        assert!(
            row(0).contains("background") && row(0).contains("indexing repository"),
            "the live job leads: {:?}",
            row(0)
        );
        assert!(
            row(2).contains('\u{276f}'),
            "the prompt sits below the activity row and the header: {:?}",
            row(2)
        );
    }

    #[test]
    fn completed_background_job_has_no_indicator_row() {
        let first = BackgroundJob::start("indexing repository");
        let second = BackgroundJob::start("warming symbols");
        let text = |jobs: &[BackgroundJob]| {
            background_line(jobs, 0).map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
        };
        let both = text(&[first.clone(), second.clone()]).unwrap();
        assert!(both.contains("background (2)"), "{both}");
        assert!(both.contains(first.label()) && both.contains(second.label()));

        first.finish();
        let one = text(&[first, second.clone()]).unwrap();
        assert!(!one.contains("background (2)"), "{one}");
        assert!(!one.contains("indexing repository"), "{one}");
        assert!(one.contains(second.label()), "{one}");

        second.finish();
        assert!(text(&[second]).is_none());
    }

    fn row_text(editor: &Editor) -> String {
        prompt_line(editor, true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn footer_text(editor: &Editor, model: &str, endpoint: &str) -> String {
        footer_line(editor, model, endpoint, None, true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn header_text(session: &str, headline: &str) -> String {
        header_line(session, headline)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    /// **The header answers "which conversation is this", and nothing else.**
    ///
    /// #1671's session name lives here. What used to sit beside it — the
    /// timestamp, the mode word, the model, the gauge — moved to the footer,
    /// because a row carrying both identity and machine state had no single
    /// owner, and grew an echo of the draft that the line below it was already
    /// showing.
    #[test]
    fn the_header_carries_identity_and_nothing_else() {
        let named = header_text("mesh docking", "");
        assert!(named.contains("[mesh docking]"), "{named}");

        // The untitled form (#shortid) and the ephemeral marker render too.
        assert!(header_text("#a1b2c3d4", "").contains("[#a1b2c3d4]"));
        assert!(header_text("ephemeral", "").contains("[ephemeral]"));

        // Prose sits beside the name, separated by a space — no bracket, no rule.
        let with_prose = header_text("mesh docking", "wiring the dock");
        assert_eq!(with_prose, "[mesh docking] wiring the dock");

        // Neither half is mandatory, and an absent half renders NOTHING rather
        // than an empty bracket or a stray separator.
        assert_eq!(header_text("", "just prose"), "just prose");
        assert_eq!(header_text("solo", ""), "[solo]");
        assert_eq!(header_text("", ""), "");

        // Machine state is the footer's, and must not have followed the name.
        for absent in ["vi", "@", "k/"] {
            assert!(
                !with_prose.contains(absent),
                "`{absent}` belongs to the footer: {with_prose}"
            );
        }
    }

    /// **No region is separated by a horizontal rule.**
    ///
    /// A full-width `─────` run is a word-wrap hazard at every terminal width,
    /// and adjacency already says these rows are regions. Asserted because a
    /// rule is the obvious thing to reach for when someone later wants the
    /// regions to "read as separate".
    #[test]
    fn no_region_draws_a_horizontal_rule() {
        let ed = vi_editor();
        let rows = [
            header_text("session", "headline"),
            footer_text(&ed, "model", "http://endpoint"),
        ];
        for row in rows {
            for rule in ['\u{2500}', '\u{2501}', '\u{2550}', '_'] {
                assert!(
                    !row.contains(&rule.to_string().repeat(4)),
                    "a run of `{rule}` is a rule: {row}"
                );
            }
        }
    }

    #[test]
    fn header_shows_context_budget_gauge_when_known() {
        let ed = vi_editor();
        let text = |g| -> String {
            footer_line(&ed, "m", "e", g, true)
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert!(
            text(Some((972_000, 1_024_000))).contains("972k/1024k"),
            "the gauge shows used/budget once the budget is known"
        );
        assert!(
            !text(None).contains("k/"),
            "no gauge until a budget is known"
        );
        assert!(
            !text(Some((100, 0))).contains("k/"),
            "a zero budget shows no gauge (no divide-by-zero, no noise)"
        );
    }

    #[test]
    fn native_status_row_shows_the_insert_indicator() {
        // The input row carries the `❯` indicator; the header carries the clock,
        // mode word, and model @ endpoint (two-line layout, #527).
        let editor = vi_editor();
        assert!(row_text(&editor).contains('❯'), "insert indicator");
    }

    #[test]
    fn blocking_modal_recedes_the_chat_chevron_without_changing_its_shape() {
        let editor = vi_editor();
        let active = prompt_line_with_focus(&editor, true, true);
        let inactive = prompt_line_with_focus(&editor, true, false);

        assert_eq!(active.spans[0].content, inactive.spans[0].content);
        assert_eq!(
            active.spans[0].style.fg,
            Some(Color::from(newt_core::tty::ACTIVE_INPUT_CT))
        );
        assert_eq!(inactive.spans[0].style.fg, Some(Color::DarkGray));
    }

    /// **Exactly one prompt on screen is live.** The receding chevron is
    /// asserted above at line level; this asserts it at FRAME level, which is
    /// the form the operator actually reported — two chevrons both painted in
    /// the live accent, with nothing saying which one owned the keyboard.
    /// Scanning every cell means a future row that reintroduces the accent
    /// while a modal is up fails here, not in a screenshot.
    #[test]
    fn no_cell_carries_the_live_accent_while_a_modal_owns_the_keyboard() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let editor = vi_editor();
        let textarea = TextArea::new(vec!["a draft that survives".to_string()]);
        let accent = Color::from(newt_core::tty::ACTIVE_INPUT_CT);

        let render = |chat_inactive: bool| {
            let (width, height) = (60_u16, 4_u16);
            let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
            term.draw(|f| {
                draw(
                    f,
                    &textarea,
                    &editor,
                    Some(1),
                    RichStatus {
                        chat_inactive,
                        ..RichStatus::default()
                    },
                );
            })
            .unwrap();
            let buf = term.backend().buffer();
            (0..height)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .filter(|&(x, y)| buf.cell((x, y)).unwrap().style().fg == Some(accent))
                .count()
        };

        assert!(
            render(false) > 0,
            "the mounted chat chevron is accented while it owns the keyboard"
        );
        assert_eq!(
            render(true),
            0,
            "a modal owns the keyboard: nothing beneath it may still read as live"
        );
    }

    #[test]
    fn the_footer_shows_datetime_mode_and_model_endpoint() {
        let insert = vi_editor(); // starts in INSERT
        let h = footer_text(&insert, "nemotron-3-nano:30b", "http://REDACTED-HOST:11434");
        assert!(h.starts_with('['), "datetime stamp: {h:?}");
        assert!(h.contains("vi --INSERT--"), "{h:?}");
        assert!(
            h.contains("nemotron-3-nano:30b @ http://REDACTED-HOST:11434"),
            "{h:?}"
        );
        // NORMAL flips the mode word live.
        let mut normal = vi_editor();
        let mut ta = TextArea::default();
        normal.input(special(KeyCode::Esc), &mut ta);
        assert!(footer_text(&normal, "m", "e").contains("vi --NORMAL--"));
        // emacs/nano show the bare editor name; empty model omits the `@`.
        assert!(footer_text(&emacs_editor(), "m", "e").contains("emacs"));
        assert!(!footer_text(&insert, "", "").contains('@'));
    }

    #[test]
    fn mode_hint_advertises_the_other_editor_modes() {
        assert!(vi_editor().mode_hint(false).contains("INSERT"));
        assert!(vi_editor().mode_hint(false).contains("/nano"));
        assert!(vi_editor().mode_hint(false).contains("/emacs"));
    }

    /// **The `:` belongs to an OPEN ex line, and to nothing else.**
    ///
    /// vi's own semantics, which this surface used to contradict: NORMAL mode
    /// showed a highlighted `:` as its prompt indicator, so an operator who
    /// had pressed Esc was looking at what vi only ever shows for a command
    /// line they had not opened. Worse, the way OUT of it read as backing out
    /// of a command rather than as pressing `i`.
    ///
    /// In vi the buffer looks the same in NORMAL as in INSERT; the mode lives
    /// in the status line (`-- INSERT --`) and in the cursor. So the chevron
    /// stays in both modes, and the `:` appears exactly when `:` is pressed.
    #[test]
    fn only_an_open_ex_line_shows_the_colon() {
        let mut ta = TextArea::default();

        // INSERT: the chevron.
        assert!(row_text(&vi_editor()).starts_with('❯'));

        // NORMAL: still the chevron, and NOT a command line.
        let mut normal = vi_editor();
        normal.input(special(KeyCode::Esc), &mut ta); // INSERT → NORMAL
        let row = row_text(&normal);
        assert!(
            row.starts_with('❯'),
            "NORMAL keeps the input chevron: {row:?}"
        );
        assert!(
            !row.trim_start().starts_with(':'),
            "NORMAL is not command-line mode: {row:?}"
        );

        // The mode is still discoverable — where vi keeps it.
        assert!(
            footer_text(&normal, "m", "e").contains("vi --NORMAL--"),
            "the header carries the mode"
        );
        assert!(
            normal.mode_hint(false).contains("i: insert"),
            "the hint says how to get back to typing"
        );

        // `:` opens the ex line, and THEN the colon shows with the command.
        let mut ex = vi_editor();
        ex.input(special(KeyCode::Esc), &mut ta);
        ex.input(key(':'), &mut ta);
        ex.input(key('w'), &mut ta);
        let exrow = row_text(&ex);
        assert!(exrow.contains(":w"), "ex line shows the command: {exrow:?}");
        assert!(!exrow.contains('❯'), "the ex line owns the row: {exrow:?}");

        // Esc closes the ex line and lands back in NORMAL — chevron, no colon.
        ex.input(special(KeyCode::Esc), &mut ta);
        let back = row_text(&ex);
        assert!(
            back.starts_with('❯') && !back.contains(":w"),
            "Esc closes the command line, back to the input row: {back:?}"
        );
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

    #[serial_test::serial(real_fs)]
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

    // ── #1669 16.3: vim tab motions ────────────────────────────────────────

    /// Drive chars from NORMAL and return the last `Step`.
    ///
    /// The leading Esc is not decoration: `Editor::new(Edit::Vi)` starts in
    /// INSERT, so a helper that skips it tests nothing but typing.
    fn normal_keys(ed: &mut Editor, ta: &mut TextArea, keys: &str) -> Step {
        ed.input(special(KeyCode::Esc), ta); // INSERT → NORMAL
        let mut last = Step::Continue;
        for c in keys.chars() {
            last = ed.input(key(c), ta);
        }
        last
    }

    /// `gt` with no count is "next tab", not "go to tab 1".
    ///
    /// The distinction is the whole reason the count is read as `0 == absent`
    /// rather than through `take_count()`, which floors at 1.
    #[test]
    fn bare_gt_is_next_not_goto_one() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        assert_eq!(
            normal_keys(&mut ed, &mut ta, "gt"),
            Step::Tab(crate::tabs::TabAction::Next)
        );
    }

    /// **`{count}gt` is ABSOLUTE.** `2gt` is "go to tab 2", not "two tabs
    /// forward" — unusual for a vi count, correct for vim, and exactly the
    /// kind of thing a later refactor would "fix" into a relative motion.
    #[test]
    fn a_counted_gt_goes_to_that_tab_absolutely() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        assert_eq!(
            normal_keys(&mut ed, &mut ta, "2gt"),
            Step::Tab(crate::tabs::TabAction::Goto(2))
        );
        // Multi-digit counts accumulate the same way every other vi count does.
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        assert_eq!(
            normal_keys(&mut ed, &mut ta, "12gt"),
            Step::Tab(crate::tabs::TabAction::Goto(12))
        );
    }

    /// `gT` is relative in BOTH forms — bare is one back, counted is n back.
    /// Deliberately different from `gt`, matching vim.
    #[test]
    fn gt_capital_is_relative_in_both_forms() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        assert_eq!(
            normal_keys(&mut ed, &mut ta, "gT"),
            Step::Tab(crate::tabs::TabAction::Prev(1))
        );
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        assert_eq!(
            normal_keys(&mut ed, &mut ta, "3gT"),
            Step::Tab(crate::tabs::TabAction::Prev(3))
        );
    }

    /// The regression that matters most: `gg` still goes to the top.
    ///
    /// `g` is now a live prefix for three things, and `gg` is a hot key. If
    /// adding the tab motions had made `gg` return a `Step::Tab`, or consumed
    /// its count, the damage would be silent and constant.
    #[test]
    fn gg_still_jumps_to_the_top_and_is_not_a_tab_motion() {
        let mut ed = vi_editor();
        let mut ta = textarea_with(Edit::Vi, "one\ntwo\nthree");
        assert_eq!(
            normal_keys(&mut ed, &mut ta, "gg"),
            Step::Continue,
            "gg is a cursor jump, never a tab action"
        );
        assert_eq!(ta.cursor().0, 0, "cursor is on the first line");
    }

    /// An unknown `g`-suffix is swallowed, as before — it must not leak a tab
    /// action or leave the count armed for the next keystroke.
    #[test]
    fn an_unknown_g_suffix_stays_inert() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        assert_eq!(normal_keys(&mut ed, &mut ta, "gz"), Step::Continue);
        // The count from a previous attempt must not survive into the next.
        assert_eq!(
            normal_keys(&mut ed, &mut ta, "gt"),
            Step::Tab(crate::tabs::TabAction::Next),
            "a stale count would have made this a Goto"
        );
    }

    /// Tab motions are NORMAL-mode only: typing `gt` while inserting is text.
    ///
    /// A vi user types `g` and `t` constantly. If the tab motion fired from
    /// INSERT, every word containing "gt" would fling the operator into
    /// another agent's tab mid-sentence.
    #[test]
    fn gt_in_insert_mode_is_just_text() {
        let mut ed = vi_editor(); // starts in INSERT
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "gt");
        assert_eq!(
            ta.lines(),
            &["gt".to_string()],
            "insert mode types the characters"
        );
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

    #[serial_test::serial(real_fs)]
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

    // ── #2006: vi state is SESSION state, not per-line state ───────────────

    /// Drive one key into a mounted editor, discarding the scrollback it emits.
    fn mounted_key(mounted: &mut MountedEditor, sink: &mut RecordingSink, key: KeyEvent) {
        mounted.on_event(Event::Key(key), sink).unwrap();
    }

    /// Drive a run of plain chars into a mounted editor.
    fn mounted_chars(mounted: &mut MountedEditor, sink: &mut RecordingSink, s: &str) {
        for c in s.chars() {
            mounted_key(mounted, sink, key(c));
        }
    }

    fn sink_text(sink: &RecordingSink) -> String {
        sink.batches
            .iter()
            .flatten()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn vi_mode_survives_a_submit() {
        // #2006: Enter sends a line; it does not put the operator back in
        // INSERT behind their back.
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hi");
        let mut sink = RecordingSink::default();
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
        assert_eq!(mounted.editor.label(), "vi N");
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter)); // submit
        assert_eq!(
            mounted.editor.label(),
            "vi N",
            "the mode the operator chose outlives the line they sent"
        );
    }

    #[test]
    fn vi_jumplist_survives_a_submit() {
        // #2006: `Editor::new` threw the jumplist away with the mode.
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "a\nb\nc");
        let mut sink = RecordingSink::default();
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
        mounted_chars(&mut mounted, &mut sink, "gg"); // records a jump origin
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter)); // submit
        sink.batches.clear();
        mounted_chars(&mut mounted, &mut sink, ":jumps");
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter));
        let text = sink_text(&sink);
        assert!(text.contains("jumps  back:"), ":jumps reported: {text:?}");
        assert!(
            !text.contains("back: —"),
            "the jump recorded before the submit is still there: {text:?}"
        );
    }

    #[test]
    fn vi_last_find_survives_a_submit() {
        // #2006: `;` repeats the last `f`/`t` — across a submit too.
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hello world");
        let mut sink = RecordingSink::default();
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
        mounted_chars(&mut mounted, &mut sink, "0fw"); // find 'w'
        assert_eq!(mounted.textarea.cursor().1, 6, "`fw` landed on the 'w'");
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter)); // submit
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // NORMAL either way
        mounted_chars(&mut mounted, &mut sink, "i"); // → INSERT
        mounted_chars(&mut mounted, &mut sink, "hello world");
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
        mounted_chars(&mut mounted, &mut sink, "0;"); // repeat the find
        assert_eq!(
            mounted.textarea.cursor().1,
            6,
            "`;` still knows what `f` was looking for"
        );
    }

    #[test]
    fn vi_pending_sequence_does_not_survive_a_submit() {
        // The other half of #2006's decision: mode/jumplist/last_find are
        // session state, but a half-typed `f`/`d`/count belongs to the line
        // that was just sent. An `f` left armed would eat the next `i`.
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hi");
        let mut sink = RecordingSink::default();
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
        mounted_chars(&mut mounted, &mut sink, "f"); // awaiting a search target
        mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter)); // submit
        mounted_chars(&mut mounted, &mut sink, "iabc");
        assert_eq!(
            mounted.textarea.lines(),
            ["abc"],
            "`i` opened INSERT; it was not swallowed as a stale search target"
        );
    }

    #[test]
    fn vi_ctrl_c_at_an_idle_prompt_keeps_the_mode() {
        // #2006: `self.vi = Vi::new()` flipped a NORMAL operator into INSERT.
        // Real vim's `i_CTRL-C` is insert→normal; it is never normal→insert.
        let mut ed = vi_editor();
        let mut ta = new_textarea(Edit::Vi);
        type_chars(&mut ed, &mut ta, "hi");
        ed.input(special(KeyCode::Esc), &mut ta); // → NORMAL
        ed.input(ctrl('c'), &mut ta);
        assert_eq!(ta.lines(), [""], "Ctrl-C still clears the draft");
        assert_eq!(ed.label(), "vi N", "…and leaves the mode alone");
    }

    #[test]
    fn vi_ctrl_c_still_cancels_a_pending_sequence() {
        // Guards the replacement for the deleted `Vi::new()`: the draft is
        // gone, so an operator/search pending against it must go too.
        let mut ed = vi_editor();
        let mut ta = new_textarea(Edit::Vi);
        type_chars(&mut ed, &mut ta, "hi");
        ed.input(special(KeyCode::Esc), &mut ta); // → NORMAL
        ed.input(key('f'), &mut ta); // awaiting a search target
        ed.input(ctrl('c'), &mut ta);
        type_chars(&mut ed, &mut ta, "iabc");
        assert_eq!(
            ta.lines(),
            ["abc"],
            "`i` opened INSERT; the pending `f` did not eat it"
        );
    }

    #[test]
    fn vi_a_fresh_mount_still_starts_in_insert() {
        // The twin of the tests above: "persists" must not become "always
        // NORMAL". A session that has never pressed Esc opens in INSERT.
        let mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
        assert_eq!(mounted.editor.label(), "vi I");
    }

    #[test]
    fn vi_state_hands_off_across_a_remount() {
        // The seam the classic per-read driver and `SurfaceRequest::Reload`
        // use: a rebuilt mount adopts the outgoing one's vi state.
        let mut old = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hello world");
        let mut sink = RecordingSink::default();
        mounted_key(&mut old, &mut sink, special(KeyCode::Esc)); // → NORMAL
        mounted_chars(&mut old, &mut sink, "0fw"); // find 'w'

        let mut new = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hello world");
        new.adopt_vi(old.take_vi());
        assert_eq!(new.editor.label(), "vi N", "the mode came across");
        mounted_chars(&mut new, &mut sink, "0;");
        assert_eq!(
            new.textarea.cursor().1,
            6,
            "…and so did the `;` repeat target"
        );
        assert_eq!(old.editor.label(), "vi I", "the outgoing mount is spent");
    }

    #[test]
    fn mode_hint_promises_an_interrupt_only_while_a_turn_runs() {
        // Contract doc §3 item 4: at an idle prompt Ctrl-C clears the draft,
        // it does not interrupt anything. The affordance and the behavior
        // share one condition.
        for editor in [vi_editor(), emacs_editor(), nano_editor()] {
            let running = editor.mode_hint(true);
            let idle = editor.mode_hint(false);
            assert!(
                running.contains("^C interrupt"),
                "a running turn advertises the interrupt: {running:?}"
            );
            assert!(
                idle.contains("^C clear") && !idle.contains("interrupt"),
                "an idle prompt advertises what Ctrl-C actually does: {idle:?}"
            );
        }
        let mut normal = vi_editor();
        let mut ta = new_textarea(Edit::Vi);
        normal.input(special(KeyCode::Esc), &mut ta);
        assert!(normal.mode_hint(false).contains("^C clear"));
    }

    /// #2010: the `^D` half is idle-only, by the same rule as `^C`. During a
    /// turn the session is not reading, so Ctrl-D exits nothing — a hint
    /// that promised `^D exit` there was the invisible behaviour the
    /// operator reported.
    #[test]
    fn mode_hint_promises_an_exit_only_while_idle() {
        for editor in [vi_editor(), emacs_editor(), nano_editor()] {
            let idle = editor.mode_hint(false);
            let running = editor.mode_hint(true);
            assert!(
                idle.contains("^D exit"),
                "an idle prompt advertises the exit: {idle:?}"
            );
            assert!(
                !running.contains("^D"),
                "a running turn must not promise an exit it cannot take: {running:?}"
            );
        }
    }

    /// #2010: Ctrl-D while a turn runs is acknowledged AT PRESS TIME — a
    /// scrollback note saying where exit lives — and is NOT an `Eof` for the
    /// presenter to drop on the floor. Idle, the same key is the EOF it
    /// always was. (Whether a mid-turn Ctrl-D should escalate to an
    /// interrupt is the operator's call; this pins only that it is heard.)
    #[test]
    fn ctrl_d_during_a_turn_is_acknowledged_not_dropped() {
        let mut mounted = MountedEditor::new(Edit::Nano, Some(1), Vec::new(), "");
        let mut sink = RecordingSink::default();
        // The field, not `set_turn_running`: that setter is unix-only (its
        // one caller is the cockpit), and this rule holds on every platform.
        mounted.turn_running = true;
        let outcome = mounted.on_event(Event::Key(ctrl('d')), &mut sink).unwrap();
        assert_eq!(outcome, None, "mid-turn Ctrl-D is not an EOF");
        let notes: Vec<String> = sink.batches.iter().flatten().map(line_text).collect();
        assert!(
            notes.iter().any(|l| l.contains("Ctrl-C interrupts")),
            "the press is answered with where exit and interrupt live: {notes:?}"
        );

        mounted.turn_running = false;
        let outcome = mounted.on_event(Event::Key(ctrl('d')), &mut sink).unwrap();
        assert_eq!(
            outcome,
            Some(EditorOutcome::Eof),
            "idle Ctrl-D is still EOF"
        );
    }
}

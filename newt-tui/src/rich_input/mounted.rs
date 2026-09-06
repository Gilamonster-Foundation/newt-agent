//! Mounted editor state and event orchestration shared by the classic and cockpit drivers.

use std::io;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use tui_textarea::TextArea;

use crate::chat::BackgroundJob;
use crate::palette::{palette_step, PaletteState, PaletteStep};
use crate::rich_input::{
    bang_view, buffer_is_empty, cancel_hidden_bang_selection, command_line, draw, echo_command,
    echo_note, echo_submitted, ex_bottom_line, history_step, new_textarea, overhang_rows,
    prompt_line, resolve_gutter, textarea_with, Chrome, CommandKind, Edit, Editor, RichStatus,
    ScrollbackSink, Step, GUTTER_W, MAX_INPUT_ROWS,
};
use crate::vi::Vi;

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
    pub(super) textarea: TextArea<'static>,
    pub(super) editor: Editor,
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
    pub(super) turn_running: bool,
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

//! **The RichTUI renderer for one interaction** (C2 of epic #1803, #1876).
//!
//! The whole of this module is the terminal half. The pure view model it
//! draws — rows, selection, answer — lives in
//! [`newt_core::interaction_view`], one crate down, where `ratatui` is not a
//! dependency and so a widget type in the model is a compile error rather
//! than something a source scan has to catch.
//!
//! This file is `rich-tui`-gated at its declaration in `lib.rs`, following
//! `transcript_pager`'s split: a lean binary must not carry a widget surface
//! it may never draw (`plain_scroller_tui.md`).

// ---------------------------------------------------------------------------
// The terminal half — the ONLY part that touches a TTY or names a widget.
//
// COMPILE-GATED to `rich-tui`, following `transcript_pager`'s split: everything
// above this line is the pure view model and stays compiled and unit-tested in
// every configuration, including lean. `ratatui`/`crossterm` are non-optional
// deps of this crate, so without this gate a lean binary would carry a widget
// surface it must never draw (`plain_scroller_tui.md`).
//
// INLINE, NEVER THE ALTERNATE SCREEN. `plain_scroller_tui.md` permits an
// alt-screen modal on RichTUI, but the carve-out is CONDITIONAL: "Operator-
// invoked and modal. It opens on an explicit command (`/transcript`), not
// ambiently, and never during a turn." An interaction prompt is model-
// triggered and happens DURING a turn, so it satisfies neither condition. The
// permitted shape is a transient `Viewport::Inline` region — the
// `config_panel` / #416 precedent, "TTY-only, no alternate screen".
// ---------------------------------------------------------------------------
#[cfg(unix)]
pub(crate) use terminal::present_in;
pub(crate) use terminal::{
    present, reader_for_shared_decision, reader_for_shared_decision_on_terminal, requested_rows,
};

#[cfg(all(test, unix))]
pub(crate) use terminal::InlineGuard;

mod terminal {
    use newt_core::interaction_view::{InteractionView, RowKind, ViewRow};
    use newt_core::markup::spans::Emphasis;

    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use newt_core::interaction_surface::SurfaceInteraction;
    use newt_core::tty::raw_mode::RawModeGuard;
    use newt_core::tty::{ControlReader, Echo, PromptLine};
    use newt_interaction::{ControlKind, SemanticRole};
    use newt_policy::HumanQuestionOutcome;
    use ratatui::layout::{Constraint, Layout, Rect};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span as TuiSpan};
    use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
    use std::io;
    use std::time::Duration;

    /// Restore the terminal on EVERY exit path — return, error, panic.
    ///
    /// **RAII, not happy-path control flow**, and that distinction is the
    /// whole point. `config_panel::run` calls `enable_raw_mode()` and then
    /// `disable_raw_mode()` as a statement AFTER its loop closure: an error
    /// return is handled, but a panic unwinds straight past it and leaves the
    /// operator's terminal raw. `AltScreenGuard`'s doc records that the
    /// hand-rolled rollback it replaced "was itself one of the three leaks"
    /// (#1411). This is a Drop obligation so there is no path to forget.
    ///
    /// The guard is bound BEFORE the fallible call, the ordering
    /// `AltScreenGuard::enter` pays for: from that point the restore is owed
    /// regardless of what the next line does.
    pub(crate) struct InlineGuard {
        /// Restores EXACTLY the mode this frame found — see below.
        _raw: RawModeGuard,
    }

    impl InlineGuard {
        pub(crate) fn enter() -> io::Result<Self> {
            // **Not `crossterm::enable_raw_mode`**, and C2b (#1891) paid for
            // the difference. crossterm keeps ONE process-global "mode prior
            // to raw", so under nesting the inner `enter` is a no-op and the
            // inner `drop` restores GLOBALLY: the outer frame is still drawn
            // while the terminal is already cooked, its keyboard
            // line-buffered and kernel-echoed. `RawModeGuard` saves the
            // termios, so each frame restores what IT found and nesting
            // composes. `a_nested_frame_does_not_restore_the_terminal_early`
            // is the PTY test that caught this version doing it wrong.
            Ok(Self {
                _raw: RawModeGuard::enter()?,
            })
        }
    }

    /// A meaning, as this surface draws it.
    ///
    /// The one place `Emphasis` becomes a `Style`. Every role is told apart
    /// by a MODIFIER (bold, italic, dim, reversed), so the surface reads
    /// correctly on a `NO_COLOR` terminal without a second code path. The
    /// colour underneath is the theme's body text: without it the rows fell
    /// back to the terminal's own foreground inside a themed border.
    fn style_of(emphasis: Emphasis) -> Style {
        let text = crate::theme::style(newt_core::tty::theme::Role::Text);
        match emphasis {
            Emphasis::Plain => text,
            Emphasis::Strong | Emphasis::Heading(_) => text.add_modifier(Modifier::BOLD),
            Emphasis::Emphasis => text.add_modifier(Modifier::ITALIC),
            Emphasis::Code | Emphasis::Quote | Emphasis::Marker => text.add_modifier(Modifier::DIM),
            Emphasis::Struck => text.add_modifier(Modifier::CROSSED_OUT),
        }
    }

    /// The rows a boxed dialog draws: the view's, less what its border
    /// already says. The plain projection keeps both, because it has no
    /// border to say them.
    fn boxed_rows(view: &InteractionView) -> Vec<ViewRow> {
        let mut rows: Vec<ViewRow> = view
            .rows()
            .iter()
            // The legend along the bottom edge is the one key list.
            .filter(|row| {
                row.kind != RowKind::Note
                    || !matches!(row.spans.as_slice(), [span] if span.text == newt_core::tty::MODAL_CONTROL_HINT)
            })
            .cloned()
            .collect();
        // The title names the dialog.
        if let Some(span) = rows.first_mut().and_then(|row| row.spans.first_mut()) {
            if let Some(rest) = span.text.strip_prefix(crate::permissions::FREE_TEXT_PREFIX) {
                span.text = rest.to_string();
            }
        }
        rows
    }

    /// One view row as a styled ratatui line.
    fn line_of(row: &ViewRow, selected: Option<usize>) -> Line<'static> {
        let is_cursor = matches!(row.kind, RowKind::Option { index } if Some(index) == selected);
        let mut spans: Vec<TuiSpan<'static>> = Vec::new();
        // The cursor is a leading marker AND a reversed row: a reversed row
        // alone is invisible on a monochrome terminal that ignores it.
        if matches!(row.kind, RowKind::Option { .. }) {
            spans.push(TuiSpan::raw(if is_cursor { "> " } else { "  " }));
        }
        for span in &row.spans {
            let mut style = style_of(span.emphasis);
            if is_cursor {
                style = style.add_modifier(Modifier::REVERSED);
            }
            spans.push(TuiSpan::styled(span.text.clone(), style));
        }
        Line::from(spans)
    }

    fn initial_view(interaction: &SurfaceInteraction) -> InteractionView {
        let mut view = InteractionView::new(interaction);
        // A host-selected offered default matches the plain adapter. Without
        // one, permission and confirmation windows retain Deny/Cancel.
        let options: Vec<_> = interaction
            .definition
            .controls
            .iter()
            .filter_map(|control| match &control.kind {
                ControlKind::Choice { options } => Some(options.as_slice()),
                _ => None,
            })
            .flatten()
            .collect();
        let selected = interaction
            .default_choice()
            .and_then(|default| options.iter().position(|option| option.id == default.id))
            .or_else(|| {
                options
                    .iter()
                    .position(|option| option.role == SemanticRole::Deny)
            })
            .or_else(|| {
                options
                    .iter()
                    .position(|option| option.role == SemanticRole::Cancel)
            });
        if let Some(index) = selected {
            view.move_selection(index as isize);
        }
        view
    }

    fn physical_rows(row: &ViewRow, width: u16) -> usize {
        let text: String = line_of(row, None)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        newt_core::tty::wrap_line(&text, usize::from(width.max(1))).len()
    }

    pub(crate) fn requested_rows(interaction: &SurfaceInteraction, cols: u16) -> u16 {
        let input = ModalInput::new(interaction);
        let body: usize = input
            .rows
            .iter()
            .map(|row| physical_rows(row, cols.saturating_sub(2)))
            .sum();
        // Two borders, a control legend, and an editable answer for a form.
        u16::try_from(body + 3 + usize::from(input.options.is_none())).unwrap_or(u16::MAX)
    }

    fn draw(frame: &mut ratatui::Frame, input: &mut ModalInput) {
        if let Some(inspection) = &mut input.inspection {
            let body = crate::modal::frame(
                frame,
                frame.area(),
                &crate::modal::Chrome {
                    title: "inspect pending request — no decision submitted",
                    ..Default::default()
                },
            );
            inspection.draw(frame, body);
            return;
        }
        let free_text = input.options.is_none();
        let lines: Vec<Line<'static>> = input
            .rows
            .iter()
            .map(|row| line_of(row, input.selection()))
            .collect();
        // What the body can show, so the chrome can say where it is: two
        // borders, the legend, and the answer row of a free-text question.
        let area = frame.area();
        let visible = usize::from(area.height.saturating_sub(3 + u16::from(free_text)));
        let total: usize = input
            .rows
            .iter()
            .map(|row| physical_rows(row, area.width.saturating_sub(2)))
            .sum();
        let overflows = total > visible;
        input.scroll = input.scroll.min(total.saturating_sub(visible) as u16);
        let top = usize::from(input.scroll);
        let keys = if free_text {
            "Enter send · Esc cancel"
        } else {
            "↑↓ select · Enter confirm · Esc cancel"
        };
        // The legend is cut on the right, so the keys that answer and leave
        // are kept longest, then scrolling.
        let scroll = if overflows {
            " · PgUp/PgDn scroll"
        } else {
            ""
        };
        let hint = format!("{keys}{scroll} · Ctrl-C exit · F4 inspect");
        // The shared edge, for the reason it exists: a prompt that has TAKEN
        // THE KEYBOARD should not be shaped like ordinary output.
        //
        // This does not break the monochrome contract documented on
        // `style_of`. That contract is "the surface reads correctly on a
        // NO_COLOR terminal without a second code path", and a border
        // satisfies it STRUCTURALLY — the box is drawn with glyphs, so it
        // survives a terminal that discards every colour. The hue is additive:
        // the rows still tell their roles apart by modifier alone, over the
        // theme's one body colour (`style_of`).
        let body = crate::modal::frame(
            frame,
            frame.area(),
            &crate::modal::Chrome {
                // A free-text question is not a list to pick from: its chrome
                // names the question and the keys that answer it, nothing
                // about selecting.
                title: if input.view.selected().is_some() {
                    "decision required"
                } else if free_text {
                    "question"
                } else {
                    "input required"
                },
                // A reader who cannot see the end should know there is one.
                subtitle: overflows.then(|| {
                    format!(
                        "· lines {}–{} of {total}",
                        top + 1,
                        (top + visible).min(total)
                    )
                }),
                hint: Some(&hint),
            },
        );
        let body = if input.options.is_none() {
            let [body, answer_area] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(body);
            // The chat prompt's own chevron and colours: this row is where
            // the operator types, the same as that one.
            let chevron = crate::rich_input::ECHO_CHEVRON;
            let shown = input.echo.display(&input.answer);
            let fitted = newt_core::tty::fit_line(
                &format!("{chevron}{shown}"),
                usize::from(answer_area.width),
            );
            let shown = format!("{}{}{}", fitted.head, fitted.fade, fitted.ellipsis);
            // A row cut inside the chevron has no chevron left to colour, and
            // a byte offset into it can land inside the ellipsis.
            let (mark, typed) = match shown.strip_prefix(chevron) {
                Some(typed) => (chevron, typed),
                None => ("", shown.as_str()),
            };
            let role = |role| crate::theme::style(role);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    TuiSpan::styled(mark.to_string(), role(newt_core::tty::theme::Role::Accent)),
                    TuiSpan::styled(typed.to_string(), role(newt_core::tty::theme::Role::Text)),
                ])),
                answer_area,
            );
            if answer_area.width > 0 && answer_area.height > 0 {
                frame.set_cursor_position((
                    answer_area.x
                        + (newt_core::tty::str_width(&shown) as u16).min(answer_area.width - 1),
                    answer_area.y,
                ));
            }
            body
        } else {
            body
        };
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((input.scroll, 0)),
            body,
        );
        if overflows {
            // On the border column, so the body keeps its width and the
            // wrapping `physical_rows` measured.
            let mut state = ScrollbarState::new(total.saturating_sub(visible)).position(top);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight).style(crate::theme::style(
                    newt_core::tty::theme::Role::ModalBorder,
                )),
                Rect {
                    width: body.width + 1,
                    ..body
                },
                &mut state,
            );
        }
    }

    /// Present one interaction on the terminal and report what the operator
    /// did.
    ///
    /// Blocking; owns the terminal for its lifetime and hands it back on every
    /// exit path.
    ///
    /// Returns the outcome AND the canonical text, so the caller can commit
    /// it once the guard has erased the frame. Committing from in here would
    /// write into a region this function is about to clear.
    pub(crate) fn present(
        interaction: &SurfaceInteraction,
    ) -> io::Result<(HumanQuestionOutcome, String)> {
        run(inline_reader(interaction)?)
    }

    fn inline_reader(interaction: &SurfaceInteraction) -> io::Result<ModalReader> {
        let guard = InlineGuard::enter()?;
        let (terminal, cleanup) = open_frame(interaction)?;
        let mut reader = ModalReader::new(terminal, interaction, cleanup)?;
        reader._inline = Some(guard);
        Ok(reader)
    }

    /// The frame's rows, and who erases them when it closes: under the
    /// transcript when the arbiter can place them, the bottom rows otherwise
    /// (`inline_viewport::lease_below_transcript`). Called again on a resize.
    fn open_frame(
        interaction: &SurfaceInteraction,
    ) -> io::Result<(crate::inline_viewport::InlineTerm, Cleanup)> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let height = requested_rows(interaction, cols).min(rows).max(1);
        // #1950: through the ONE inline constructor. A permission frame that
        // will not open is a decision the operator never gets to make.
        // #1979: Shift, for `config_panel`'s reason — a permission frame opens
        // DURING a turn, over whatever is already pinned to the bottom.
        let lease = crate::inline_viewport::lease_below_transcript(
            height,
            newt_core::tty::OnCollision::Shift,
        )?;
        let cleanup = if lease.cursor_return().is_some() {
            Cleanup::Lease
        } else {
            Cleanup::Viewport
        };
        Ok((crate::inline_viewport::inline_terminal(lease)?, cleanup))
    }

    /// Same modal on the cockpit's saved real terminal. The presenter owns
    /// the row reservation and cleanup; this loop owns input until dismissal.
    #[cfg(unix)]
    pub(crate) fn present_in(
        terminal: crate::inline_viewport::InlineTerm,
        interaction: &SurfaceInteraction,
    ) -> io::Result<(HumanQuestionOutcome, String)> {
        run(reader(terminal, interaction)?)
    }

    struct ModalInput {
        view: InteractionView,
        /// [`boxed_rows`] of `view`, which never change while it is open.
        rows: Vec<ViewRow>,
        options: Option<Vec<newt_interaction::ChoiceOption>>,
        answer: String,
        echo: Echo,
        scroll: u16,
        armed: bool,
        inspection: Option<crate::transcript_pager::OutputView>,
        inspection_text: String,
    }

    impl ModalInput {
        fn new(interaction: &SurfaceInteraction) -> Self {
            let view = initial_view(interaction);
            Self {
                rows: boxed_rows(&view),
                view,
                options: match interaction.definition.controls.as_slice() {
                    [control] => match &control.kind {
                        ControlKind::Choice { options } => Some(options.clone()),
                        _ => None,
                    },
                    _ => None,
                },
                answer: String::new(),
                echo: newt_core::interaction_terminal::echo_for(&interaction.definition),
                scroll: 0,
                armed: true,
                inspection: None,
                inspection_text: format!(
                    "{}\n\n{}",
                    interaction.definition.markdown,
                    interaction.definition.note.as_deref().unwrap_or_default()
                ),
            }
        }

        fn new_shared(interaction: &SurfaceInteraction) -> Self {
            Self {
                armed: false,
                ..Self::new(interaction)
            }
        }

        fn selection(&self) -> Option<usize> {
            let options = self.options.as_ref()?;
            if self.answer.is_empty() {
                return self.armed.then(|| self.view.selected()).flatten();
            }
            let id = newt_interaction::binding::resolve_typed(options, &self.answer)?;
            options.iter().position(|option| option.id == id)
        }

        fn event(&mut self, event: Event, area: Rect) -> Option<PromptLine> {
            if let Some(inspection) = &mut self.inspection {
                if let Event::Key(key) = event {
                    if inspection.key(key, usize::from(area.height.saturating_sub(3))) {
                        self.inspection = None;
                    }
                }
                return None;
            }
            if matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press && key.code == KeyCode::F(4))
            {
                self.inspection = Some(crate::transcript_pager::OutputView::new(
                    self.inspection_text.lines(),
                ));
                return None;
            }
            if let Event::Paste(text) = event {
                self.answer
                    .extend(text.chars().filter(|ch| !ch.is_control()));
                return None;
            }
            let Event::Key(key) = event else {
                return None;
            };
            if key.kind != KeyEventKind::Press {
                return None;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                // Ctrl-C / Ctrl-D exit; Esc backs out. Same vocabulary as the
                // plain modal, so the controls do not change with the surface.
                KeyCode::Char('c' | 'd') if ctrl => return Some(PromptLine::Exit),
                KeyCode::Esc => return Some(PromptLine::Back),
                KeyCode::Up | KeyCode::Down if self.options.is_some() => {
                    if let Some(selection) = self.selection() {
                        self.view.move_selection(
                            selection as isize - self.view.selected().unwrap_or(0) as isize,
                        );
                    }
                    self.answer.clear();
                    self.armed = true;
                    let (view, scroll) = (&mut self.view, &mut self.scroll);
                    view.move_selection(if key.code == KeyCode::Up { -1 } else { 1 });
                    let visible = usize::from(area.height.saturating_sub(3));
                    let mut before = 0;
                    for row in &self.rows {
                        let count = physical_rows(row, area.width.saturating_sub(2));
                        if matches!(row.kind, RowKind::Option { index } if Some(index) == view.selected())
                        {
                            if before < usize::from(*scroll) {
                                *scroll = before as u16;
                            } else if before + count > usize::from(*scroll) + visible {
                                *scroll = (before + count).saturating_sub(visible) as u16;
                            }
                            break;
                        }
                        before += count;
                    }
                }
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(5),
                KeyCode::PageDown => {
                    // Stop at the last full page, as `draw` lays it out: two
                    // borders, the legend, and a free-text answer row.
                    let chrome = 3 + u16::from(self.options.is_none());
                    let total: usize = self
                        .rows
                        .iter()
                        .map(|row| physical_rows(row, area.width.saturating_sub(2)))
                        .sum();
                    let last =
                        total.saturating_sub(usize::from(area.height.saturating_sub(chrome)));
                    self.scroll = self.scroll.saturating_add(5).min(last as u16);
                }
                KeyCode::Backspace => {
                    self.answer.pop();
                }
                KeyCode::Enter => {
                    let typed = std::mem::take(&mut self.answer);
                    let answer = match &self.options {
                        Some(options) if !typed.is_empty() => {
                            newt_interaction::binding::resolve_typed(options, &typed)
                                .map_or(typed, |id| id.as_str().to_string())
                        }
                        Some(_) if self.armed => self.view.answer_for_selection().unwrap_or(typed),
                        _ => typed,
                    };
                    return Some(PromptLine::Line(answer));
                }
                KeyCode::Char(typed) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                    self.answer.push(typed);
                }
                _ => {}
            }
            None
        }
    }

    /// Who erases the frame's rows when the reader closes, and what a resize
    /// does to it.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Cleanup {
        /// The cockpit presenter reserved the rows and cleans them up; a
        /// resize re-seats the frame on the bottom rows it lends.
        Presenter,
        /// The arbiter placed the rows and erases them when the lease drops,
        /// returning the cursor to the transcript
        /// (`Terminal::lease_below_cursor`). A resize closes and re-opens the
        /// frame: a `Fixed` viewport never re-measures, and ratatui's own
        /// `resize`/`clear` run `ESC[J` to the end of the screen, through
        /// whatever holds the rows below.
        Lease,
        /// A bottom-anchored `Inline` viewport, kept for a terminal that does
        /// not answer the cursor query: ratatui's `clear` erases it on drop,
        /// and its own autoresize follows a resize.
        Viewport,
    }

    /// Pollable shared dialog: web verdicts and terminal answers keep their
    /// existing arbitration loop while drawing and decoding the same window.
    pub(crate) struct ModalReader {
        /// `None` only after a resize closed the frame and re-opening failed.
        terminal: Option<crate::inline_viewport::InlineTerm>,
        input: ModalInput,
        interaction: SurfaceInteraction,
        cleanup: Cleanup,
        _raw: RawModeGuard,
        // The inline guard took raw mode first, so it must restore it LAST.
        _inline: Option<InlineGuard>,
    }

    impl ModalReader {
        fn new(
            terminal: crate::inline_viewport::InlineTerm,
            interaction: &SurfaceInteraction,
            cleanup: Cleanup,
        ) -> io::Result<Self> {
            Ok(Self {
                terminal: Some(terminal),
                input: ModalInput::new(interaction),
                interaction: interaction.clone(),
                cleanup,
                _raw: RawModeGuard::enter()?,
                _inline: None,
            })
        }
    }

    pub(crate) fn reader(
        terminal: crate::inline_viewport::InlineTerm,
        interaction: &SurfaceInteraction,
    ) -> io::Result<ModalReader> {
        ModalReader::new(terminal, interaction, Cleanup::Presenter)
    }

    pub(crate) fn reader_for_shared_decision(
        terminal: crate::inline_viewport::InlineTerm,
        interaction: &SurfaceInteraction,
    ) -> io::Result<ModalReader> {
        let mut reader = reader(terminal, interaction)?;
        reader.input = ModalInput::new_shared(interaction);
        Ok(reader)
    }

    pub(crate) fn reader_for_shared_decision_on_terminal(
        interaction: &SurfaceInteraction,
    ) -> io::Result<ModalReader> {
        let mut reader = inline_reader(interaction)?;
        reader.input = ModalInput::new_shared(interaction);
        Ok(reader)
    }

    impl Drop for ModalReader {
        fn drop(&mut self) {
            if self.cleanup == Cleanup::Viewport {
                // A web verdict or unwind must erase the entire viewport
                // before its inline guard gives raw mode back. (A leased frame
                // is erased by its lease, when the terminal field drops.)
                if let Some(terminal) = &mut self.terminal {
                    let _ = terminal.clear();
                }
            }
        }
    }

    impl ControlReader for ModalReader {
        fn poll(&mut self, timeout: Duration) -> io::Result<Option<PromptLine>> {
            let terminal = self
                .terminal
                .as_mut()
                .ok_or_else(|| io::Error::other("the interaction frame has no viewport"))?;
            let input = &mut self.input;
            terminal.draw(|f| draw(f, input))?;
            let area = terminal.get_frame().area();
            if !event::poll(timeout)? {
                return Ok(None);
            }
            let event = event::read()?;
            if let Event::Resize(cols, rows) = event {
                match self.cleanup {
                    Cleanup::Presenter => {
                        let height = requested_rows(&self.interaction, cols).min(rows);
                        terminal.clear()?;
                        terminal.resize(Rect::new(0, rows.saturating_sub(height), cols, height))?;
                    }
                    Cleanup::Lease => {
                        // Close, so the lease returns the cursor to the
                        // transcript; then re-open from there, on the
                        // resized screen.
                        self.terminal = None;
                        let (terminal, cleanup) = open_frame(&self.interaction)?;
                        self.terminal = Some(terminal);
                        self.cleanup = cleanup;
                    }
                    Cleanup::Viewport => {}
                }
                self.input.scroll = 0;
                return Ok(None);
            }
            Ok(self.input.event(event, area))
        }
    }

    fn run(mut reader: ModalReader) -> io::Result<(HumanQuestionOutcome, String)> {
        let canonical = reader.input.view.fallback().to_string();
        loop {
            let Some(line) = reader.poll(Duration::from_millis(250))? else {
                continue;
            };
            let outcome = match line {
                PromptLine::Line(answer) => HumanQuestionOutcome::Answer(answer),
                PromptLine::Back => HumanQuestionOutcome::Cancelled,
                PromptLine::Exit => HumanQuestionOutcome::ExitRequested,
                PromptLine::Eof => HumanQuestionOutcome::InputClosed,
            };
            return Ok((outcome, canonical));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use newt_interaction::{
            ChoiceOption, Control, ControlId, ControlKind, InteractionDefinition, InteractionKind,
            OptionId, Requirement, SemanticRole,
        };

        fn interaction() -> SurfaceInteraction {
            SurfaceInteraction::blocking(InteractionDefinition::new(
                InteractionKind::Choice,
                "Connect the calendar MCP server to `calendar.example.test` so it can read the morning schedule.",
                vec![Control {
                    id: ControlId::new("decision").unwrap(),
                    kind: ControlKind::Choice { options: vec![
                        ("once", "a", "allow once", SemanticRole::Allow),
                        ("permanent", "A", "allow permanently (saved for future launches)", SemanticRole::Allow),
                        ("deny", "d", "deny", SemanticRole::Deny),
                    ].into_iter().map(|(id, key, label, role)| ChoiceOption {
                        id: OptionId::new(id).unwrap(), key: key.into(), label: label.into(), role, aliases: vec![],
                    }).collect() },
                    label: String::new(), requirement: Requirement::Required,
                }],
            ))
        }

        #[test]
        fn modal_default_selection_never_grants_authority() {
            let view = initial_view(&interaction());
            assert_eq!(view.answer_for_selection().as_deref(), Some("deny"));
        }

        #[test]
        fn modal_height_counts_wrapped_reason_and_every_choice() {
            let view = InteractionView::new(&interaction());
            let height = requested_rows(&interaction(), 30);
            assert!(
                height > view.rows().len() as u16 + 2,
                "a narrow window must reserve the wrapped reason and choices"
            );
        }

        fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
            Event::Key(crossterm::event::KeyEvent::new(code, modifiers))
        }

        #[test]
        fn modal_input_enter_defaults_to_deny() {
            let mut input = ModalInput::new(&interaction());
            assert_eq!(
                input.event(
                    key(KeyCode::Enter, KeyModifiers::NONE),
                    Rect::new(0, 0, 80, 24)
                ),
                Some(newt_core::tty::PromptLine::Line("deny".into()))
            );
        }

        #[test]
        fn inspecting_a_pending_decision_never_answers_or_changes_it() {
            let mut input = ModalInput::new(&interaction());
            let area = Rect::new(0, 0, 40, 10);
            input.event(key(KeyCode::Char('a'), KeyModifiers::NONE), area);
            assert_eq!(
                input.event(key(KeyCode::F(4), KeyModifiers::NONE), area),
                None
            );
            assert!(input.inspection.is_some());
            for code in [KeyCode::Enter, KeyCode::Char('d'), KeyCode::Down] {
                assert_eq!(input.event(key(code, KeyModifiers::NONE), area), None);
            }
            assert_eq!(
                input.event(key(KeyCode::Esc, KeyModifiers::NONE), area),
                None
            );
            assert!(input.inspection.is_none());
            assert_eq!(input.answer, "a");
            assert_eq!(
                input.event(key(KeyCode::Enter, KeyModifiers::NONE), area),
                Some(PromptLine::Line("once".into()))
            );
        }

        #[test]
        fn modal_input_enter_uses_the_configured_offered_default() {
            for expected in ["once", "deny"] {
                let interaction =
                    interaction().with_default_option(OptionId::new(expected).unwrap());
                let mut input = ModalInput::new(&interaction);
                assert_eq!(input.view.answer_for_selection().as_deref(), Some(expected));
                assert_eq!(
                    input.event(
                        key(KeyCode::Enter, KeyModifiers::NONE),
                        Rect::new(0, 0, 80, 24)
                    ),
                    Some(PromptLine::Line(expected.into()))
                );
            }
        }

        #[test]
        fn modal_input_ignores_an_unoffered_default() {
            let interaction = interaction().with_default_option(OptionId::new("absent").unwrap());
            let mut input = ModalInput::new(&interaction);
            assert_eq!(
                input.event(
                    key(KeyCode::Enter, KeyModifiers::NONE),
                    Rect::new(0, 0, 80, 24)
                ),
                Some(PromptLine::Line("deny".into()))
            );
        }

        #[test]
        fn modal_input_preserves_case_sensitive_permanent_grants() {
            for (typed, modifiers, expected) in [
                ('a', KeyModifiers::NONE, "once"),
                ('A', KeyModifiers::SHIFT, "permanent"),
            ] {
                let mut input = ModalInput::new(&interaction());
                assert_eq!(
                    input.event(
                        key(KeyCode::Char(typed), modifiers),
                        Rect::new(0, 0, 80, 24)
                    ),
                    None,
                    "an accelerator selects; Enter confirms without leaking to the next prompt"
                );
                assert_eq!(
                    input.event(
                        key(KeyCode::Enter, KeyModifiers::NONE),
                        Rect::new(0, 0, 80, 24)
                    ),
                    Some(newt_core::tty::PromptLine::Line(expected.into())),
                    "permanent approval must require its distinct accelerator"
                );
            }
        }

        #[test]
        fn modal_input_confirmation_never_defaults_to_yes() {
            let interaction = SurfaceInteraction::blocking(newt_core::interaction_form::confirm(
                "Delete this conversation?",
                "",
                "yes",
                "no",
            ));
            let mut input = ModalInput::new(&interaction);
            assert_eq!(
                input.event(
                    key(KeyCode::Enter, KeyModifiers::NONE),
                    Rect::new(0, 0, 80, 24)
                ),
                Some(PromptLine::Line("no".into()))
            );
        }

        #[test]
        fn modal_input_resolves_complete_numbered_keys_and_aliases() {
            let menu = SurfaceInteraction::blocking(newt_core::interaction_form::menu(
                "Choose a setting",
                "",
                &[("1", "first"), ("10", "tenth"), ("11", "eleventh")],
            ));
            let confirm = SurfaceInteraction::blocking(newt_core::interaction_form::confirm(
                "Continue?",
                "",
                "yes",
                "no",
            ));
            for (interaction, typed, expected) in [
                (&menu, "10", "10"),
                (&menu, "11", "11"),
                (&confirm, "Y", "yes"),
            ] {
                let mut input = ModalInput::new(interaction);
                for ch in typed.chars() {
                    assert_eq!(
                        input.event(
                            key(KeyCode::Char(ch), KeyModifiers::NONE),
                            Rect::new(0, 0, 80, 24)
                        ),
                        None
                    );
                }
                assert_eq!(
                    input.event(
                        key(KeyCode::Enter, KeyModifiers::NONE),
                        Rect::new(0, 0, 80, 24)
                    ),
                    Some(PromptLine::Line(expected.into()))
                );
            }
        }

        #[test]
        fn modal_input_shared_decision_waits_for_explicit_selection() {
            let mut input = ModalInput::new_shared(&interaction());
            let area = Rect::new(0, 0, 80, 24);
            assert_eq!(input.selection(), None);
            assert_eq!(
                input.event(key(KeyCode::Enter, KeyModifiers::NONE), area),
                Some(PromptLine::Line(String::new()))
            );
            assert_eq!(
                input.event(key(KeyCode::Char('A'), KeyModifiers::SHIFT), area),
                None
            );
            assert_eq!(
                input.event(key(KeyCode::Enter, KeyModifiers::NONE), area),
                Some(PromptLine::Line("permanent".into()))
            );
        }

        #[test]
        fn modal_input_keeps_back_and_exit_distinct_from_answers() {
            for (code, modifiers, expected) in [
                (
                    KeyCode::Esc,
                    KeyModifiers::NONE,
                    newt_core::tty::PromptLine::Back,
                ),
                (
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                    newt_core::tty::PromptLine::Exit,
                ),
                (
                    KeyCode::Char('d'),
                    KeyModifiers::CONTROL,
                    newt_core::tty::PromptLine::Exit,
                ),
            ] {
                let mut input = ModalInput::new(&interaction());
                assert_eq!(
                    input.event(key(code, modifiers), Rect::new(0, 0, 80, 24)),
                    Some(expected)
                );
            }
        }

        #[test]
        fn modal_input_edits_and_submits_free_text_without_consuming_ordinary_letters() {
            let interaction =
                SurfaceInteraction::blocking(crate::permissions::free_text_form("Name this task"));
            let mut input = ModalInput::new(&interaction);
            let area = Rect::new(0, 0, 80, 24);
            for code in [
                KeyCode::Char('a'),
                KeyCode::Char('é'),
                KeyCode::Char('x'),
                KeyCode::Backspace,
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Char('!'),
            ] {
                assert_eq!(input.event(key(code, KeyModifiers::NONE), area), None);
            }
            assert_eq!(
                input.event(key(KeyCode::Enter, KeyModifiers::NONE), area),
                Some(newt_core::tty::PromptLine::Line("aé!".into()))
            );
        }

        #[test]
        fn workspace_review_wraps_and_scrolls_without_losing_save_or_cancel() {
            let _env = crate::test_env_guard::env_read_guard();
            let (_directory, interaction) = crate::workspace_settings::tests::review_for_renderer();
            for (width, height) in [(80, 20), (40, 12)] {
                let area = Rect::new(0, 0, width, height);
                let mut input = ModalInput::new(&interaction);
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                let mut snapshots = Vec::new();
                let mut all_visible = String::new();
                let mut reached_end = false;
                for _ in 0..512 {
                    terminal.draw(|frame| draw(frame, &mut input)).unwrap();
                    let buffer = terminal.backend().buffer();
                    let rows: Vec<String> = (0..height)
                        .map(|y| {
                            (0..width)
                                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                                .collect()
                        })
                        .collect();
                    for y in 1..height.saturating_sub(2) {
                        for x in 1..width.saturating_sub(1) {
                            all_visible.extend(
                                buffer
                                    .cell((x, y))
                                    .unwrap()
                                    .symbol()
                                    .chars()
                                    .filter(|ch| !ch.is_whitespace()),
                            );
                        }
                    }
                    snapshots.push(rows.join("\n"));
                    let before = input.scroll;
                    assert_eq!(
                        input.event(key(KeyCode::PageDown, KeyModifiers::NONE), area),
                        None
                    );
                    if input.scroll == before {
                        reached_end = true;
                        break;
                    }
                }
                assert!(reached_end, "the finite review can be fully inspected");
                for text in [
                    "Active this session",
                    "Saved for next launch",
                    "Draft for next launch",
                    "workspace-tail-marker",
                    "[y]es, save for next launch",
                    "[n]o, cancel (default)",
                ] {
                    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
                    assert!(
                        all_visible.contains(&compact),
                        "missing {text:?} at {width}x{height}\n{}",
                        snapshots.join("\n--- scroll ---\n")
                    );
                }
                println!(
                    "WORKSPACE REVIEW {width}x{height} FIRST\n{}\nLAST\n{}",
                    snapshots.first().unwrap(),
                    snapshots.last().unwrap()
                );
                assert_eq!(
                    input.event(key(KeyCode::Enter, KeyModifiers::NONE), area),
                    Some(PromptLine::Line("no".into())),
                    "scrolling never changes Cancel as the default"
                );
                let mut explicit_save = ModalInput::new(&interaction);
                assert_eq!(
                    explicit_save.event(key(KeyCode::Char('y'), KeyModifiers::NONE), area),
                    None
                );
                assert_eq!(
                    explicit_save.event(key(KeyCode::Enter, KeyModifiers::NONE), area),
                    Some(PromptLine::Line("yes".into()))
                );
            }
        }

        fn drawn(question: &str, width: u16, height: u16, pages: usize) -> String {
            let interaction =
                SurfaceInteraction::blocking(crate::permissions::free_text_form(question));
            screen(&interaction, width, height, pages)
        }

        fn screen(
            interaction: &SurfaceInteraction,
            width: u16,
            height: u16,
            pages: usize,
        ) -> String {
            let mut input = ModalInput::new(interaction);
            let area = Rect::new(0, 0, width, height);
            for _ in 0..pages {
                input.event(key(KeyCode::PageDown, KeyModifiers::NONE), area);
            }
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &mut input)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer.cell((x, y)).unwrap().symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// The border names the dialog and the legend lists the keys, once
        /// each. The plain projection's `Prompt —` and control note said both
        /// a second time, with `Esc=back` beside `Esc cancel`.
        #[test]
        fn a_boxed_question_says_each_thing_once() {
            let screen = drawn("Which branches should go?", 70, 5, 0);
            assert!(screen.contains(" question "), "{screen}");
            assert!(screen.contains("Which branches should go?"), "{screen}");
            assert!(!screen.contains("Prompt"), "{screen}");
            assert!(!screen.contains("Esc=back"), "{screen}");
            assert_eq!(screen.matches("Esc").count(), 1, "{screen}");
            assert!(screen.contains("›"), "{screen}");
            // Nothing to scroll, so nothing about scrolling.
            assert!(
                !screen.contains("PgUp") && !screen.contains("lines"),
                "{screen}"
            );
            assert_eq!(
                requested_rows(
                    &SurfaceInteraction::blocking(crate::permissions::free_text_form("One row?")),
                    70
                ),
                5,
                "two borders, the question, the answer row and the legend"
            );
        }

        /// A question longer than the box scrolls under a fixed answer row,
        /// says where it is, and stops at its last line.
        #[test]
        fn a_long_question_scrolls_and_says_where_it_is() {
            let question = (1..=20)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n\n");
            let top = drawn(&question, 80, 8, 0);
            assert!(top.contains("· lines 1–4 of 20"), "{top}");
            assert!(top.contains("PgUp/PgDn scroll"), "{top}");
            assert!(top.contains("line 1") && !top.contains("line 20"), "{top}");
            assert!(
                top.contains('█'),
                "the right border carries a scrollbar: {top}"
            );
            // Narrower than the legend: the cut takes the tail, and the keys
            // that answer, leave and scroll stay.
            let narrow = drawn(&question, 46, 8, 0);
            assert!(
                narrow.contains("Enter send · Esc cancel · PgUp/PgDn scroll"),
                "{narrow}"
            );
            // A decision dialog's legend is longer: 80 columns still show how
            // to scroll, and 40 still show how to leave.
            let decision = SurfaceInteraction::blocking(newt_core::interaction_form::confirm(
                &question, "", "yes", "no",
            ));
            let wide = screen(&decision, 80, 8, 0);
            assert!(wide.contains("Esc cancel · PgUp/PgDn scroll"), "{wide}");
            let narrow = screen(&decision, 40, 8, 0);
            assert!(narrow.contains("Esc cancel"), "{narrow}");
            let end = drawn(&question, 80, 8, 99);
            assert!(end.contains("· lines 17–20 of 20"), "{end}");
            assert!(end.contains("line 20") && end.contains("›"), "{end}");
        }

        /// A pane squeezed to a few columns cuts the answer row inside its
        /// chevron. Splitting that row at the chevron's byte length landed
        /// inside the ellipsis and panicked.
        #[test]
        fn a_squeezed_question_still_draws() {
            let interaction =
                SurfaceInteraction::blocking(crate::permissions::free_text_form("Narrow?"));
            // Nothing typed, a narrow character, and a wide one.
            for typed in [None, Some('x'), Some('界')] {
                for (width, height) in (1..=8).flat_map(|w| (1..=9).map(move |h| (w, h))) {
                    let mut input = ModalInput::new(&interaction);
                    if let Some(c) = typed {
                        let area = Rect::new(0, 0, width, height);
                        input.event(key(KeyCode::Char(c), KeyModifiers::NONE), area);
                    }
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap()
                        .draw(|frame| draw(frame, &mut input))
                        .unwrap();
                }
            }
        }

        /// The question takes the theme's text colour and the answer row's
        /// chevron its accent. Before #3119 only the border was themed.
        #[test]
        fn a_boxed_question_follows_the_theme() {
            use newt_core::tty::theme::Role;
            let interaction =
                SurfaceInteraction::blocking(crate::permissions::free_text_form("Themed?"));
            let mut input = ModalInput::new(&interaction);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 5)).unwrap();
            terminal.draw(|frame| draw(frame, &mut input)).unwrap();
            let buffer = terminal.backend().buffer();
            // Inside the left border: row 1 is the question, row 2 the answer.
            let (question, chevron) = (buffer.cell((1, 1)).unwrap(), buffer.cell((1, 2)).unwrap());
            assert_eq!((question.symbol(), chevron.symbol()), ("T", "›"));
            assert_eq!(question.fg, crate::theme::color(Role::Text));
            assert_eq!(chevron.fg, crate::theme::color(Role::Accent));
            assert_ne!(question.fg, chevron.fg, "two roles, two colours");
        }

        #[test]
        fn modal_input_masks_secret_answer_in_the_shared_renderer() {
            let mut definition = crate::permissions::free_text_form("Enter the credential");
            definition.controls[0].kind = ControlKind::Secret;
            let interaction = SurfaceInteraction::blocking(definition);
            let mut input = ModalInput::new(&interaction);
            let area = Rect::new(0, 0, 60, 12);
            let secret = "synthetic-secret";
            for ch in secret.chars() {
                assert_eq!(
                    input.event(key(KeyCode::Char(ch), KeyModifiers::NONE), area),
                    None
                );
            }
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(area.width, area.height))
                    .unwrap();
            terminal.draw(|frame| draw(frame, &mut input)).unwrap();
            let visible: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(
                !visible.contains(secret),
                "a secret control must never paint its answer"
            );
            assert!(
                visible.contains(&"*".repeat(secret.chars().count())),
                "the masked answer must remain visible as feedback"
            );
            assert_eq!(
                input.event(key(KeyCode::Enter, KeyModifiers::NONE), area),
                Some(newt_core::tty::PromptLine::Line(secret.into())),
                "masking changes presentation, not the submitted value"
            );
        }
    }
}

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
    use crossterm::terminal::{Clear, ClearType};
    use newt_core::interaction_surface::SurfaceInteraction;
    use newt_core::tty::raw_mode::RawModeGuard;
    use newt_core::tty::{ControlReader, Echo, PromptLine};
    use newt_core::HumanQuestionOutcome;
    use newt_interaction::{ControlKind, SemanticRole};
    use ratatui::layout::{Constraint, Layout, Rect};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span as TuiSpan};
    use ratatui::widgets::{Paragraph, Wrap};
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

    impl Drop for InlineGuard {
        fn drop(&mut self) {
            // Erase the reserved region before handing the terminal back, or
            // the next committed line prints over a live widget frame. The
            // raw-mode restore is `_raw`'s Drop, which runs after this body.
            let mut out = io::stdout();
            let _ = crossterm::execute!(
                out,
                crossterm::cursor::MoveToColumn(0),
                Clear(ClearType::FromCursorDown)
            );
        }
    }

    /// A meaning, as this surface draws it.
    ///
    /// The one place `Emphasis` becomes a `Style`. Monochrome by
    /// construction: every role maps to a MODIFIER (bold, italic, dim,
    /// reversed), never to a colour, so the surface reads correctly on a
    /// `NO_COLOR` terminal without a second code path to keep in step.
    fn style_of(emphasis: Emphasis) -> Style {
        match emphasis {
            Emphasis::Plain => Style::default(),
            Emphasis::Strong | Emphasis::Heading(_) => {
                Style::default().add_modifier(Modifier::BOLD)
            }
            Emphasis::Emphasis => Style::default().add_modifier(Modifier::ITALIC),
            Emphasis::Code | Emphasis::Quote | Emphasis::Marker => {
                Style::default().add_modifier(Modifier::DIM)
            }
            Emphasis::Struck => Style::default().add_modifier(Modifier::CROSSED_OUT),
        }
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
        // Enter on a newly opened permission window retains the plain
        // adapter's fail-closed default. The role, never label prose, decides.
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
        let denial = options
            .iter()
            .position(|option| option.role == SemanticRole::Deny)
            .or_else(|| {
                options
                    .iter()
                    .position(|option| option.role == SemanticRole::Cancel)
            });
        if let Some(index) = denial {
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
            .view
            .rows()
            .iter()
            .map(|row| physical_rows(row, cols.saturating_sub(2)))
            .sum();
        // Two borders, a control legend, and an editable answer for a form.
        u16::try_from(body + 3 + usize::from(input.options.is_none())).unwrap_or(u16::MAX)
    }

    fn draw(frame: &mut ratatui::Frame, input: &ModalInput) {
        let view = &input.view;
        let lines: Vec<Line<'static>> = view
            .rows()
            .iter()
            .map(|row| line_of(row, input.selection()))
            .collect();
        // The shared edge, for the reason it exists: a prompt that has TAKEN
        // THE KEYBOARD should not be shaped like ordinary output.
        //
        // This does not break the monochrome contract documented on
        // `style_of`. That contract is "the surface reads correctly on a
        // NO_COLOR terminal without a second code path", and a border
        // satisfies it STRUCTURALLY — the box is drawn with glyphs, so it
        // survives a terminal that discards every colour. The hue is additive,
        // and the ROWS are untouched: every span still maps to a modifier,
        // never to a colour.
        let body = crate::modal::frame(
            frame,
            frame.area(),
            &crate::modal::Chrome {
                title: if view.selected().is_some() {
                    "decision required"
                } else {
                    "input required"
                },
                hint: Some("Esc cancel · ↑↓ select · Enter confirm · PgUp/PgDn scroll"),
                ..crate::modal::Chrome::default()
            },
        );
        let body = if input.options.is_none() {
            let [body, answer_area] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(body);
            let shown = input.echo.display(&input.answer);
            let fitted =
                newt_core::tty::fit_line(&format!("> {shown}"), usize::from(answer_area.width));
            let shown = format!("{}{}{}", fitted.head, fitted.fade, fitted.ellipsis);
            frame.render_widget(Paragraph::new(shown.as_str()), answer_area);
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
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let height = requested_rows(interaction, cols).min(rows).max(1);
        let guard = InlineGuard::enter()?;
        // #1950: through the ONE inline constructor. A permission frame that
        // will not open is a decision the operator never gets to make.
        // #1979: Shift, for `config_panel`'s reason — a permission frame opens
        // DURING a turn, over whatever is already pinned to the bottom.
        let lease =
            crate::inline_viewport::lease_bottom_rows(height, newt_core::tty::OnCollision::Shift)?;
        let terminal = crate::inline_viewport::inline_terminal(lease)?;
        let mut reader = ModalReader::new(terminal, interaction, false)?;
        reader._inline = Some(guard);
        Ok(reader)
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
        options: Option<Vec<newt_interaction::ChoiceOption>>,
        answer: String,
        echo: Echo,
        scroll: u16,
        armed: bool,
    }

    impl ModalInput {
        fn new(interaction: &SurfaceInteraction) -> Self {
            Self {
                view: initial_view(interaction),
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
                    for row in view.rows() {
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
                    let total: usize = self
                        .view
                        .rows()
                        .iter()
                        .map(|row| physical_rows(row, area.width.saturating_sub(2)))
                        .sum();
                    self.scroll = self
                        .scroll
                        .saturating_add(5)
                        .min(total.saturating_sub(1) as u16);
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

    /// Pollable shared dialog: web verdicts and terminal answers keep their
    /// existing arbitration loop while drawing and decoding the same window.
    pub(crate) struct ModalReader {
        terminal: crate::inline_viewport::InlineTerm,
        input: ModalInput,
        interaction: SurfaceInteraction,
        fixed: bool,
        _raw: RawModeGuard,
        // The inline guard took raw mode first, so it must restore it LAST.
        _inline: Option<InlineGuard>,
    }

    impl ModalReader {
        fn new(
            terminal: crate::inline_viewport::InlineTerm,
            interaction: &SurfaceInteraction,
            fixed: bool,
        ) -> io::Result<Self> {
            Ok(Self {
                terminal,
                input: ModalInput::new(interaction),
                interaction: interaction.clone(),
                fixed,
                _raw: RawModeGuard::enter()?,
                _inline: None,
            })
        }
    }

    pub(crate) fn reader(
        terminal: crate::inline_viewport::InlineTerm,
        interaction: &SurfaceInteraction,
    ) -> io::Result<ModalReader> {
        ModalReader::new(terminal, interaction, true)
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
            if !self.fixed {
                // A web verdict or unwind must erase the entire viewport
                // before its inline guard gives raw mode back.
                let _ = self.terminal.clear();
            }
        }
    }

    impl ControlReader for ModalReader {
        fn poll(&mut self, timeout: Duration) -> io::Result<Option<PromptLine>> {
            let input = &self.input;
            self.terminal.draw(|f| draw(f, input))?;
            if !event::poll(timeout)? {
                return Ok(None);
            }
            let event = event::read()?;
            if let Event::Resize(cols, rows) = event {
                if self.fixed {
                    let height = requested_rows(&self.interaction, cols).min(rows);
                    self.terminal.clear()?;
                    self.terminal.resize(Rect::new(
                        0,
                        rows.saturating_sub(height),
                        cols,
                        height,
                    ))?;
                }
                self.input.scroll = 0;
                return Ok(None);
            }
            Ok(self.input.event(event, self.terminal.get_frame().area()))
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
            terminal.draw(|frame| draw(frame, &input)).unwrap();
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

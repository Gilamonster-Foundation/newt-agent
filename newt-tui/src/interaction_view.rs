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
pub(crate) use terminal::present;

#[cfg(all(test, unix))]
pub(crate) use terminal::InlineGuard;

mod terminal {
    use newt_core::interaction_view::{InteractionView, RowKind, ViewRow};
    use newt_core::markup::spans::Emphasis;

    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{Clear, ClearType};
    use newt_core::interaction_surface::SurfaceInteraction;
    use newt_core::tty::raw_mode::RawModeGuard;
    use newt_core::HumanQuestionOutcome;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span as TuiSpan};
    use ratatui::widgets::{Paragraph, Wrap};
    use std::io;
    use std::time::Duration;

    /// Give the region a ceiling so a long body cannot swallow the scrollback
    /// it is drawn over. Beyond this the operator reads the committed
    /// canonical text, which is printed either way.
    const MAX_ROWS: usize = 16;

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

    fn draw(frame: &mut ratatui::Frame, view: &InteractionView) {
        let lines: Vec<Line<'static>> = view
            .rows()
            .iter()
            .take(MAX_ROWS)
            .map(|row| line_of(row, view.selected()))
            .collect();
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            frame.area(),
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
        let mut view = InteractionView::new(interaction);
        let canonical = view.fallback().to_string();
        let height = view.rows().len().clamp(1, MAX_ROWS) as u16;
        let _guard = InlineGuard::enter()?;
        // #1950: through the ONE inline constructor. A permission frame that
        // will not open is a decision the operator never gets to make.
        // #1979: Shift, for `config_panel`'s reason — a permission frame opens
        // DURING a turn, over whatever is already pinned to the bottom.
        let lease =
            crate::inline_viewport::lease_bottom_rows(height, newt_core::tty::OnCollision::Shift)?;
        let mut terminal = crate::inline_viewport::inline_terminal(lease)?;
        loop {
            terminal.draw(|f| draw(f, &view))?;
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                // Ctrl-C / Ctrl-D exit; Esc backs out. Same vocabulary as the
                // plain modal, so the controls do not change with the surface.
                KeyCode::Char('c' | 'd') if ctrl => {
                    return Ok((HumanQuestionOutcome::ExitRequested, canonical))
                }
                KeyCode::Esc => return Ok((HumanQuestionOutcome::Cancelled, canonical)),
                KeyCode::Up => view.move_selection(-1),
                KeyCode::Down => view.move_selection(1),
                KeyCode::Enter => {
                    let outcome = match view.answer_for_selection() {
                        Some(answer) => HumanQuestionOutcome::Answer(answer),
                        // A form with nothing to pick has nothing Enter can
                        // mean. Refusing beats submitting an empty answer a
                        // security-sensitive caller would then have to judge.
                        None => HumanQuestionOutcome::Cancelled,
                    };
                    return Ok((outcome, canonical));
                }
                KeyCode::Char(typed) if !ctrl => {
                    if let Some(answer) = view.answer_for_key(typed) {
                        return Ok((HumanQuestionOutcome::Answer(answer), canonical));
                    }
                }
                _ => {}
            }
        }
    }
}

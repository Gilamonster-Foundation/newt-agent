//! #1670 (meta-scroller Layer 2): the RichTUI transcript pager.
//!
//! A full-screen, on-demand view of the whole retained conversation where the
//! **conversation spine** — the operator's `›` prompts and the model's `▸`
//! replies — is the primary structure: jump message-to-message, scroll freely,
//! and fold/unfold the grey per-turn tool blocks between the green messages.
//! The top grows over the entire stored transcript, not a fixed viewport.
//!
//! Charter: alt-screen surfaces are permitted on the feature-gated RICH tier
//! only (`docs/decisions/plain_scroller_tui.md`, 2026-08-11 amendment). The
//! lean surface answers `/transcript` with a plain printed spine instead
//! (`conversation_show_message` — no scroll regions, ever).
//!
//! Grey depth note: the store deliberately retains tool **summaries** only
//! (`ToolEvent` — name/ok/duration, never raw output; the privacy stance in
//! `conversation.rs`). So a fold's body is the turn's tool-call summary lines.
//! Retaining bounded tool-output bodies for the pager is the tracked follow-up
//! on #1670 — it needs its own session-retention design, not a store change.
//!
//! The row model is PURE (no TTY, no ratatui types) so navigation, folding,
//! and jump targets are unit-tested; only `run_pager` touches the terminal.

use newt_core::ConversationTurn;

/// What a flattened pager row is, for styling and for jump targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    /// First line of a turn's operator prompt — the `›` anchor jumps land on.
    PromptHead,
    /// Continuation line of a prompt.
    Prompt,
    /// First line of the model reply (`▸`).
    ReplyHead,
    /// Continuation line of a reply.
    Reply,
    /// The grey fold header: `⚙ N tool calls …` — Enter/Space toggles.
    ToolFold,
    /// One tool-call summary line inside an unfolded block.
    Tool,
    /// Blank separator between turns.
    Blank,
}

/// One flattened, styled-by-kind pager row. Long lines are CLIPPED by the
/// renderer, not wrapped: the pager is a navigation surface, and exact
/// row-per-line math is what keeps scrolling and jump targets testable.
///
/// Where the full text lives, stated accurately (#1677 review): for turns THIS
/// terminal printed, it is in normal scrollback. For a **resumed** conversation
/// it is not — those turns were never printed here — so the fallback for full
/// text is `/conversation show <id>`, or `/transcript` on the LEAN surface,
/// which prints the spine unclipped. The pager offers no wrap toggle and no
/// horizontal scroll; a wrap mode is follow-up work on #1672. Saying
/// "scrollback always has it" would be false for exactly the case this command
/// exists to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PagerRow {
    pub kind: RowKind,
    /// Which turn this row belongs to (separator rows belong to the turn
    /// ABOVE them).
    pub turn: usize,
    pub text: String,
}

/// The pure pager state: the record's turns flattened against per-turn fold
/// flags, plus one scroll position. The "current" turn is DERIVED from the
/// scroll (the last prompt head at or above the top row), so there is no
/// second cursor to drift.
pub(crate) struct PagerState {
    turns: Vec<TurnRows>,
    folded: Vec<bool>,
    /// Top visible flattened-row index.
    pub scroll: usize,
    pub title: String,
}

/// One turn's pre-split lines (fold state applied at flatten time).
struct TurnRows {
    prompt: Vec<String>,
    reply: Vec<String>,
    tools: Vec<String>,
}

/// `⚙ name · ✓/✗ · 12ms` — the same vocabulary as the inline tool header, so
/// the fold body reads as the familiar grey.
fn tool_line(event: &newt_core::ToolEvent) -> String {
    let ok = if event.ok { "✓" } else { "✗" };
    match event.duration_ms {
        Some(ms) => format!("⚙ {} · {ok} · {ms}ms", event.tool),
        None => format!("⚙ {} · {ok}", event.tool),
    }
}

fn split_lines(text: &str) -> Vec<String> {
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

impl PagerState {
    /// Build from a stored conversation's title + turns (the only record
    /// fields the pager reads — taking them directly keeps this model
    /// decoupled from `ConversationRecord`'s width). Tool blocks start
    /// FOLDED — the spine is the point; the grey is one keypress away.
    pub(crate) fn new(title: &str, turns: &[ConversationTurn]) -> Self {
        let turns: Vec<TurnRows> = turns.iter().map(TurnRows::from_turn).collect();
        let folded = vec![true; turns.len()];
        Self {
            turns,
            folded,
            scroll: 0,
            title: title.to_string(),
        }
    }

    /// Flatten turns + fold flags into styled rows. O(total lines) — fine for
    /// a keypress-driven redraw of a stored conversation.
    pub(crate) fn rows(&self) -> Vec<PagerRow> {
        let mut out = Vec::new();
        for (i, turn) in self.turns.iter().enumerate() {
            if i > 0 {
                out.push(PagerRow {
                    kind: RowKind::Blank,
                    turn: i - 1,
                    text: String::new(),
                });
            }
            for (n, line) in turn.prompt.iter().enumerate() {
                let (kind, sigil) = if n == 0 {
                    (RowKind::PromptHead, "› ")
                } else {
                    (RowKind::Prompt, "  ")
                };
                out.push(PagerRow {
                    kind,
                    turn: i,
                    text: format!("{sigil}{line}"),
                });
            }
            if !turn.tools.is_empty() {
                let state = if self.folded[i] { "▸" } else { "▾" };
                out.push(PagerRow {
                    kind: RowKind::ToolFold,
                    turn: i,
                    // Honest about depth (#1677): folded invites the
                    // keypress; UNFOLDED says what the reader is actually
                    // getting. The store keeps tool SUMMARIES only —
                    // `ToolEvent` has no output field — so a header that
                    // said just "N tool calls" would imply bodies that were
                    // never retained. Bounded output retention is the
                    // tracked #1672 follow-up.
                    text: format!(
                        "{state} ⚙ {} tool call{}{}",
                        turn.tools.len(),
                        if turn.tools.len() == 1 { "" } else { "s" },
                        if self.folded[i] {
                            " — Enter unfolds"
                        } else {
                            " — summaries only; tool output is not retained"
                        }
                    ),
                });
                if !self.folded[i] {
                    for line in &turn.tools {
                        out.push(PagerRow {
                            kind: RowKind::Tool,
                            turn: i,
                            text: format!("  {line}"),
                        });
                    }
                }
            }
            for (n, line) in turn.reply.iter().enumerate() {
                let (kind, sigil) = if n == 0 {
                    (RowKind::ReplyHead, "▸ ")
                } else {
                    (RowKind::Reply, "  ")
                };
                out.push(PagerRow {
                    kind,
                    turn: i,
                    text: format!("{sigil}{line}"),
                });
            }
        }
        out
    }

    fn max_scroll(&self, page_rows: usize) -> usize {
        self.rows().len().saturating_sub(page_rows.max(1))
    }

    pub(crate) fn scroll_by(&mut self, delta: isize, page_rows: usize) {
        let max = self.max_scroll(page_rows);
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    /// Re-clamp the scroll position to the end rail for a `page_rows`-tall
    /// viewport (#1677). The renderer calls this every frame, so a RESIZE —
    /// the one viewport change the keyboard cannot produce — can never leave
    /// the view stranded past the rail that `end()`/`scroll_by()` enforce.
    pub(crate) fn clamp_scroll(&mut self, page_rows: usize) {
        self.scroll = self.scroll.min(self.max_scroll(page_rows));
    }

    pub(crate) fn home(&mut self) {
        self.scroll = 0;
    }

    pub(crate) fn end(&mut self, page_rows: usize) {
        self.scroll = self.max_scroll(page_rows);
    }

    /// The turn whose prompt head is nearest AT or ABOVE the top visible row —
    /// what jumps and folds operate on. Turn 0 before any head is visible.
    pub(crate) fn current_turn(&self) -> usize {
        let rows = self.rows();
        let mut current = 0;
        for row in rows.iter().take(self.scroll + 1) {
            if row.kind == RowKind::PromptHead {
                current = row.turn;
            }
        }
        current
    }

    /// Jump so the NEXT turn's prompt head is the top row (no-op on the last).
    pub(crate) fn next_message(&mut self, page_rows: usize) {
        let target = self.current_turn() + 1;
        if let Some(row) = self.prompt_head_row(target) {
            self.scroll = row.min(self.max_scroll(page_rows));
        }
    }

    /// Jump to THIS turn's prompt head if we've scrolled past it, else the
    /// previous turn's — the pager twin of "go back to what I asked".
    pub(crate) fn prev_message(&mut self) {
        let current = self.current_turn();
        let head = self.prompt_head_row(current).unwrap_or(0);
        let target = if self.scroll > head {
            head
        } else {
            self.prompt_head_row(current.saturating_sub(1)).unwrap_or(0)
        };
        self.scroll = target;
    }

    fn prompt_head_row(&self, turn: usize) -> Option<usize> {
        self.rows()
            .iter()
            .position(|row| row.kind == RowKind::PromptHead && row.turn == turn)
    }

    /// Toggle the current turn's grey block. Folding above the viewport moves
    /// rows; keep the current prompt head pinned so the spine doesn't jump.
    pub(crate) fn toggle_fold(&mut self, page_rows: usize) {
        let turn = self.current_turn();
        if self.turns.get(turn).map(|t| t.tools.is_empty()) != Some(false) {
            return;
        }
        let head_before = self.prompt_head_row(turn).unwrap_or(0);
        let offset = self.scroll.saturating_sub(head_before);
        self.folded[turn] = !self.folded[turn];
        let head_after = self.prompt_head_row(turn).unwrap_or(0);
        self.scroll = (head_after + offset).min(self.max_scroll(page_rows));
    }

    /// `turn N/M` for the header bar.
    pub(crate) fn position(&self) -> String {
        format!(
            "turn {}/{}",
            self.current_turn() + 1,
            self.turns.len().max(1)
        )
    }
}

impl TurnRows {
    fn from_turn(turn: &ConversationTurn) -> Self {
        Self {
            prompt: split_lines(&turn.user),
            reply: split_lines(&turn.assistant),
            tools: turn.events.iter().map(tool_line).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// The modal terminal loop (the only part that touches the TTY).
//
// COMPILE-GATED to `rich-tui` (#1677 review). Everything above this line is the
// pure transcript VIEW MODEL — no terminal, no ratatui — and stays compiled and
// unit-tested in every configuration, including the lean/no-rich build. Only
// this half, which takes the terminal over (raw mode + alternate screen), is
// severed from lean. `ratatui`/`crossterm` are non-optional deps of this crate,
// so without this gate a lean binary would carry an alt-screen surface it can
// never legitimately enter — `plain_scroller_tui.md`: the lean path has no
// scroll regions, ever.
// ---------------------------------------------------------------------------
#[cfg(all(feature = "rich-tui", feature = "live-spill"))]
pub(crate) use terminal::run_output_pager;
#[cfg(feature = "rich-tui")]
pub(crate) use terminal::run_pager;
/// Re-exported for the real-PTY acceptance test only (#1677): it drops the
/// guard mid-unwind in a child process to prove the restoration claim against
/// an actual terminal. Nothing in the shipping path constructs it directly —
/// `run_pager` owns its lifetime.
// unix as well as test: the sole consumer is `transcript_pager_pty_test`,
// which is itself `cfg(all(test, unix, feature = "rich-tui"))` because a pty
// pair is unix-only. Without the `unix` here the re-export is an unused
// import on Windows, and this repo runs `-D warnings`.
#[cfg(all(test, unix, feature = "rich-tui"))]
pub(crate) use terminal::AltScreenGuard;

#[cfg(feature = "rich-tui")]
mod terminal {
    use super::{PagerState, RowKind};

    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
    use newt_core::tty::raw_mode::RawModeGuard;
    use ratatui::backend::CrosstermBackend;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    use std::io;

    #[cfg(feature = "live-spill")]
    use crate::completed_spill::CompletedSpill;

    /// Restore the primary screen + cooked mode on EVERY exit path — error or
    /// panic included. Leaking the alternate screen would strand the whole
    /// session invisible, a worse failure than any pager bug.
    ///
    /// `pub(crate)` solely for `transcript_pager_pty_test` (#1677): the real-PTY
    /// error-path test drops this guard mid-unwind in a child process to prove
    /// the restoration claim above against an actual terminal.
    ///
    /// **Composed onto [`RawModeGuard`] (#1905).** The alternate screen is
    /// this type's own; raw mode is the shared, nesting-aware owner held as a
    /// field. Rust drops fields AFTER the `Drop::drop` body, so the screen is
    /// left before raw mode is released — the order this guard already had.
    pub(crate) struct AltScreenGuard {
        /// **The whole screen, held (#1980).** Entering the alternate screen IS
        /// taking every row, and until now the arbiter did not know: an inline
        /// surface could mint the bottom rows while the pager owned the screen.
        ///
        /// DECLARATION ORDER IS THE CONTRACT, as for `_raw`. Fields drop AFTER
        /// `Drop::drop`, so the screen is left first and only then are the rows
        /// returned — a lease released while the pager was still drawing would
        /// advertise rows it had not finished with.
        _region: newt_core::tty::RegionLease,
        _raw: RawModeGuard,
    }

    impl AltScreenGuard {
        pub(crate) fn enter() -> io::Result<Self> {
            let raw = RawModeGuard::enter()?;
            // `SuspendHolder`, and it is the honest policy rather than the
            // convenient one: the alternate screen genuinely DOES suspend
            // whatever is beneath it — the primary screen is preserved and
            // restored by the terminal itself. It therefore never fails, so
            // this adds no new refusal path and changes no behaviour here.
            // What changes is elsewhere: a surface minting `Refuse` or `Shift`
            // now sees that the screen is taken.
            let region = newt_core::tty::Terminal::lease_region(
                newt_core::tty::Region::WholeScreen,
                newt_core::tty::OnCollision::SuspendHolder,
            )
            .ok_or_else(|| io::Error::other("the screen could not be leased"))?;
            // Bind the guard BEFORE the fallible `execute!`, so Drop owns the
            // restore from this point on — the same shape `SplashScreenGuard`
            // uses (lib.rs), whose doc records that the hand-rolled rollback
            // this replaces "was itself one of the three leaks" (#1411).
            //
            // The asymmetry mattered: `execute!` queues into `io::stdout()`'s
            // LineWriter and then flushes, and a FAILED flush RETAINS the
            // unwritten `?1049h` in that process-global buffer. The old
            // rollback could give raw mode back but could never emit
            // `?1049l` — so the next print (the caller's own "transcript
            // pager error: …" line) flushed the retained bytes and put the
            // session on the alternate screen with no owner alive to leave
            // it, for the rest of its life. Reviewer-reproduced against real
            // crossterm on a would-block fd (#1677 review).
            let guard = Self {
                _region: region,
                _raw: raw,
            };
            crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
            Ok(guard)
        }
    }

    impl Drop for AltScreenGuard {
        fn drop(&mut self) {
            // Screen only; raw mode is released by `_raw` after this returns.
            let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
        }
    }

    fn row_style(kind: RowKind) -> Style {
        match kind {
            // The spine is the primary content — default foreground, heads bold.
            RowKind::PromptHead | RowKind::ReplyHead => {
                Style::default().add_modifier(Modifier::BOLD)
            }
            RowKind::Prompt | RowKind::Reply => Style::default(),
            // The grey is secondary — exactly the inline vocabulary.
            RowKind::ToolFold | RowKind::Tool => Style::default().fg(Color::DarkGray),
            RowKind::Blank => Style::default(),
        }
    }

    /// Run the pager until the operator quits (q / Esc). Blocking; owns the
    /// terminal for its lifetime and restores it on return.
    pub(crate) fn run_pager(state: &mut PagerState) -> io::Result<()> {
        let _guard = AltScreenGuard::enter()?;
        let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;
        loop {
            let mut page_rows = 1usize;
            terminal.draw(|f| {
            let title = if state.title.is_empty() {
                "transcript"
            } else {
                state.title.as_str()
            };
            let body = crate::modal::frame(
                f,
                f.area(),
                &crate::modal::Chrome {
                    title,
                    subtitle: Some(format!("· {}", state.position())),
                    hint: Some(
                        "q quit · ↑↓ scroll · PgUp/PgDn page · n/p message · Enter fold · g/G top/bottom",
                    ),
                },
            );
            page_rows = body.height.max(1) as usize;

            // #1677: re-clamp to the SAME end rail the keyboard uses. The
            // renderer's own clamp used to be `len - 1`, one rail past the
            // model's `len - page_rows`, so GROWING the terminal could strand
            // the view in a position no keystroke can produce.
            state.clamp_scroll(page_rows);
            let rows = state.rows();
            let visible = rows
                .iter()
                .skip(state.scroll)
                .take(page_rows)
                .map(|row| Line::from(Span::styled(row.text.clone(), row_style(row.kind))))
                .collect::<Vec<_>>();
            f.render_widget(Paragraph::new(visible), body);
        })?;

            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let half = (page_rows / 2).max(1) as isize;
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if ctrl => return Ok(()),
                KeyCode::Up | KeyCode::Char('k') => state.scroll_by(-1, page_rows),
                KeyCode::Down | KeyCode::Char('j') => state.scroll_by(1, page_rows),
                KeyCode::PageUp => state.scroll_by(-(page_rows as isize), page_rows),
                KeyCode::PageDown => state.scroll_by(page_rows as isize, page_rows),
                KeyCode::Char('u') if ctrl => state.scroll_by(-half, page_rows),
                KeyCode::Char('d') if ctrl => state.scroll_by(half, page_rows),
                KeyCode::Char('g') | KeyCode::Home => state.home(),
                KeyCode::Char('G') | KeyCode::End => state.end(page_rows),
                KeyCode::Char('n') => state.next_message(page_rows),
                KeyCode::Char('p') => state.prev_message(),
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Tab => state.toggle_fold(page_rows),
                _ => {}
            }
        }
    }

    /// Open one retained completed-tool body. This deliberately shares the
    /// transcript pager's alternate-screen guard and navigation vocabulary,
    /// but has no folds: every retained line is immediately visible.
    #[cfg(feature = "live-spill")]
    pub(crate) fn run_output_pager(spill: &CompletedSpill) -> io::Result<()> {
        let _guard = AltScreenGuard::enter()?;
        let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;
        let mut scroll = 0usize;
        loop {
            let mut page_rows = 1usize;
            terminal.draw(|f| {
                let retention = if spill.dropped_lines() == 0 {
                    format!("{} lines", spill.total_lines())
                } else {
                    format!(
                        "{} of {} lines retained (oldest dropped)",
                        spill.lines().len(),
                        spill.total_lines()
                    )
                };
                let body = crate::modal::frame(
                    f,
                    f.area(),
                    &crate::modal::Chrome {
                        title: &format!("spill {}", spill.id()),
                        subtitle: Some(format!("· {retention}")),
                        hint: Some("q quit · ↑↓ scroll · PgUp/PgDn page · g/G top/bottom"),
                    },
                );
                page_rows = body.height.max(1) as usize;
                let max_scroll = spill.lines().len().saturating_sub(page_rows);
                scroll = scroll.min(max_scroll);

                let visible = spill
                    .lines()
                    .iter()
                    .skip(scroll)
                    .take(page_rows)
                    .map(|line| {
                        Line::from(Span::styled(
                            line.clone(),
                            Style::default().fg(crate::theme::color(crate::theme::Role::Dim)),
                        ))
                    })
                    .collect::<Vec<_>>();
                f.render_widget(Paragraph::new(visible), body);
            })?;

            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let half = (page_rows / 2).max(1);
            let max_scroll = spill.lines().len().saturating_sub(page_rows);
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if ctrl => return Ok(()),
                KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => scroll = (scroll + 1).min(max_scroll),
                KeyCode::PageUp => scroll = scroll.saturating_sub(page_rows),
                KeyCode::PageDown => scroll = (scroll + page_rows).min(max_scroll),
                KeyCode::Char('u') if ctrl => scroll = scroll.saturating_sub(half),
                KeyCode::Char('d') if ctrl => scroll = (scroll + half).min(max_scroll),
                KeyCode::Char('g') | KeyCode::Home => scroll = 0,
                KeyCode::Char('G') | KeyCode::End => scroll = max_scroll,
                _ => {}
            }
        }
    }
} // mod terminal (rich-tui only)

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(user: &str, assistant: &str, tools: usize) -> ConversationTurn {
        ConversationTurn {
            user: user.into(),
            assistant: assistant.into(),
            events: (0..tools)
                .map(|n| {
                    newt_core::ToolEvent::from_call(
                        format!("tool{n}"),
                        &serde_json::json!({"path": "x"}),
                        true,
                        Some(12),
                    )
                })
                .collect(),
            phantom_reaches: Vec::new(),
            tokens_in: None,
            tokens_out: None,
        }
    }

    /// The flattened shape: spine rows carry the `›`/`▸` sigils, tool blocks
    /// start FOLDED behind a header, and turns are blank-separated.
    #[test]
    fn rows_flatten_the_spine_with_folded_grey() {
        let state = PagerState::new(
            "mesh docking",
            &[
                turn("first ask\nsecond line", "the answer", 2),
                turn("follow-up", "done", 0),
            ],
        );
        let rows = state.rows();
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "› first ask",
                "  second line",
                "▸ ⚙ 2 tool calls — Enter unfolds",
                "▸ the answer",
                "",
                "› follow-up",
                "▸ done",
            ]
        );
        assert_eq!(rows[0].kind, RowKind::PromptHead);
        assert_eq!(rows[2].kind, RowKind::ToolFold);
        // Folded: no Tool rows at all.
        assert!(rows.iter().all(|r| r.kind != RowKind::Tool));
    }

    /// Unfolding reveals the summary lines in the inline grey vocabulary
    /// (⚙ name · ✓ · ms) and refolding hides them again.
    #[test]
    fn toggle_fold_reveals_and_hides_tool_summaries() {
        let mut state = PagerState::new("mesh docking", &[turn("ask", "answer", 2)]);
        state.toggle_fold(10);
        let rows = state.rows();
        assert!(rows
            .iter()
            .any(|r| r.kind == RowKind::Tool && r.text.contains("⚙ tool0 · ✓ · 12ms")));
        assert!(rows.iter().any(|r| r.text.contains("▾")));
        state.toggle_fold(10);
        assert!(state.rows().iter().all(|r| r.kind != RowKind::Tool));
    }

    /// A turn with no tools has no fold row, and toggling is a no-op.
    #[test]
    fn no_tools_means_no_fold_row() {
        let mut state = PagerState::new("mesh docking", &[turn("ask", "answer", 0)]);
        assert!(state.rows().iter().all(|r| r.kind != RowKind::ToolFold));
        let before = state.rows();
        state.toggle_fold(10);
        assert_eq!(state.rows(), before);
    }

    /// n jumps land each successive prompt head on the top row; p walks back —
    /// first to the current head, then to the previous turn's.
    #[test]
    fn message_jumps_walk_the_spine() {
        let mut state = PagerState::new(
            "mesh docking",
            &[
                turn("one", "a", 1),
                turn("two", "b", 0),
                turn("three", "c", 0),
            ],
        );
        // A small page so every head is reachable as the top row (jumps clamp
        // to max_scroll, the end-of-content rail — see scroll_clamps test).
        let page = 2;
        assert_eq!(state.current_turn(), 0);
        state.next_message(page);
        assert_eq!(state.rows()[state.scroll].text, "› two");
        assert_eq!(state.current_turn(), 1);
        state.next_message(page);
        assert_eq!(state.rows()[state.scroll].text, "› three");
        // Exactly at a head, p walks to the PREVIOUS turn…
        state.prev_message();
        assert_eq!(state.rows()[state.scroll].text, "› two");
        // …and after scrolling past a head, p first RETURNS to it.
        state.scroll_by(1, page);
        state.prev_message();
        assert_eq!(state.rows()[state.scroll].text, "› two");
    }

    /// Scroll clamps to [0, rows - page] and home/end hit the rails.
    #[test]
    fn scroll_clamps_to_content() {
        let mut state =
            PagerState::new("mesh docking", &[turn("one", "a", 0), turn("two", "b", 0)]);
        let page = 3;
        state.scroll_by(-10, page);
        assert_eq!(state.scroll, 0);
        state.scroll_by(100, page);
        assert_eq!(state.scroll, state.rows().len() - page);
        state.home();
        assert_eq!(state.scroll, 0);
        state.end(page);
        assert_eq!(state.scroll, state.rows().len() - page);
    }

    /// Folding a block ABOVE the viewport must not teleport the spine: the
    /// current prompt head keeps its on-screen offset.
    #[test]
    fn toggle_fold_keeps_the_current_head_pinned() {
        let mut state = PagerState::new("mesh docking", &[turn("ask", "answer", 3)]);
        let page = 4;
        state.toggle_fold(page); // unfold: 3 extra rows appear
        let head = 0;
        state.scroll = head; // head at top
        state.toggle_fold(page); // refold from the head — offset 0 preserved
        assert_eq!(state.rows()[state.scroll].text, "› ask");
    }

    /// The header position tracks the derived current turn.
    #[test]
    fn position_names_the_current_turn() {
        let mut state =
            PagerState::new("mesh docking", &[turn("one", "a", 0), turn("two", "b", 0)]);
        assert_eq!(state.position(), "turn 1/2");
        state.next_message(2);
        assert_eq!(state.position(), "turn 2/2");
    }

    /// Empty prompts/replies still produce a head row (the spine never skips
    /// a turn), and an empty record renders no rows without panicking.
    #[test]
    fn empty_edges_are_safe() {
        let state = PagerState::new("mesh docking", &[turn("", "", 0)]);
        let rows = state.rows();
        assert_eq!(rows[0].text, "› ");
        assert_eq!(rows[1].text, "▸ ");

        let mut empty = PagerState::new("mesh docking", &[]);
        assert!(empty.rows().is_empty());
        empty.scroll_by(5, 3);
        assert_eq!(empty.scroll, 0);
        assert_eq!(empty.position(), "turn 1/1");
        empty.next_message(3);
        empty.prev_message();
        empty.toggle_fold(3);
    }
    // ── #1677 state-model stress ────────────────────────────────────────
    //
    // The pure model is where scroll/fold/jump correctness is decided, so the
    // awkward shapes get pinned HERE rather than through a terminal: resize
    // (the one viewport change no keystroke can produce), fold churn changing
    // the rail underfoot, the extremes, and text the renderer will clip.

    #[test]
    fn a_resize_smaller_then_larger_never_strands_the_view() {
        // Regression for the renderer/model rail mismatch (#1677): the draw
        // path clamped to `len - 1` while every model clamp uses
        // `len - page_rows`, so GROWING the terminal left the scroll beyond
        // the end rail — a position the keyboard cannot reach, and one that
        // renders a short final page with content scrolled off the top.
        let turns: Vec<_> = (0..12)
            .map(|i| turn(&format!("p{i}"), "reply", 0))
            .collect();
        let mut s = PagerState::new("t", &turns);
        let total = s.rows().len();

        // Small viewport, parked at the end rail.
        s.end(4);
        assert_eq!(s.scroll, total - 4);

        // Grow the terminal: the rail moves UP, so the position must follow.
        s.clamp_scroll(20);
        assert_eq!(
            s.scroll,
            total.saturating_sub(20),
            "growing the viewport must re-clamp to the new end rail"
        );

        // Shrink again: the rail moves down, and clamping must not *push* the
        // view (clamp is a ceiling, never a jump).
        let before = s.scroll;
        s.clamp_scroll(4);
        assert_eq!(s.scroll, before, "shrinking must not move a valid position");

        // A viewport taller than the content pins to the top.
        s.clamp_scroll(total + 50);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn a_viewport_of_one_row_is_survivable() {
        // The degenerate terminal (a 1-row split) must not divide by zero or
        // strand: `max_scroll` floors page_rows at 1.
        let turns: Vec<_> = (0..3).map(|i| turn(&format!("p{i}"), "r", 0)).collect();
        let mut s = PagerState::new("t", &turns);
        let total = s.rows().len();
        s.end(1);
        assert_eq!(s.scroll, total - 1);
        s.scroll_by(50, 1);
        assert_eq!(s.scroll, total - 1, "still clamped at the last row");
        s.clamp_scroll(0); // a zero-height body is treated as one row
        assert!(s.scroll < total);
    }

    #[test]
    fn page_and_half_page_moves_walk_and_clamp() {
        let turns: Vec<_> = (0..30).map(|i| turn(&format!("p{i}"), "r", 0)).collect();
        let mut s = PagerState::new("t", &turns);
        let page = 10usize;
        let max = s.rows().len() - page;

        s.scroll_by(page as isize, page); // PageDown
        assert_eq!(s.scroll, page);
        s.scroll_by(-(page as isize), page); // PageUp
        assert_eq!(s.scroll, 0);
        s.scroll_by(-(page as isize), page); // PageUp at the top rail
        assert_eq!(s.scroll, 0, "saturates at the top, never wraps");
        s.scroll_by((page / 2) as isize, page); // Ctrl-D
        assert_eq!(s.scroll, page / 2);
        for _ in 0..50 {
            s.scroll_by(page as isize, page);
        }
        assert_eq!(s.scroll, max, "clamps at the end rail, never past it");
    }

    #[test]
    fn home_and_end_are_the_rails() {
        let turns: Vec<_> = (0..8).map(|i| turn(&format!("p{i}"), "r", 0)).collect();
        let mut s = PagerState::new("t", &turns);
        let page = 5usize;
        s.end(page);
        assert_eq!(s.scroll, s.rows().len() - page);
        s.home();
        assert_eq!(s.scroll, 0);
        // End on content SHORTER than the viewport stays at the top rather
        // than producing a negative rail.
        let short = vec![turn("only", "one", 0)];
        let mut s2 = PagerState::new("t", &short);
        s2.end(100);
        assert_eq!(s2.scroll, 0);
    }

    #[test]
    fn unfolding_moves_the_rail_and_scroll_stays_valid() {
        // Fold churn changes `rows().len()` underfoot, so every clamp must be
        // recomputed against the CURRENT flattening rather than a cached
        // length. Toggling is done on the turn the model says is current
        // (turn 0 at home) instead of walking to an arbitrary turn — see
        // `the_end_rail_bounds_how_far_the_spine_jumps_can_reach` for why a
        // walk to the LAST turn is not a thing the model promises.
        let turns: Vec<_> = (0..6).map(|i| turn(&format!("p{i}"), "r", 3)).collect();
        let mut s = PagerState::new("t", &turns);
        let page = 6usize;

        s.home();
        assert_eq!(s.current_turn(), 0);
        let folded_rows = s.rows().len();
        let folded_rail = {
            s.end(page);
            s.scroll
        };

        // Unfold turn 0: content grows by exactly its tool lines, so the end
        // rail moves DOWN by the same amount.
        s.home();
        s.toggle_fold(page);
        assert_eq!(
            s.rows().len(),
            folded_rows + 3,
            "unfolding adds exactly the turn's tool lines"
        );
        s.end(page);
        assert_eq!(
            s.scroll,
            folded_rail + 3,
            "the end rail tracks the new content length"
        );

        // Park at the rail, then RE-fold: the rail moves back up and the
        // parked position must be re-clamped rather than left past the end.
        s.home();
        s.toggle_fold(page);
        s.scroll = folded_rail + 3; // where the operator was before the fold
        s.clamp_scroll(page);
        assert_eq!(
            s.scroll, folded_rail,
            "re-folding pulls a stranded position back to the rail"
        );
    }

    #[test]
    fn the_end_rail_bounds_how_far_the_spine_jumps_can_reach() {
        // A PINNED LIMITATION, deliberately recorded rather than left as
        // folklore (#1677). `current_turn` is derived as "the last prompt head
        // at or above the top row", and `next_message` clamps its target to
        // the end rail. So when the final turn's head falls INSIDE the last
        // page, `n` cannot make that turn current: scroll parks at
        // `max_scroll`, the head sits below the top row, and the derivation
        // keeps naming the previous turn. The final turn is fully VISIBLE at
        // that point — this is a naming/targeting limit, not a scrolling one —
        // but two consequences follow that a reader should know about: the
        // position header under-reports at the bottom, and a fold keystroke
        // there targets the turn at the TOP of the view.
        //
        // Widening the derivation to "the last head visible in the viewport"
        // would fix both, and would also move fold targeting — which is a
        // behavior change, not a landing fix. Tracked as follow-up on #1672.
        let turns: Vec<_> = (0..4).map(|i| turn(&format!("p{i}"), "short", 0)).collect();
        let mut s = PagerState::new("t", &turns);
        let page = 8usize; // tall enough that the last heads sit inside it
        let rows = s.rows();
        let last_head = rows
            .iter()
            .rposition(|r| r.kind == RowKind::PromptHead)
            .expect("a head exists");
        assert!(
            last_head > s.max_scroll(page),
            "precondition: the final head lies beyond the end rail"
        );

        // Pressing `n` repeatedly converges and then STOPS, without spinning.
        s.home();
        let mut seen = Vec::new();
        for _ in 0..10 {
            s.next_message(page);
            seen.push(s.current_turn());
        }
        let reached = *seen.last().expect("jumped");
        assert!(
            reached < turns.len() - 1,
            "the final turn is not reachable as `current` from the end rail"
        );
        assert!(
            seen.windows(2).all(|w| w[1] >= w[0]),
            "jumps never move backwards: {seen:?}"
        );
        assert_eq!(
            s.scroll,
            s.max_scroll(page),
            "the view IS parked at the bottom — the last turn is on screen"
        );
    }

    #[test]
    fn the_current_turn_follows_the_scroll() {
        let turns: Vec<_> = (0..5)
            .map(|i| turn(&format!("prompt {i}"), "reply\nline\nline", 0))
            .collect();
        let mut s = PagerState::new("t", &turns);
        assert_eq!(s.current_turn(), 0);
        let rows = s.rows();
        // Park the top row exactly on the last turn's prompt head.
        let last_head = rows
            .iter()
            .rposition(|r| r.kind == RowKind::PromptHead)
            .expect("a prompt head exists");
        s.scroll = last_head;
        assert_eq!(
            s.current_turn(),
            4,
            "current is the last head at or above the top row"
        );
        s.home();
        assert_eq!(s.current_turn(), 0, "derived, not remembered");
    }

    #[test]
    fn wide_and_unicode_text_survives_the_model_intact() {
        // The model does not wrap or clip — the renderer does. What it must
        // guarantee is that the bytes it hands over are the stored bytes, so
        // a CJK/emoji transcript is never corrupted on the way to the screen.
        let cjk = "日本語のテキストです";
        let emoji = "🦎 newt 👀 spine";
        let turns = vec![turn(cjk, emoji, 0)];
        let s = PagerState::new("t", &turns);
        let rows = s.rows();
        assert!(
            rows.iter().any(|r| r.text.contains(cjk)),
            "wide text preserved: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.text.contains(emoji)),
            "emoji preserved: {rows:?}"
        );
        // Row count is per LINE, not per display column — one line in, one
        // row out, regardless of how many columns it will occupy.
        assert_eq!(
            rows.iter()
                .filter(|r| r.kind == RowKind::PromptHead)
                .count(),
            1
        );
    }

    #[test]
    fn a_very_long_line_is_one_row_and_is_not_truncated_by_the_model() {
        let long = "x".repeat(20_000);
        let turns = vec![turn(&long, "r", 0)];
        let s = PagerState::new("t", &turns);
        let rows = s.rows();
        let head = rows
            .iter()
            .find(|r| r.kind == RowKind::PromptHead)
            .expect("head");
        assert!(head.text.len() >= 20_000, "the model never truncates");
        assert_eq!(
            rows.iter()
                .filter(|r| r.kind == RowKind::PromptHead)
                .count(),
            1,
            "one logical line is exactly one row (clipping is the renderer's job)"
        );
    }

    #[test]
    fn one_turn_and_many_turns_flatten_consistently() {
        let one = PagerState::new("t", &[turn("p", "r", 0)]);
        assert_eq!(
            one.rows()
                .iter()
                .filter(|r| r.kind == RowKind::Blank)
                .count(),
            0,
            "no separator before the first turn"
        );
        let many: Vec<_> = (0..25).map(|i| turn(&format!("p{i}"), "r", 0)).collect();
        let s = PagerState::new("t", &many);
        assert_eq!(
            s.rows().iter().filter(|r| r.kind == RowKind::Blank).count(),
            24,
            "exactly one separator BETWEEN each pair of turns"
        );
        assert_eq!(
            s.rows()
                .iter()
                .filter(|r| r.kind == RowKind::PromptHead)
                .count(),
            25
        );
    }

    #[test]
    fn a_ten_thousand_row_transcript_stays_linear() {
        // Long-transcript sanity (#1677). Deliberately asserts SHAPE, not
        // wall-clock: timing has no place in the mocked unit tier and would be
        // flaky under parallel CI load. What this pins is what a quadratic
        // regression would break — flattening is exactly one row per logical
        // line, the rails are computed from the current flattening, and a fold
        // adds exactly its own tool lines with no compounding. The measured
        // interactive timing is recorded in the PR body instead.
        // 2_000 turns x 6 rows each (prompt + 3 reply lines + fold header +
        // separator) clears the 10k bar the sanity check is specified at.
        let turns: Vec<_> = (0..2_000)
            .map(|i| turn(&format!("prompt {i}"), "reply\nsecond\nthird", 2))
            .collect();
        let mut s = PagerState::new("long", &turns);
        let rows = s.rows().len();
        assert!(rows > 10_000, "expected a 10k+ row transcript, got {rows}");

        let page = 40usize;
        s.end(page);
        assert_eq!(s.scroll, rows - page, "the end rail is exact at this size");
        s.home();
        assert_eq!(s.scroll, 0);

        // A bounded walk down the spine: each jump moves forward, and none of
        // them costs more than the flattening itself.
        let mut last = s.current_turn();
        for _ in 0..200 {
            s.next_message(page);
            let now = s.current_turn();
            assert!(now >= last, "jumps are monotonic on a long transcript");
            last = now;
        }
        assert!(last > 100, "200 jumps made real progress, reached {last}");

        // Folding at this size adds exactly the folded turn's tool lines.
        s.home();
        let before = s.rows().len();
        s.toggle_fold(page);
        assert_eq!(
            s.rows().len(),
            before + 2,
            "one unfold adds exactly its two tool lines, even at 10k rows"
        );
    }
}

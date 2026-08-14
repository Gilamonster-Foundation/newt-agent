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
/// renderer, not wrapped: the pager is a navigation surface and exact
/// row-per-line math keeps scrolling and jumps testable; the full text always
/// remains in normal scrollback (searchable, copy-pasteable — the charter's
/// scrollback guarantees are about the primary surface, which this never
/// replaces).
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
                    text: format!(
                        "{state} ⚙ {} tool call{} — Enter unfolds",
                        turn.tools.len(),
                        if turn.tools.len() == 1 { "" } else { "s" }
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
// ---------------------------------------------------------------------------

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::io;

/// Restore the primary screen + cooked mode on EVERY exit path — error or
/// panic included. Leaking the alternate screen would strand the whole
/// session invisible, a worse failure than any pager bug.
struct AltScreenGuard;

impl AltScreenGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = crossterm::execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(Self)
    }
}

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn row_style(kind: RowKind) -> Style {
    match kind {
        // The spine is the primary content — default foreground, heads bold.
        RowKind::PromptHead | RowKind::ReplyHead => Style::default().add_modifier(Modifier::BOLD),
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
            let [header, body, footer] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(1),
            ])
            .areas(f.area());
            page_rows = body.height.max(1) as usize;

            let title = if state.title.is_empty() {
                "transcript".to_string()
            } else {
                state.title.clone()
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!(" {title} "),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("· {}", state.position()), Style::default().fg(Color::Gray)),
                ])),
                header,
            );

            let rows = state.rows();
            state.scroll = state.scroll.min(rows.len().saturating_sub(1));
            let visible = rows
                .iter()
                .skip(state.scroll)
                .take(page_rows)
                .map(|row| Line::from(Span::styled(row.text.clone(), row_style(row.kind))))
                .collect::<Vec<_>>();
            f.render_widget(Paragraph::new(visible), body);

            f.render_widget(
                Paragraph::new(Span::styled(
                    " q quit · ↑↓ scroll · PgUp/PgDn page · n/p message · Enter fold · g/G top/bottom",
                    Style::default().fg(Color::DarkGray),
                )),
                footer,
            );
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
}

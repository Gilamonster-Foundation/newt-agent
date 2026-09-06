//! Command markers and display-only bang projection for the rich input editor.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tui_textarea::{CursorMove, TextArea};

/// Command rows use the same high-contrast dark slab live and in scrollback.
/// The marker color carries the command family; the body stays neutral so a
/// shell command remains easy to audit character-for-character.
pub(super) const COMMAND_BG: Color = Color::Rgb(82, 82, 82);
/// A command draft stays recognizable behind a blocking modal, but no longer
/// competes with the modal for visual focus.
pub(super) const INACTIVE_COMMAND_BG: Color = Color::Rgb(45, 45, 45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommandKind {
    Bang,
    Ex,
}

impl CommandKind {
    pub(super) fn marker(self) -> char {
        match self {
            Self::Bang => '!',
            Self::Ex => ':',
        }
    }

    pub(super) fn marker_style(self) -> Style {
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

pub(super) fn command_line(kind: CommandKind, tail: &str) -> Line<'static> {
    command_line_with_focus(kind, tail, true)
}

pub(super) fn command_line_with_focus(
    kind: CommandKind,
    tail: &str,
    focused: bool,
) -> Line<'static> {
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

pub(super) fn command_background(focused: bool) -> Color {
    if focused {
        COMMAND_BG
    } else {
        INACTIVE_COMMAND_BG
    }
}

pub(super) fn is_bang_escape(body: &str) -> bool {
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
pub(super) struct BangView<'a> {
    pub(super) textarea: TextArea<'a>,
    /// The source caret is on whitespace or `!` hidden by this projection.
    /// Render it on the visible marker instead of after that marker.
    pub(super) cursor_on_marker: bool,
}

pub(super) fn bang_view<'a>(textarea: &TextArea<'a>) -> Option<BangView<'a>> {
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
pub(super) fn cancel_hidden_bang_selection(textarea: &mut TextArea<'_>) {
    if textarea.is_selecting() && is_bang_escape(&textarea.lines().join("\n")) {
        textarea.cancel_selection();
    }
}

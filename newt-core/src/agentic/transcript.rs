//! A **reusable transcript render** for a downstream chat pane (issue #308 —
//! the cowork foundation).
//!
//! ## Why this is render-*data*, not a ratatui widget
//!
//! newt's own chat surface is a plain scroller and `docs/decisions/
//! plain_scroller_tui.md` forbids adding ratatui / widget surfaces to it —
//! ratatui is a dependency *only* for the startup splash, and panes/splits
//! belong downstream in **gilamonster-agent**, which inherits newt's
//! **published** crates. So the reusable render lives in `newt-core` as
//! **renderer-agnostic data**: [`transcript_lines`] turns a `&[MemMessage]`
//! into width-wrapped, role-tagged [`TranscriptLine`]s sized to fit an
//! arbitrary `Rect`'s width. The downstream cowork UI maps each line to a
//! `ratatui::text::Line` with its own styling and places the pane wherever its
//! layout wants — newt-core never depends on ratatui, and the plain-scroller
//! chat path is untouched.
//!
//! ```
//! use newt_core::agentic::{transcript_lines, TranscriptStyle, TranscriptRole};
//! use newt_core::MemMessage;
//!
//! let msgs = [
//!     MemMessage::user("hello"),
//!     MemMessage::assistant("hi there"),
//! ];
//! // Render into a 20-column-wide pane.
//! let lines = transcript_lines(&msgs, 20);
//! assert!(lines.iter().any(|l| l.role == TranscriptRole::User));
//! assert!(lines.iter().any(|l| l.role == TranscriptRole::Assistant));
//! // Speaker labels appear; system + tool turns are hidden by default.
//! let _ = TranscriptStyle::default();
//! ```

use crate::{MemMessage, Role};

/// Which speaker a rendered line belongs to. A renderer keys its styling
/// (color, alignment, gutter) on this. Mirrors the subset of [`Role`] a chat
/// pane shows; system / tool turns are folded away by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    /// A human turn.
    User,
    /// A model turn.
    Assistant,
    /// A tool result (only present when [`TranscriptStyle::show_tools`] is set).
    Tool,
    /// A system/preamble turn (only present when
    /// [`TranscriptStyle::show_system`] is set).
    System,
}

impl TranscriptRole {
    fn from_role(role: &Role) -> Self {
        match role {
            Role::User => Self::User,
            Role::Assistant => Self::Assistant,
            Role::Tool => Self::Tool,
            Role::System => Self::System,
        }
    }

    /// The speaker label a default renderer prints in the gutter.
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "you",
            Self::Assistant => "newt",
            Self::Tool => "tool",
            Self::System => "system",
        }
    }
}

/// One physical line of the rendered transcript — already wrapped to the pane
/// width. A renderer draws exactly this, one terminal row per [`TranscriptLine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    /// The speaker this line belongs to (drives the renderer's styling).
    pub role: TranscriptRole,
    /// `true` for the first wrapped line of a message — the row a renderer
    /// stamps the speaker label / gutter onto. Continuation lines are `false`.
    pub is_first: bool,
    /// The line text, already wrapped to the requested width (never longer).
    pub text: String,
}

/// Renderer-agnostic styling knobs for [`transcript_lines_styled`]. Defaults
/// match a typical chat pane: blank line between turns, system and tool turns
/// hidden.
#[derive(Debug, Clone)]
pub struct TranscriptStyle {
    /// Emit a blank spacer line between consecutive turns.
    pub blank_between_turns: bool,
    /// Include `system`-role turns (the preamble). Off by default.
    pub show_system: bool,
    /// Include `tool`-role turns. Off by default — a chat pane usually shows
    /// only the human/model dialogue.
    pub show_tools: bool,
}

impl Default for TranscriptStyle {
    fn default() -> Self {
        Self {
            blank_between_turns: true,
            show_system: false,
            show_tools: false,
        }
    }
}

/// Wrap a chat transcript into width-bounded [`TranscriptLine`]s ready to draw
/// into a `width`-column pane, using the default [`TranscriptStyle`].
///
/// `width` is the inner width of the target `Rect` (columns available for
/// text). A `width` of 0 is treated as 1 to avoid an infinite wrap. Hard
/// newlines in message content start new lines; over-long lines are wrapped at
/// the width boundary (word-aware when a space is available).
pub fn transcript_lines(messages: &[MemMessage], width: usize) -> Vec<TranscriptLine> {
    transcript_lines_styled(messages, width, &TranscriptStyle::default())
}

/// Like [`transcript_lines`] but with explicit [`TranscriptStyle`].
pub fn transcript_lines_styled(
    messages: &[MemMessage],
    width: usize,
    style: &TranscriptStyle,
) -> Vec<TranscriptLine> {
    let width = width.max(1);
    let mut out: Vec<TranscriptLine> = Vec::new();
    let mut first_turn = true;
    for msg in messages {
        let role = TranscriptRole::from_role(&msg.role);
        let visible = match role {
            TranscriptRole::System => style.show_system,
            TranscriptRole::Tool => style.show_tools,
            TranscriptRole::User | TranscriptRole::Assistant => true,
        };
        if !visible {
            continue;
        }
        if style.blank_between_turns && !first_turn {
            out.push(TranscriptLine {
                role,
                is_first: false,
                text: String::new(),
            });
        }
        first_turn = false;

        let mut first_line_of_msg = true;
        // Preserve hard newlines; wrap each segment to the width.
        for segment in msg.content.split('\n') {
            for wrapped in wrap_to_width(segment, width) {
                out.push(TranscriptLine {
                    role,
                    is_first: first_line_of_msg,
                    text: wrapped,
                });
                first_line_of_msg = false;
            }
        }
        // An entirely empty message still occupies its label row.
        if first_line_of_msg {
            out.push(TranscriptLine {
                role,
                is_first: true,
                text: String::new(),
            });
        }
    }
    out
}

/// Wrap a single logical line (no embedded `\n`) into chunks no wider than
/// `width` columns (by `char` count — a pragmatic proxy for display width that
/// matches how the plain scroller already reasons about output). Breaks on the
/// last space before the limit when one exists, else hard-breaks mid-word. An
/// empty input yields a single empty chunk so the line still renders.
fn wrap_to_width(line: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= width {
        return vec![line.to_string()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + width).min(chars.len());
        // Try to break on the last space within [start, end) for a clean wrap.
        let mut break_at = end;
        if end < chars.len() {
            if let Some(space) = chars[start..end].iter().rposition(|&c| c == ' ') {
                let candidate = start + space;
                // Only honor the space-break when it leaves real content; a
                // leading space would make a zero-width chunk.
                if candidate > start {
                    break_at = candidate;
                }
            }
        }
        let chunk: String = chars[start..break_at].iter().collect();
        chunks.push(chunk);
        // Skip the single break space when we broke on one.
        start = if break_at < end && chars.get(break_at) == Some(&' ') {
            break_at + 1
        } else {
            break_at
        };
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[TranscriptLine]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn user_and_assistant_turns_render_with_roles() {
        let msgs = [
            MemMessage::user("hello there"),
            MemMessage::assistant("hi, how can I help?"),
        ];
        let lines = transcript_lines(&msgs, 80);
        // First content line is the user turn, marked is_first.
        let first = &lines[0];
        assert_eq!(first.role, TranscriptRole::User);
        assert!(first.is_first);
        assert_eq!(first.text, "hello there");
        // The assistant turn appears too, with a blank spacer between them.
        assert!(lines
            .iter()
            .any(|l| l.role == TranscriptRole::Assistant && l.text == "hi, how can I help?"));
        assert!(
            lines.iter().any(|l| l.text.is_empty()),
            "a blank spacer separates turns by default"
        );
    }

    #[test]
    fn system_and_tool_turns_are_hidden_by_default() {
        let msgs = [
            MemMessage::system("you are newt"),
            MemMessage::user("hi"),
            MemMessage {
                role: Role::Tool,
                content: "tool output".into(),
            },
            MemMessage::assistant("done"),
        ];
        let lines = transcript_lines(&msgs, 80);
        assert!(!lines.iter().any(|l| l.role == TranscriptRole::System));
        assert!(!lines.iter().any(|l| l.role == TranscriptRole::Tool));
        assert!(texts(&lines).contains(&"hi"));
        assert!(texts(&lines).contains(&"done"));
    }

    #[test]
    fn system_and_tool_turns_appear_when_enabled() {
        let msgs = [
            MemMessage::system("preamble"),
            MemMessage {
                role: Role::Tool,
                content: "ran".into(),
            },
        ];
        let style = TranscriptStyle {
            show_system: true,
            show_tools: true,
            blank_between_turns: false,
        };
        let lines = transcript_lines_styled(&msgs, 80, &style);
        assert!(lines
            .iter()
            .any(|l| l.role == TranscriptRole::System && l.text == "preamble"));
        assert!(lines
            .iter()
            .any(|l| l.role == TranscriptRole::Tool && l.text == "ran"));
        // No blank spacers when disabled.
        assert!(!lines.iter().any(|l| l.text.is_empty()));
    }

    #[test]
    fn long_lines_wrap_to_the_pane_width() {
        // One 30-char word-broken line into a 10-col pane.
        let msgs = [MemMessage::user("the quick brown fox jumps over it")];
        let width = 10;
        let lines = transcript_lines(&msgs, width);
        // Every rendered line fits the pane.
        for l in &lines {
            assert!(
                l.text.chars().count() <= width,
                "line over width ({width}): {:?}",
                l.text
            );
        }
        // The first wrapped line carries the is_first flag; the rest don't.
        let content: Vec<&TranscriptLine> = lines.iter().filter(|l| !l.text.is_empty()).collect();
        assert!(content[0].is_first);
        assert!(content[1..].iter().all(|l| !l.is_first));
        // Reassembling the wrapped text recovers the words.
        let joined = content
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("quick"));
        assert!(joined.contains("jumps"));
    }

    #[test]
    fn hard_newlines_start_new_lines() {
        let msgs = [MemMessage::assistant("line one\nline two\nline three")];
        let lines = transcript_lines(&msgs, 80);
        let content: Vec<&str> = lines
            .iter()
            .filter(|l| !l.text.is_empty())
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(content, vec!["line one", "line two", "line three"]);
        // Only the very first physical line of the message is is_first.
        let firsts: Vec<bool> = lines
            .iter()
            .filter(|l| !l.text.is_empty())
            .map(|l| l.is_first)
            .collect();
        assert_eq!(firsts, vec![true, false, false]);
    }

    #[test]
    fn zero_width_does_not_loop_forever() {
        let msgs = [MemMessage::user("abc")];
        // Treated as width 1 — each char on its own line, terminates.
        let lines = transcript_lines(&msgs, 0);
        let content: Vec<&str> = lines
            .iter()
            .filter(|l| !l.text.is_empty())
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(content, vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_message_keeps_its_label_row() {
        let msgs = [MemMessage::assistant("")];
        let lines = transcript_lines(&msgs, 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].role, TranscriptRole::Assistant);
        assert!(lines[0].is_first);
        assert_eq!(lines[0].text, "");
    }

    #[test]
    fn role_labels_are_stable() {
        assert_eq!(TranscriptRole::User.label(), "you");
        assert_eq!(TranscriptRole::Assistant.label(), "newt");
        assert_eq!(TranscriptRole::Tool.label(), "tool");
        assert_eq!(TranscriptRole::System.label(), "system");
    }
}

//! **The chrome every modal wears**, in one place.
//!
//! Before this, newt-tui drew four different dialogs four different ways and
//! only one of them had a border at all:
//!
//! | surface | border | title | hint |
//! |---|---|---|---|
//! | `config_panel::render_panel` | plain, unstyled | plain | dim |
//! | `transcript_pager::run_pager` | none | bold + grey | dim |
//! | the spill pager | none | bold + grey | dim |
//! | `interaction_view` | none | none | in the body |
//!
//! The two pagers were near-verbatim copies of each other, and the panel's
//! border carried no `border_style` at all — so a dialog that had TAKEN THE
//! KEYBOARD drew in whatever colour the terminal last set.
//!
//! [`frame`] is now the only thing that draws a modal's edge. It reads
//! [`crate::theme`], so `NEWT_THEME='modal-border=cyan'` moves every dialog at
//! once, and a fifth surface gets the house style by asking rather than by
//! copying whichever neighbour it happened to read first.
//!
//! # What belongs here and what does not
//!
//! The chrome does: the border, the title, the subtitle register, the dim key
//! legend. It does NOT own the body or the key loop — those differ honestly
//! between a scrolling pager, a dial panel and a permission prompt, and a
//! shared "scroller" that fit all three would fit none of them well. The win
//! is one edge, not one widget.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::theme::{color, Role};

/// What a modal says about itself, above and below its body.
#[derive(Default)]
pub(crate) struct Chrome<'a> {
    /// The dialog's name, in the border.
    pub(crate) title: &'a str,
    /// The register beside the title — `· 3 of 12`, `· 215 lines`. Absent for
    /// a dialog with nothing to count.
    pub(crate) subtitle: Option<String>,
    /// The dim key legend along the bottom. A modal that takes the keyboard
    /// and does not say how to leave is the complaint `SpillEligibility`
    /// documents about unactionable refusals, wearing a different hat.
    pub(crate) hint: Option<&'a str>,
}

/// Draw a modal's edge and return the area its body may use.
///
/// The caller renders whatever it likes into the returned [`Rect`]; this owns
/// only the parts that should look the same everywhere.
pub(crate) fn frame(f: &mut Frame, area: Rect, chrome: &Chrome<'_>) -> Rect {
    // The title carries the subtitle so both ride the border rather than
    // spending a body row — the pagers used to spend one, which is why a
    // 3-row terminal showed them one line of content.
    let mut spans = vec![Span::styled(
        format!(" {} ", chrome.title),
        Style::default()
            .fg(color(Role::ModalTitle))
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(subtitle) = &chrome.subtitle {
        spans.push(Span::styled(
            format!("{subtitle} "),
            Style::default().fg(color(Role::Muted)),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        // Rounded, because a modal is the one surface allowed to look like a
        // separate thing sitting on top of the transcript.
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(Role::ModalBorder)))
        .title(Line::from(spans));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(hint) = chrome.hint else {
        return inner;
    };
    // The legend takes the last inner row, so the body never has to know the
    // hint exists.
    let [body, legend] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(color(Role::Dim)))),
        legend,
    );
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(chrome: &Chrome<'_>, w: u16, h: u16) -> (Vec<String>, ratatui::buffer::Buffer, Rect) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut body = Rect::default();
        term.draw(|f| {
            body = frame(f, f.area(), chrome);
            f.render_widget(Paragraph::new("BODY"), body);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let rows = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        (rows, buf, body)
    }

    /// Every modal gets an edge, and the edge is THEMED — the defect this
    /// module exists for is a dialog that owns the keyboard drawing in
    /// whatever colour the terminal last set.
    #[test]
    fn the_edge_is_drawn_and_themed() {
        let chrome = Chrome {
            title: "settings",
            subtitle: Some("· 3 of 12".to_string()),
            hint: Some("q quit"),
        };
        let (rows, buf, _) = render(&chrome, 40, 6);

        assert!(rows[0].starts_with('╭'), "rounded edge: {:?}", rows[0]);
        assert!(rows[5].starts_with('╰'), "and it closes: {:?}", rows[5]);
        let corner = buf.cell((0u16, 0u16)).unwrap();
        assert_eq!(corner.fg, color(Role::ModalBorder));
        assert_ne!(
            corner.fg,
            ratatui::style::Color::Reset,
            "an unstyled border is the defect, not the fix"
        );
    }

    /// Title and subtitle ride the BORDER, not a body row. The pagers used to
    /// spend a row on each, which is why a short terminal showed almost no
    /// content.
    #[test]
    fn the_title_rides_the_border_and_the_body_keeps_its_rows() {
        let chrome = Chrome {
            title: "spill 3",
            subtitle: Some("· 215 lines".to_string()),
            hint: Some("q quit · ↑↓ scroll"),
        };
        let (rows, _, body) = render(&chrome, 44, 8);

        assert!(rows[0].contains("spill 3"), "{:?}", rows[0]);
        assert!(rows[0].contains("215 lines"), "{:?}", rows[0]);
        assert!(rows[1].contains("BODY"), "the body starts immediately");
        // 8 rows: 2 border + 1 legend = 5 for content.
        assert_eq!(body.height, 5);
        assert!(
            rows[6].contains("q quit"),
            "legend above the edge: {:?}",
            rows[6]
        );
    }

    /// A modal with nothing to count and no keys to advertise still gets the
    /// same edge — the shape is the shared thing.
    #[test]
    fn a_bare_modal_is_the_same_shape_and_spends_no_row_on_an_absent_hint() {
        let chrome = Chrome {
            title: "permission required",
            ..Chrome::default()
        };
        let (rows, _, body) = render(&chrome, 40, 6);
        assert!(rows[0].starts_with('╭'));
        assert!(rows[0].contains("permission required"));
        assert_eq!(body.height, 4, "no hint means no reserved legend row");
    }

    /// A terminal too small for chrome must not panic or underflow. Three rows
    /// is two borders and one legend, leaving nothing — the honest answer is a
    /// zero-height body, not a wrapped-around `Rect`.
    #[test]
    fn a_terminal_with_no_room_yields_an_empty_body_rather_than_panicking() {
        let chrome = Chrome {
            title: "t",
            subtitle: None,
            hint: Some("q"),
        };
        let (_, _, body) = render(&chrome, 12, 3);
        assert_eq!(body.height, 0);
        let (_, _, tiny) = render(&chrome, 12, 2);
        assert_eq!(tiny.height, 0);
        let (_, _, degenerate) = render(&chrome, 12, 1);
        assert_eq!(degenerate.height, 0);
    }
}

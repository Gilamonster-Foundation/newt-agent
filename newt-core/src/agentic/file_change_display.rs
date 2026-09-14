//! Ephemeral presentation of a captured file change, tied to one raw result.
//!
//! The model and originals are borrowed by display code only. Existing tool
//! observations and text spill retention remain the durable and recovery paths.

use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

use crossterm::style::{Attribute, ContentStyle, StyledContent};
use newtui::diff::ChangeSet;
use newtui::{DiffData, DiffProjection, DiffSide, Tone, WidgetNoticeKind};

/// Shared host palette for static and live views; syntax changes foreground only.
pub fn file_change_style(tone: Tone) -> ContentStyle {
    use crate::tty::theme::{active, Role};
    let theme = active();
    let (foreground, background) = match tone {
        Tone::Added => (Role::Added, Some(Role::AddedBackground)),
        Tone::Removed => (Role::Removed, Some(Role::RemovedBackground)),
        Tone::Muted => (Role::Dim, None),
        Tone::Hunk | Tone::Accent => (Role::Emphasis, None),
        Tone::Healthy => (Role::Ok, None),
        Tone::Caution => (Role::GaugeWarn, None),
        Tone::Critical => (Role::GaugeCritical, None),
        _ => (Role::Text, None),
    };
    let mut style = ContentStyle {
        foreground_color: Some(theme.color(foreground)),
        background_color: background.map(|role| theme.color(role)),
        ..ContentStyle::default()
    };
    if tone == Tone::Label {
        style.attributes.set(Attribute::Bold);
    }
    style
}

/// Serialize the host's already resolved color policy. As with modal answer
/// rows, do not let Crossterm's independently memoized NO_COLOR override it.
pub fn write_file_change_row(
    writer: &mut impl Write,
    row: &[StyledContent<String>],
) -> io::Result<()> {
    for run in row {
        let style = run.style();
        for (color, background) in [
            (style.foreground_color, false),
            (style.background_color, true),
        ] {
            if let Some(color) = color {
                write_color(writer, color, background)?;
            }
        }
        crossterm::queue!(writer, crossterm::style::SetAttributes(style.attributes))?;
        write!(writer, "{}\x1b[0m", run.content())?;
    }
    Ok(())
}

fn write_color(
    writer: &mut impl Write,
    color: crossterm::style::Color,
    background: bool,
) -> io::Result<()> {
    use crossterm::style::Color;
    let selector = if background { 48 } else { 38 };
    let index = match color {
        Color::Reset => return write!(writer, "\x1b[{}m", if background { 49 } else { 39 }),
        Color::Rgb { r, g, b } => return write!(writer, "\x1b[{selector};2;{r};{g};{b}m"),
        Color::AnsiValue(index) => index,
        Color::Black => 0,
        Color::DarkRed => 1,
        Color::DarkGreen => 2,
        Color::DarkYellow => 3,
        Color::DarkBlue => 4,
        Color::DarkMagenta => 5,
        Color::DarkCyan => 6,
        Color::Grey => 7,
        Color::DarkGrey => 8,
        Color::Red => 9,
        Color::Green => 10,
        Color::Yellow => 11,
        Color::Blue => 12,
        Color::Magenta => 13,
        Color::Cyan => 14,
        Color::White => 15,
    };
    write!(writer, "\x1b[{selector};5;{index}m")
}

fn safe_text(text: &str) -> String {
    crate::notes_scan::neutralize_for_display(text).replace('\t', "<U+0009>")
}

/// A file tool's already observed change and its exact receipt in one result.
#[derive(Clone, Debug)]
pub struct FileChangePresentation {
    changes: Arc<ChangeSet>,
    path: Arc<str>,
    before: Option<Arc<str>>,
    after: Option<Arc<str>>,
    receipt: Arc<str>,
    range: Range<usize>,
    #[cfg(feature = "markdown-syntect")]
    syntax: Arc<std::sync::OnceLock<[super::syntax_foreground::SyntaxLines; 2]>>,
}

/// Widest review a projection renders. The complete projection materializes
/// every row at full width, so an extreme terminal would otherwise allocate
/// rows x width padding cells; wider hosts see the same rows, left-aligned.
const REVIEW_WIDTH_LIMIT: usize = 1024;

impl FileChangePresentation {
    /// Bind a captured model and originals to the exact receipt byte range in
    /// one raw result. Display callers still validate that binding before use.
    pub fn new(
        changes: ChangeSet,
        path: String,
        before: Option<String>,
        after: Option<String>,
        receipt: String,
        range: Range<usize>,
    ) -> Self {
        Self {
            changes: Arc::new(changes),
            path: path.into(),
            before: before.map(Into::into),
            after: after.map(Into::into),
            receipt: receipt.into(),
            range,
            #[cfg(feature = "markdown-syntect")]
            syntax: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Validate against the original raw String, before any display escaping.
    /// On mismatch the caller retains its existing full-text presentation.
    pub fn surrounding_text<'a>(&self, raw: &'a str) -> Option<(&'a str, &'a str)> {
        if raw.get(self.range.clone())? != self.receipt.as_ref() {
            return None;
        }
        Some((raw.get(..self.range.start)?, raw.get(self.range.end..)?))
    }

    /// Keep outcome text and appended display notices, without applying raw
    /// byte ranges to an escaped String. Unrelated overrides use the plain path.
    pub fn surrounding_display_text(&self, raw: &str, displayed: &str) -> Option<(String, String)> {
        let (prefix, suffix) = self.surrounding_text(raw)?;
        let extra = if displayed == raw {
            ""
        } else {
            displayed.strip_prefix(&safe_text(raw))?
        };
        Some((
            safe_text(prefix),
            format!("{}{}", safe_text(suffix), safe_text(extra)),
        ))
    }

    /// Complete logical projection at the current width. NewtUI reports the
    /// row count even when the one-row measuring rectangle has no source room.
    pub fn complete_projection(&self, width: usize) -> DiffProjection {
        let count = self
            .project(1, 1, 0)
            .output
            .notices
            .iter()
            .find_map(|notice| match notice.kind {
                WidgetNoticeKind::OmittedRows { before, after } => Some(before + after),
                _ => None,
            })
            .unwrap_or(0);
        self.project(width, count.saturating_add(1), 0)
    }

    /// Safe rows for a host viewport. Prefix and suffix must come from
    /// `surrounding_display_text`; source glyphs come only from NewtUI runs.
    pub fn display_rows(
        &self,
        prefix: &str,
        suffix: &str,
        width: usize,
    ) -> Vec<Vec<StyledContent<String>>> {
        let projection = self.complete_projection(width);
        let plain = |text: &str| {
            text.lines()
                .map(|line| vec![file_change_style(Tone::Muted).apply(line.to_string())])
                .collect::<Vec<_>>()
        };
        let mut rows = plain(prefix);
        rows.extend(self.styled_lines(&projection, file_change_style));
        if projection
            .output
            .notices
            .iter()
            .any(|notice| matches!(notice.kind, WidgetNoticeKind::GlyphReplacements { .. }))
        {
            rows.extend(plain("[File-change display substitutes unsupported source/header glyphs with ?; this view is not a raw patch.]"));
        }
        if width > REVIEW_WIDTH_LIMIT {
            rows.extend(plain(&format!(
                "[File-change review width bounded to {REVIEW_WIDTH_LIMIT} columns.]"
            )));
        }
        rows.extend(plain(suffix));
        rows
    }

    /// Safe cells come only from NewtUI; originals are never reinserted here.
    /// Width is capped at `REVIEW_WIDTH_LIMIT`.
    pub fn project(&self, width: usize, height: usize, row: usize) -> DiffProjection {
        newtui::diff_with_sources(
            DiffData::new(&self.changes)
                .context(usize::MAX)
                .row_offset(row),
            width.min(REVIEW_WIDTH_LIMIT),
            height,
        )
    }

    /// Original full-file text is solely input to the optional highlighter.
    pub fn original(&self, side: DiffSide) -> Option<&str> {
        match side {
            DiffSide::Old => self.before.as_deref(),
            DiffSide::New => self.after.as_deref(),
        }
    }

    /// Language inference may inspect the path; views use NewtUI's safe header.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Apply host tones and optional syntax foregrounds to NewtUI's safe cells.
    /// No text is read back from the originals, and semantic backgrounds survive.
    pub fn styled_lines(
        &self,
        projection: &DiffProjection,
        tone_style: impl Fn(Tone) -> ContentStyle,
    ) -> Vec<Vec<StyledContent<String>>> {
        let cells: Vec<Vec<_>> = projection
            .output
            .lines
            .iter()
            .map(|line| {
                line.runs
                    .iter()
                    .flat_map(|run| {
                        let style = tone_style(run.tone);
                        run.text.chars().map(move |glyph| (glyph, style))
                    })
                    .collect()
            })
            .collect();
        #[cfg(feature = "markdown-syntect")]
        let cells = self.apply_syntax(cells, projection);
        cells
            .into_iter()
            .map(|line| {
                let mut runs: Vec<(ContentStyle, String)> = Vec::new();
                for (glyph, style) in line {
                    if let Some((_, text)) =
                        runs.last_mut().filter(|(previous, _)| *previous == style)
                    {
                        text.push(glyph);
                    } else {
                        runs.push((style, glyph.to_string()));
                    }
                }
                runs.into_iter()
                    .map(|(style, text)| style.apply(text))
                    .collect()
            })
            .collect()
    }

    #[cfg(feature = "markdown-syntect")]
    fn apply_syntax(
        &self,
        mut cells: Vec<Vec<(char, ContentStyle)>>,
        projection: &DiffProjection,
    ) -> Vec<Vec<(char, ContentStyle)>> {
        use crossterm::style::Color;
        let syntax = self.syntax.get_or_init(|| {
            let language = std::path::Path::new(self.path())
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("");
            [self.before.as_deref(), self.after.as_deref()].map(|body| {
                body.map(|body| super::syntax_foreground::highlight(language, body))
                    .unwrap_or_default()
            })
        });
        for source in &projection.sources {
            // This presentation carries one captured file, not a multi-file
            // source archive. Unknown addresses simply keep semantic tones.
            if source.file != 0 {
                continue;
            }
            let side = match source.side {
                DiffSide::Old => 0,
                DiffSide::New => 1,
            };
            let Some(ranges) = source
                .line_number
                .checked_sub(1)
                .and_then(|line| syntax[side].get(line as usize))
                .and_then(Option::as_ref)
            else {
                continue;
            };
            let Some(row) = cells.get_mut(source.output_row) else {
                continue;
            };
            for (column, scalar) in source
                .output_columns
                .clone()
                .zip(source.source_codepoints.clone())
            {
                let Some(run) = ranges.iter().find(|run| run.codepoints.contains(&scalar)) else {
                    continue;
                };
                let Some((_, style)) = row.get_mut(column) else {
                    continue;
                };
                let foreground = run.style.foreground;
                style.foreground_color = Some(Color::Rgb {
                    r: foreground.r,
                    g: foreground.g,
                    b: foreground.b,
                });
            }
        }
        cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extreme_geometry_bounds_padding_without_changing_the_captured_model() {
        let unified = format!(
            "--- state.txt\n+++ state.txt\n@@ -1,100 +1,100 @@\n{}{}",
            "-old\n".repeat(100),
            "+new\n".repeat(100)
        );
        let model = newtui::diff::from_unified(&unified).unwrap();
        let receipt = model.to_markdown();
        let hint = FileChangePresentation::new(
            model,
            "state.txt".into(),
            Some("old\n".repeat(100)),
            Some("new\n".repeat(100)),
            receipt.clone(),
            0..receipt.len(),
        );
        let projection = hint.complete_projection(8192);
        let cells: usize = projection
            .output
            .lines
            .iter()
            .map(|line| {
                line.runs
                    .iter()
                    .map(|run| run.text.chars().count())
                    .sum::<usize>()
            })
            .sum();
        assert!(
            cells <= 1024 * 1024,
            "terminal geometry allocated {cells} projected cells"
        );
        assert_eq!(hint.changes.to_unified(), unified);
        let rows = hint.display_rows("", "", 8192);
        assert!(
            rows.iter()
                .flatten()
                .any(|run| run.content().contains("review width bounded")),
            "the review must explain the narrower projection"
        );
    }

    fn hint(receipt: &str, range: Range<usize>) -> FileChangePresentation {
        let changes = newtui::diff::from_unified(
            "--- before.rs\n+++ after.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n tail\n",
        )
        .unwrap();
        FileChangePresentation::new(
            changes,
            "state.rs".into(),
            Some("old\ntail\n".into()),
            Some("new\ntail\n".into()),
            receipt.into(),
            range,
        )
    }

    #[test]
    fn exact_receipt_range_preserves_status_and_build_suffix_without_scanning() {
        let receipt = "```diff\nactual patch\n```";
        let prefix = format!("wrote a name containing {receipt}\n");
        let suffix = "\nbuild check failed: ```diff\nnot the receipt\n```";
        let raw = format!("{prefix}{receipt}{suffix}");
        let presentation = hint(receipt, prefix.len()..prefix.len() + receipt.len());
        assert_eq!(
            presentation.surrounding_text(&raw),
            Some((prefix.as_str(), suffix))
        );
    }

    #[test]
    fn stale_misaligned_and_invalid_utf8_ranges_fall_back_without_panicking() {
        let raw = "éreceipt終";
        let good = hint("receipt", 2..9);
        assert_eq!(good.surrounding_text(raw), Some(("é", "終")));
        assert!(good.surrounding_text("échanged終").is_none());
        for range in [1..9, 2..10, Range { start: 9, end: 2 }, 2..usize::MAX] {
            assert!(hint("receipt", range).surrounding_text(raw).is_none());
        }
    }

    #[test]
    fn projection_uses_safe_widget_cells_and_honors_the_requested_row() {
        let hint = hint("receipt", 0..7);
        let expected = newtui::diff_with_sources(
            DiffData::new(&hint.changes)
                .context(usize::MAX)
                .row_offset(2),
            30,
            3,
        );
        assert_eq!(hint.project(30, 3, 2), expected);
        assert_eq!(hint.original(DiffSide::Old), Some("old\ntail\n"));
        assert_eq!(hint.original(DiffSide::New), Some("new\ntail\n"));
        assert_eq!(hint.path(), "state.rs");
    }

    #[cfg(feature = "markdown-syntect")]
    #[test]
    fn syntax_styles_only_safe_source_cells_and_preserves_semantic_backgrounds() {
        use crossterm::style::Color;
        let before = "/*\nlet café = \"🦎\";\n*/\n";
        let after = "/*\nlet café = \"🦎e\u{301}\u{1b}\t\";\n*/\n";
        let changes = newtui::diff::from_unified(
            "--- state.rs\n+++ state.rs\n@@ -1,3 +1,3 @@\n /*\n-let café = \"🦎\";\n+let café = \"🦎e\u{301}\u{1b}\t\";\n */\n",
        ).unwrap();
        let hint = FileChangePresentation::new(
            changes,
            "state.rs".into(),
            Some(before.into()),
            Some(after.into()),
            "receipt".into(),
            0..7,
        );
        let projection = hint.project(70, 12, 0);
        let painted = hint.styled_lines(&projection, |_| ContentStyle {
            background_color: Some(Color::DarkGreen),
            ..ContentStyle::default()
        });
        assert_eq!(painted.len(), projection.output.lines.len());
        for (painted, expected) in painted.iter().zip(&projection.output.lines) {
            let visible: String = painted.iter().map(|run| run.content().as_str()).collect();
            let safe = expected.text();
            assert_eq!(visible, safe, "only widget glyphs may be emitted");
            assert!(visible.chars().all(|ch| !ch.is_control()));
            assert!(painted
                .iter()
                .all(|run| run.style().background_color == Some(Color::DarkGreen)));
        }
        let source = projection
            .sources
            .iter()
            .find(|source| source.side == DiffSide::New && source.line_number == 2)
            .unwrap();
        let cells: Vec<_> = painted[source.output_row]
            .iter()
            .flat_map(|run| run.content().chars().map(|ch| (ch, *run.style())))
            .collect();
        let full = super::super::syntax_foreground::highlight("rs", after);
        let comment = full[1].as_ref().unwrap()[0].style.foreground;
        let isolated =
            super::super::syntax_foreground::highlight("rs", after.lines().nth(1).unwrap());
        assert_ne!(
            comment,
            isolated[0].as_ref().unwrap()[0].style.foreground,
            "the full-file state matters"
        );
        assert_eq!(
            cells[source.output_columns.start].1.foreground_color,
            Some(Color::Rgb {
                r: comment.r,
                g: comment.g,
                b: comment.b
            })
        );
        assert!(cells[source.output_columns.clone()]
            .iter()
            .any(|(ch, _)| *ch == '?'));
    }
}

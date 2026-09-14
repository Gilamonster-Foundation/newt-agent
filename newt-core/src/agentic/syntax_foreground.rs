//! Shared full-file syntax state for Markdown and observed file-change cells.
//!
//! Ranges contain no source text. Byte offsets serve the existing Markdown
//! adapter; Unicode scalar offsets address NewtUI's safe source-cell mapping.

use std::ops::Range;

use syntect::highlighting::Style;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyntaxRange {
    pub bytes: Range<usize>,
    pub codepoints: Range<usize>,
    pub style: Style,
}

pub(crate) type SyntaxLines = Vec<Option<Vec<SyntaxRange>>>;

/// Each source line is highlighted with a synthetic trailing newline, as the
/// existing Markdown renderer requires. A failed line keeps its plain fallback.
pub(crate) fn highlight(lang: &str, body: &str) -> SyntaxLines {
    use std::sync::OnceLock;
    use syntect::easy::HighlightLines;
    use syntect::highlighting::ThemeSet;
    use syntect::parsing::SyntaxSet;

    static ASSETS: OnceLock<(SyntaxSet, ThemeSet)> = OnceLock::new();
    let (syntaxes, themes) = ASSETS.get_or_init(|| {
        (
            SyntaxSet::load_defaults_newlines(),
            ThemeSet::load_defaults(),
        )
    });
    let syntax = syntaxes
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, &themes.themes["base16-ocean.dark"]);
    body.split('\n')
        .map(|line| {
            let with_newline = format!("{line}\n");
            let ranges = highlighter.highlight_line(&with_newline, syntaxes).ok()?;
            let mut bytes = 0;
            let mut codepoints = 0;
            Some(
                ranges
                    .into_iter()
                    .map(|(style, text)| {
                        let byte_start = bytes;
                        let scalar_start = codepoints;
                        bytes += text.len();
                        codepoints += text.chars().count();
                        SyntaxRange {
                            bytes: byte_start..bytes,
                            codepoints: scalar_start..codepoints,
                            style,
                        }
                    })
                    .collect(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_source_preserves_multiline_syntax_state() {
        let lines = highlight("rs", "/* opening\nlet value = 42;\n*/\nlet value = 42;");
        let comment = lines[1].as_ref().expect("a valid Rust line");
        let code = lines[3].as_ref().expect("a valid Rust line");
        assert_ne!(comment[0].style.foreground, code[0].style.foreground);
        assert!(comment
            .iter()
            .all(|run| run.style.foreground == comment[0].style.foreground));
        assert!(lines
            .iter()
            .flatten()
            .flatten()
            .all(|run| run.style.foreground.a == 255));
    }

    #[test]
    fn byte_ranges_and_scalar_ranges_address_the_same_original_fragments() {
        let body = "let café = \"🦎e\u{301}\";";
        let with_newline = format!("{body}\n");
        let lines = highlight("rs", body);
        let ranges = lines[0].as_ref().expect("a valid Unicode Rust line");
        let mut bytes = 0;
        let mut codepoints = 0;
        for run in ranges {
            assert_eq!(run.bytes.start, bytes);
            assert_eq!(run.codepoints.start, codepoints);
            let fragment = &with_newline[run.bytes.clone()];
            let scalars: String = with_newline
                .chars()
                .skip(run.codepoints.start)
                .take(run.codepoints.len())
                .collect();
            assert_eq!(fragment, scalars);
            bytes = run.bytes.end;
            codepoints = run.codepoints.end;
        }
        assert_eq!(bytes, with_newline.len());
        assert_eq!(codepoints, with_newline.chars().count());
    }
}

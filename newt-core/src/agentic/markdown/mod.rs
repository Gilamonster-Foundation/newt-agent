//! Markdown → ANSI rendering for assistant chat output (Step 25.1, #568).
//!
//! newt's chat surface is a **plain scroller** (`docs/decisions/plain_scroller_tui.md`):
//! no alternate screen, no widgets, no repainting already-scrolled lines. So we
//! parse Markdown with `pulldown-cmark` and render it ourselves to styled,
//! word-wrapped **scrolled lines** on `crossterm` SGR codes — never a ratatui
//! surface. Color is auto-gated by the caller; with color off this is a
//! byte-for-byte passthrough (and under `--no-default-features` the whole
//! renderer is replaced by a passthrough shim, so the headless wyvern tier
//! carries no markdown dependencies).
//!
//! Renders inline emphasis, headings, lists (bullet/ordered/task), blockquotes,
//! thematic breaks, fenced code (dim, un-highlighted), and GFM tables
//! (box-drawing, width-fit). Block-aware streaming (25.3), config + `/markdown`
//! (25.4), the wyvern source-tidy (25.5), and syntect highlighting (25.6) follow.

mod emitter;
mod inline;
mod stream;
mod table;
mod width;

pub use stream::MarkdownStreamWriter;

use emitter::Emitter;
use pulldown_cmark::{Options, Parser};

/// Rendering inputs. `cols` is injected by the caller (the TUI passes
/// `term_cols()`); the renderer never probes the terminal itself, keeping it
/// pure and deterministic for the mocked unit tier.
#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    /// Render styled ANSI when `true`; raw passthrough when `false`.
    pub color: bool,
    /// Wrap width in display columns.
    pub cols: usize,
}

/// Render a complete Markdown document to a styled ANSI string (no trailing
/// newline — the caller owns the final line break). With `color == false` the
/// source is returned verbatim.
pub fn render_markdown(src: &str, opts: RenderOpts) -> String {
    if !opts.color {
        return src.to_string();
    }
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(src, options);
    let mut em = Emitter::new(opts.cols);
    for ev in parser {
        em.handle(ev);
    }
    em.finish()
}

#[cfg(test)]
mod tests {
    use super::{render_markdown, RenderOpts};

    // SGR fragments, mirrored from inline.rs so expectations read clearly.
    const BOLD: &str = "\x1b[1m";
    const ITALIC: &str = "\x1b[3m";
    const UNDER: &str = "\x1b[4m";
    const STRIKE: &str = "\x1b[9m";
    const RESET: &str = "\x1b[0m";
    const ORANGE: &str = "\x1b[38;2;220;60;20m";
    const FADE: &str = "\x1b[38;2;90;90;90m";

    fn r(src: &str) -> String {
        render_markdown(
            src,
            RenderOpts {
                color: true,
                cols: 80,
            },
        )
    }
    fn rw(src: &str, cols: usize) -> String {
        render_markdown(src, RenderOpts { color: true, cols })
    }

    #[test]
    fn plain_text_passes_through_unstyled() {
        assert_eq!(r("hello world"), "hello world");
    }

    #[test]
    fn inline_emphasis_variants() {
        assert_eq!(r("**bold**"), format!("{BOLD}bold{RESET}"));
        assert_eq!(r("*it*"), format!("{ITALIC}it{RESET}"));
        assert_eq!(r("~~s~~"), format!("{STRIKE}s{RESET}"));
        assert_eq!(r("`c`"), format!("{FADE}c{RESET}"));
    }

    #[test]
    fn emphasis_adjacent_to_text_has_no_spurious_space() {
        // The whole reason wrap operates on cells, not words.
        assert_eq!(r("un**bold**ed"), format!("un{BOLD}bold{RESET}ed"));
    }

    #[test]
    fn heading_is_bold_orange() {
        assert_eq!(r("# Title"), format!("{BOLD}{ORANGE}Title{RESET}"));
    }

    #[test]
    fn paragraphs_are_separated_by_a_blank_line() {
        assert_eq!(r("a\n\nb"), "a\n\nb");
    }

    #[test]
    fn greedy_word_wrap_breaks_at_spaces() {
        assert_eq!(rw("alpha beta gamma", 11), "alpha beta\ngamma");
    }

    #[test]
    fn wrap_counts_display_width_not_chars() {
        // 日本語 = 6 cols, 語 = 2 cols. At budget 8 they cannot share a line
        // (6+1+2 = 9). A char-count budget would see 3+1+1 = 5 and NOT wrap —
        // so this discriminates display-width awareness from char counting.
        assert_eq!(rw("日本語 語", 8), "日本語\n語");
    }

    #[test]
    fn bullet_list_tight() {
        assert_eq!(r("- one\n- two"), "• one\n• two");
    }

    #[test]
    fn ordered_list_numbers() {
        assert_eq!(r("1. a\n2. b"), "1. a\n2. b");
    }

    #[test]
    fn task_list_markers() {
        assert_eq!(r("- [x] done\n- [ ] todo"), "• ✓ done\n• ☐ todo");
    }

    #[test]
    fn nested_list_indents_under_parent() {
        // Two-space marker width → child marker sits at column 2.
        assert_eq!(r("- a\n  - b"), "• a\n  • b");
    }

    #[test]
    fn blockquote_has_a_dim_bar() {
        assert_eq!(r("> quote"), format!("{FADE}│ {RESET}quote"));
    }

    #[test]
    fn fenced_code_is_dim_and_inset_not_reflowed() {
        assert_eq!(
            r("```\nlet x = 1;\n```"),
            format!("  {FADE}let x = 1;{RESET}")
        );
    }

    #[test]
    fn thematic_break_is_a_full_width_rule() {
        assert_eq!(r("---"), format!("{FADE}{}{RESET}", "─".repeat(80)));
    }

    #[test]
    fn link_text_is_underlined_with_dim_url() {
        // The space before the dim URL is a wrap separator (regenerated
        // unstyled), so it sits between the two styled runs.
        assert_eq!(
            r("[text](https://x.io)"),
            format!("{UNDER}text{RESET} {FADE}(https://x.io){RESET}")
        );
    }

    #[test]
    fn color_off_is_byte_for_byte_passthrough() {
        for s in [
            "**x** and `y`",
            "# h\n\n- a\n- b",
            "| a | b |\n| - | - |\n| 1 | 2 |",
            "> quote\n\n```\ncode\n```",
            "",
        ] {
            assert_eq!(
                render_markdown(
                    s,
                    RenderOpts {
                        color: false,
                        cols: 80
                    }
                ),
                s,
                "color-off must return source verbatim"
            );
        }
    }

    #[test]
    fn empty_input_renders_empty() {
        assert_eq!(r(""), "");
    }

    // ---- Step 25.2: GFM tables ----

    /// Strip SGR escape sequences so the box-drawing skeleton can be asserted
    /// independently of the styling.
    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for n in chars.by_ref() {
                    if n == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn table_box_drawing_skeleton() {
        let t = "| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(
            strip(&r(t)),
            "┌───┬───┐\n│ a │ b │\n├───┼───┤\n│ 1 │ 2 │\n└───┴───┘"
        );
    }

    #[test]
    fn table_header_is_bold_borders_are_dim() {
        let out = r("| a |\n|---|\n| 1 |");
        assert!(
            out.contains(&format!("{BOLD}a")),
            "header cell must be bold"
        );
        assert!(out.contains(FADE), "borders must be dim");
        // A body cell carries no styling.
        assert!(strip(&out).contains("│ 1 │"));
    }

    #[test]
    fn table_right_alignment_pads_on_the_left() {
        // `:--` would be left, `--:` is right. Column width 2 ("ab"); "x" → " x".
        let t = "| ab |\n|---:|\n| x |";
        assert!(
            strip(&r(t)).contains("│  x │"),
            "right-aligned cell pads left"
        );
    }

    #[test]
    fn table_overflow_truncates_with_ellipsis() {
        // Narrow budget forces the 8-wide cell down; it truncates to `abcde…`.
        let t = "| name |\n|------|\n| abcdefgh |";
        assert!(
            strip(&rw(t, 10)).contains("abcde…"),
            "overflowing cell truncates with an ellipsis"
        );
    }

    #[test]
    fn table_column_width_counts_cjk_display_width() {
        // 日本語 = 6 display cols → top rule spans 6+2 = 8 dashes (not 3+2 = 5).
        let t = "| 日本語 |\n|--------|\n| x |";
        assert!(
            strip(&r(t)).contains("┌────────┐"),
            "CJK header sized by display width, not char count"
        );
    }

    #[test]
    fn table_inside_blockquote_is_prefixed_with_the_bar() {
        let out = r("> | a |\n> |---|\n> | 1 |");
        // Every rendered table line opens with the quote bar, then the box.
        for line in out.lines() {
            let plain = strip(line);
            assert!(
                plain.starts_with("│ ┌")
                    || plain.starts_with("│ │")
                    || plain.starts_with("│ ├")
                    || plain.starts_with("│ └"),
                "quoted table line must open with the bar then box: {plain:?}"
            );
        }
    }
}

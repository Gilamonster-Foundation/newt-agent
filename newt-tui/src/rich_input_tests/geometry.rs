use super::*;
use crate::rich_input::gutter::use_gutter;

#[test]
fn wrap_segments_empty_and_fitting() {
    assert_eq!(wrap_segments("", 10, 10), vec![(0, String::new())]);
    assert_eq!(wrap_segments("hi", 10, 10), vec![(0, "hi".to_string())]);
}

#[test]
fn wrap_segments_breaks_at_spaces_word_wrap() {
    // first_w = cont_w = 8. The breaking space ends its segment (no char
    // dropped — every index has a home for cursor mapping).
    let segs = wrap_segments("hello world foo", 8, 8);
    assert_eq!(
        segs,
        vec![
            (0, "hello ".to_string()),
            (6, "world ".to_string()),
            (12, "foo".to_string()),
        ]
    );
    // Concatenating the segments reproduces the line exactly.
    let joined: String = segs.iter().map(|(_, s)| s.as_str()).collect();
    assert_eq!(joined, "hello world foo");
}

#[test]
fn wrap_segments_uses_contextual_emoji_widths() {
    for text in ["❤️a", "👩\u{200D}💻a"] {
        assert_eq!(str_width(text), 3, "fixture must occupy three cells");
        let segs = wrap_segments(text, 2, 2);
        assert_eq!(segs.len(), 2, "{text:?} must wrap before the trailing a");
        assert_eq!(segs[1].1, "a");
        assert!(
            segs.iter().all(|(_, segment)| str_width(segment) <= 2),
            "every contextual-width segment must fit its two-cell budget: {segs:?}"
        );
        assert_eq!(
            segs.iter()
                .map(|(_, segment)| segment.as_str())
                .collect::<String>(),
            text,
            "wrapping must preserve the presentation and joiner scalars"
        );
    }
}

#[test]
fn overhang_rows_cursor_uses_contextual_emoji_widths() {
    let prompt = Line::from("!");
    for (text, cursor_col) in [("❤️a", 2), ("👩\u{200D}💻a", 3)] {
        let (_, cx, cy) = overhang_rows(&prompt, &[text.to_string()], (0, cursor_col), 1, 4, None);
        assert_eq!((cx, cy), (3, 0), "cursor follows the two-cell emoji");
    }
}

#[test]
fn committed_prompt_echo_wraps_a_long_line_without_clipping() {
    // The reported bug: a committed `› <prompt>` line wider than the
    // terminal was CLIPPED at the right edge (its tail lost). The echo must
    // instead carry the overflow onto continuation rows, exactly as the
    // interactive input surface already does via `wrap_segments`.
    let width = 24;
    let hang = 1;
    let body = "the quick brown fox jumps over the lazy dog and keeps on running east";
    let rows = echo_body_rows(body, hang, width);
    // Every row fits within the terminal once its leading marker is added,
    // so ratatui's fixed-height paint never truncates it.
    for r in &rows {
        let marker = if r.lead {
            ECHO_CHEVRON.chars().count()
        } else {
            hang
        };
        assert!(
            marker + r.text.chars().count() <= width,
            "row overruns width {width} (lead={}): {:?}",
            r.lead,
            r.text
        );
    }
    // Nothing is dropped: the wrapped segments reassemble the whole prompt.
    let joined: String = rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(joined, body, "the full prompt survives the wrap");
    assert!(
        rows.len() > 1,
        "a line wider than the terminal actually wraps"
    );
    assert!(rows[0].lead, "the first row carries the chevron");
    assert!(
        rows[1..].iter().all(|r| !r.lead),
        "continuations hang under it — no second chevron"
    );
}

#[test]
fn committed_prompt_echo_is_unchanged_when_the_line_fits() {
    // A prompt that fits the width is a single lead row carrying the whole
    // text — byte-identical to the pre-fix single-row form (0.7.x preserved).
    let rows = echo_body_rows("ship it", 1, 40);
    assert_eq!(
        rows,
        vec![EchoRow {
            lead: true,
            text: "ship it".to_string(),
        }]
    );
}

#[test]
fn committed_prompt_echo_preserves_multiline_input() {
    // Multi-line input keeps one lead row (chevron) then hang rows —
    // unchanged from the historical per-line layout when nothing overflows.
    let rows = echo_body_rows("alpha\nbeta", 1, 40);
    assert_eq!(
        rows,
        vec![
            EchoRow {
                lead: true,
                text: "alpha".to_string(),
            },
            EchoRow {
                lead: false,
                text: "beta".to_string(),
            },
        ]
    );
}

#[test]
fn committed_note_wraps_long_lines_without_clipping() {
    // The sibling emitter: a `:command` note (e.g. a capability-denied
    // diagnostic) wider than the terminal must wrap, not clip.
    let width = 16;
    let note = "capability denied: fs_write does not permit '/etc/hosts'";
    let rows = note_rows(note, width);
    for r in &rows {
        assert!(
            r.chars().count() <= width,
            "note row overruns width {width}: {r:?}"
        );
    }
    assert_eq!(rows.join(""), note, "the full note survives the wrap");
    assert!(rows.len() > 1, "a long note line actually wraps");
}

#[test]
fn wrap_segments_hard_breaks_an_unbreakable_run() {
    assert_eq!(
        wrap_segments("abcdefghij", 4, 4),
        vec![
            (0, "abcd".to_string()),
            (4, "efgh".to_string()),
            (8, "ij".to_string()),
        ]
    );
}

#[test]
fn wrap_segments_honors_a_narrower_first_width() {
    // First segment fits 3 (after a wide prompt), continuations fit 6.
    let segs = wrap_segments("abcdefghi", 3, 6);
    assert_eq!(segs[0], (0, "abc".to_string()));
    assert_eq!(segs[1], (3, "defghi".to_string()));
}

#[test]
fn overhang_rows_wraps_a_long_line_and_tracks_the_cursor() {
    let prompt = Line::from("❯ "); // width 2
    let lines = vec!["hello world foo".to_string()];
    // width 8 → row0 text width = 8-2 = 6; continuations 8-1 = 7 (g=1).
    // "hello " (6, after the 2-col prompt), then "world f" (7), then "oo".
    let (rows, cx, cy) = overhang_rows(&prompt, &lines, (0, 15), 1, 8, None);
    assert!(rows.len() >= 2, "the long line wrapped to multiple rows");
    // Cursor at end (col 15) lands on the last wrapped row.
    assert_eq!(cy as usize, rows.len() - 1);
    assert!(cx >= 1, "cursor is indented on the continuation row");
}

#[test]
fn overhang_rows_short_line_is_one_row_after_the_prompt() {
    let prompt = Line::from("❯ "); // width 2
    let (rows, cx, cy) = overhang_rows(&prompt, &["hi".to_string()], (0, 2), 1, 80, None);
    assert_eq!(rows.len(), 1);
    assert_eq!(cy, 0);
    assert_eq!(cx, 2 + 2, "prompt width (2) + cursor col (2)");
}

#[test]
fn overhang_rows_cursor_sits_on_the_hint_not_after_it() {
    let prompt = Line::from("❯ "); // width 2
                                   // Empty line with a dim hint: the cursor anchors at the prompt end (col
                                   // 2) — ON the hint's first cell — NOT after the whole hint string.
    let (rows, cx, cy) = overhang_rows(
        &prompt,
        &[String::new()],
        (0, 0),
        1,
        80,
        Some("vi INSERT — type…"),
    );
    assert_eq!((cx, cy), (2, 0), "cursor at prompt end, on the hint");
    let text: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("vi INSERT"),
        "hint rendered after the cursor: {text:?}"
    );
}

#[test]
fn resolve_gutter_auto_off_and_fixed() {
    // auto (None): prompt-width gutter when it fits, else 0.
    assert_eq!(
        resolve_gutter(None, 80),
        GUTTER_W,
        "auto wide → inline gutter"
    );
    assert_eq!(resolve_gutter(None, 50), 0, "auto squished → stacked (0)");
    // off (Some(0)): always 0.
    assert_eq!(resolve_gutter(Some(0), 80), 0);
    // fixed N: exactly N, clamped to the usable width.
    assert_eq!(resolve_gutter(Some(3), 80), 3, "3-space indent");
    assert_eq!(
        resolve_gutter(Some(25), 80),
        25,
        "wide enough to hold the prompt"
    );
    assert_eq!(resolve_gutter(Some(200), 80), 79, "clamped to width-1");
}

#[test]
fn use_gutter_drops_when_over_a_third() {
    // Keep the gutter while GUTTER_W (19) <= 0.33*width, i.e. width >= ~58.
    assert!(use_gutter(80), "gutter fits at 80 cols");
    assert!(use_gutter(58), "19 <= 0.33*58 (19.14) → gutter stays on");
    assert!(!use_gutter(57), "19 > 0.33*57 (18.81) → drop the gutter");
    assert!(!use_gutter(40), "way too narrow → drop the gutter");
    assert!(!use_gutter(0), "zero width never uses a gutter");
}

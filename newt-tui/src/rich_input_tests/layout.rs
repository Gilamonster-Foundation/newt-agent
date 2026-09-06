use super::*;
use test_support::{emacs_editor, key, nano_editor, special, type_chars, vi_editor};

/// Replace a footer `[YYYY-MM-DD HH:MM:SS]` stamp with fixed digits.
///
/// For frame comparisons that must not depend on the wall clock. Narrow on
/// purpose: it rewrites a run only when it has the timestamp's exact
/// shape, so a tab number, a row count or a model name keeps its own
/// digits and stays byte-compared.
fn normalize_clock(rows: &[String]) -> Vec<String> {
    // `[dddd-dd-dd dd:dd:dd]` — the shape `footer_line` renders.
    const SHAPE: &str = "[dddd-dd-dd dd:dd:dd]";
    let looks_like_stamp = |window: &str| {
        window
            .chars()
            .zip(SHAPE.chars())
            .all(|(c, want)| match want {
                'd' => c.is_ascii_digit(),
                other => c == other,
            })
    };
    rows.iter()
        .map(|row| {
            let chars: Vec<char> = row.chars().collect();
            let mut out = chars.clone();
            for start in 0..chars.len().saturating_sub(SHAPE.len() - 1) {
                let window: String = chars[start..start + SHAPE.len()].iter().collect();
                if looks_like_stamp(&window) {
                    for (offset, want) in SHAPE.chars().enumerate() {
                        if want == 'd' {
                            out[start + offset] = '0';
                        }
                    }
                }
            }
            out.into_iter().collect()
        })
        .collect()
}

/// The normalizer is narrow: it rewrites a timestamp and nothing else.
///
/// Without this, "mask the clock" could quietly become "mask every digit",
/// and the frame comparison would stop being able to see a tab number.
#[test]
fn the_clock_normalizer_touches_only_a_timestamp() {
    let rows = vec![
        "[2026-09-04 20:31:07] vi  1 tab  40%".to_string(),
        "no stamp here: 12345".to_string(),
    ];
    let out = normalize_clock(&rows);
    assert_eq!(out[0], "[0000-00-00 00:00:00] vi  1 tab  40%");
    assert_eq!(out[1], rows[1], "a row without a stamp is untouched");
    // Two different clock readings normalize to the same row...
    let a = normalize_clock(&["[2026-09-04 20:31:07] x".to_string()]);
    let b = normalize_clock(&["[2026-09-04 20:31:08] x".to_string()]);
    assert_eq!(a, b);
    // ...while a real difference beside it still shows.
    let c = normalize_clock(&["[2026-09-04 20:31:08] y".to_string()]);
    assert_ne!(b, c);
}

/// #1669 PR-B, the load-bearing invariant: with fewer than two tabs the
/// frame is **byte-identical** to the pre-bar surface.
///
/// Not "an empty row" and not "a row of spaces" — no row at all. Almost
/// every session is single-conversation, and the bar is not worth a
/// permanent row of their terminal. Comparing the whole rendered buffer
/// rather than eyeballing one row is what makes that a guarantee.
#[test]
fn a_single_tab_frame_is_byte_identical_to_no_bar() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let ed = emacs_editor();
    let ta = TextArea::new(vec!["hello".to_string()]);

    let render = |tabs: &[crate::tab_bar::TabCell]| -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(40, 5)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &ta,
                &ed,
                Some(1),
                RichStatus {
                    tabs,
                    ..RichStatus::default()
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..5)
            .map(|y| {
                (0..40)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    };

    let none = render(&[]);
    let one = render(&[crate::tab_bar::TabCell {
        number: 1,
        label: "solo".into(),
        active: true,
        degraded: false,
        pending: false,
    }]);
    // **The footer carries a live wall clock**, and these are two separate
    // renders. `footer_line` stamps `chrono::Local::now()` at SECOND
    // resolution, so two frames taken either side of a tick differ in the
    // clock and nowhere else — a failure that reads "the tab bar leaked a
    // row" while meaning "a second passed". It fired on the coverage job,
    // where instrumentation makes each render slow enough to straddle a
    // boundary often.
    //
    // Only the stamp is normalized, and only where it parses as one, so
    // every other cell stays byte-compared — including the tab number,
    // which is also a digit and must NOT be masked. The invariant under
    // test is that fewer than two tabs render no row; it was never about
    // the clock's digits, and a unit test must not read the wall clock at
    // all (`CLAUDE.md`, testing tiers).
    let none = normalize_clock(&none);
    let one = normalize_clock(&one);
    assert_eq!(none, one, "one tab must render exactly like no tabs");
    assert!(
        !one.iter().any(|r| r.contains("solo")),
        "the single tab's label appears nowhere: {one:?}"
    );
}

/// Two tabs claim exactly one row, at the BOTTOM of the region, and the
/// rows above are untouched.
#[test]
fn two_tabs_add_one_row_below_the_clock() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let ed = emacs_editor();
    let ta = TextArea::new(vec!["hello".to_string()]);
    let cell = |n: usize, l: &str, a: bool| crate::tab_bar::TabCell {
        number: n,
        label: l.into(),
        active: a,
        degraded: false,
        pending: false,
    };
    let render = |tabs: &[crate::tab_bar::TabCell]| -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(40, 5)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &ta,
                &ed,
                Some(1),
                RichStatus {
                    tabs,
                    ..RichStatus::default()
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..5)
            .map(|y| {
                (0..40)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    };

    let none = render(&[]);
    let two = render(&[cell(1, "build", true), cell(2, "deploy", false)]);
    // The bar is the OUTERMOST row: everything above it belongs to the
    // selected tab, so the container sits outside its contents. The clock
    // stays directly above it — the last row of the tab's own frame.
    assert!(
        two[4].contains("1:build") && two[4].contains("2:deploy"),
        "the bar is the bottom-most row: {:?}",
        two[4]
    );
    assert!(
        two[3].contains("emacs"),
        "the clock sits directly above the bar: {:?}",
        two[3]
    );
    assert!(
        none[4].contains("emacs"),
        "with one tab the clock is the last row: {:?}",
        none[4]
    );
    assert!(
        !none.iter().any(|row| row.contains("1:build")),
        "sanity: the no-tab frame has no bar"
    );
}

#[test]
fn vi_ex_command_renders_on_a_bottom_row_when_multiline() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    // #531: a `:`-command on a multi-line buffer belongs on its own bottom
    // row, vi-style — not glued to the first row's prompt.
    let mut ed = vi_editor();
    let mut ta = TextArea::new(vec!["hello".to_string(), "world".to_string()]);
    ed.input(special(KeyCode::Esc), &mut ta); // INSERT → NORMAL
    ed.input(key(':'), &mut ta); // open the ex line
    type_chars(&mut ed, &mut ta, "wq");
    assert!(
        ex_bottom_line(&ed, &ta).is_some(),
        "multi-line ex → bottom row"
    );

    // Two-line layout (#527): row 0 is the status header; the message renders
    // below it, and the `:`-command on the last (bottom) row.
    let mut term = Terminal::new(TestBackend::new(40, 4)).unwrap();
    term.draw(|f| draw(f, &ta, &ed, Some(1), RichStatus::default()))
        .unwrap();
    let buf = term.backend().buffer();
    let row = |y: u16| -> String {
        (0..40)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect::<String>()
    };
    // Bottom of the INPUT region — which is one row above the footer.
    assert!(
        row(2).starts_with(":wq"),
        "command on the input region's bottom row: {:?}",
        row(2)
    );
    assert!(
        row(1).contains("hello"),
        "message renders below the header: {:?}",
        row(1)
    );
    assert!(
        !row(0).contains(":wq") && !row(1).contains(":wq"),
        "command must NOT be glued to the input rows"
    );
}

#[test]
fn vi_ex_command_stays_inline_on_a_single_line() {
    // Single line keeps the inline chevron↔`:` swap (the part the user likes).
    let mut ed = vi_editor();
    let mut ta = TextArea::new(vec!["hi".to_string()]);
    ed.input(special(KeyCode::Esc), &mut ta);
    ed.input(key(':'), &mut ta);
    type_chars(&mut ed, &mut ta, "wq");
    assert!(
        ex_bottom_line(&ed, &ta).is_none(),
        "single-line stays inline"
    );
    let line = prompt_line(&ed, true);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains(":wq"), "single-line ex is inline: {text:?}");
}

#[test]
fn vi_ex_command_bottom_row_in_wide_gutter_mode() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    // Same as the overhang case but through the wide-gutter render path
    // (gutter >= GUTTER_W) — the `:command` still lands on the bottom row.
    let mut ed = vi_editor();
    let mut ta = TextArea::new(vec!["hello".to_string(), "world".to_string()]);
    ed.input(special(KeyCode::Esc), &mut ta);
    ed.input(key(':'), &mut ta);
    type_chars(&mut ed, &mut ta, "wq");
    let mut term = Terminal::new(TestBackend::new(80, 4)).unwrap();
    term.draw(|f| draw(f, &ta, &ed, Some(25), RichStatus::default()))
        .unwrap(); // 25 >= GUTTER_W (19)
    let buf = term.backend().buffer();
    // Bottom of the INPUT region — one row above the footer.
    let last: String = (0..80)
        .map(|x| buf.cell((x, 2)).unwrap().symbol().to_string())
        .collect();
    assert!(
        last.starts_with(":wq"),
        "command on the input region's bottom row (wide gutter): {last:?}"
    );
}

#[test]
fn slash_palette_renders_above_the_input_row() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    // #1674: the palette renders INSIDE the existing inline draw path —
    // between the status header and the input row — through the same
    // frame as the editor. No second surface, no second event loop.
    let mut palette = PaletteState::from_corpus();
    palette.on_buffer_change("", "/");
    palette.on_buffer_change("/", "/model");
    let rows = palette.viewport_rows(8);
    assert!(rows >= 2, "the /model filter keeps several corpus entries");
    palette.set_viewport(rows);
    let editor = nano_editor();
    let ta = TextArea::new(vec!["/model".to_string()]);
    let h = 1 + rows as u16 + 1; // header + palette + input
    let mut term = Terminal::new(TestBackend::new(100, h)).unwrap();
    term.draw(|f| {
        draw(
            f,
            &ta,
            &editor,
            Some(1),
            RichStatus {
                palette: Some(&palette),
                ..RichStatus::default()
            },
        );
    })
    .unwrap();
    let buf = term.backend().buffer();
    let row = |y: u16| -> String {
        (0..100)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect()
    };
    // Directly under the header: the highlighted first prefix match, with
    // its corpus description beside it.
    //
    // #2009 PR3 retired `/models` into `/status models`, so the corpus —
    // and therefore the palette, which is derived from it and never
    // hand-edited — no longer offers it. `/model` now matches itself,
    // which is the palette correctly teaching the surface that exists.
    assert!(
        row(1).starts_with("❯ /model "),
        "highlight on the first match: {:?}",
        row(1)
    );
    assert!(
        row(1).contains("switch model"),
        "description rides beside the command: {:?}",
        row(1)
    );
    // The input still shows the typed line — one row above the footer,
    // which is now what occupies the bottom.
    assert!(
        row(h - 2).contains("❯ /model"),
        "input row intact below the palette: {:?}",
        row(h - 2)
    );
    assert!(
        row(h - 1).contains("nano"),
        "and the footer is the last row: {:?}",
        row(h - 1)
    );
}

#[test]
fn overhang_prompt_is_inline_with_one_space_hanging_continuation() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let editor = vi_editor();
    // Two input lines (as if a `o`/newline added a continuation).
    let ta = TextArea::new(vec!["this".to_string(), "more".to_string()]);
    // Width 80 so the full status header fits (it clips on a narrow term);
    // height 3: row 0 = status header (#527), rows 1-2 = the input.
    // Header, two input rows, footer.
    let mut term = Terminal::new(TestBackend::new(80, 4)).unwrap();
    // gutter = 1 → the overhang layout (the default).
    term.draw(|f| {
        draw(
            f,
            &ta,
            &editor,
            Some(1),
            RichStatus {
                model: "m",
                endpoint: "http://e:1",
                ..RichStatus::default()
            },
        );
    })
    .unwrap();
    let buf = term.backend().buffer();
    let row = |y: u16| -> String {
        (0..80)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect::<String>()
    };
    // Machine state is the FOOTER's, on the last row — it used to lead.
    assert!(
        row(3).contains("vi --INSERT--") && row(3).contains("m @ http://e:1"),
        "footer row carries mode + model @ endpoint: {:?}",
        row(3)
    );
    // Row 1: the prompt prefixes the first input line inline (`❯ this`).
    assert!(
        row(1).contains("❯ this"),
        "first input line rides on the prompt row: {:?}",
        row(1)
    );
    // Row 2: continuation hangs by exactly one space, not the prompt width.
    assert!(
        row(2).starts_with(" more"),
        "continuation is 1-space hang-indented: {:?}",
        row(2)
    );
}

#[test]
fn a_running_job_leads_the_layout_rather_than_shifting_the_input() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let editor = vi_editor();
    let textarea = TextArea::default();
    let job = BackgroundJob::start("indexing repository");
    // Four regions when a job runs: activity, header, input, footer.
    let mut term = Terminal::new(TestBackend::new(80, 4)).unwrap();
    term.draw(|f| {
        draw(
            f,
            &textarea,
            &editor,
            Some(1),
            RichStatus {
                model: "m",
                endpoint: "http://e:1",
                background_jobs: std::slice::from_ref(&job),
                ..RichStatus::default()
            },
        );
    })
    .unwrap();
    let buf = term.backend().buffer();
    let row = |y: u16| -> String {
        (0..80)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect::<String>()
    };

    // The activity row LEADS the layout now. It was bottom-anchored, which
    // put a row that appears and disappears directly under the input —
    // shifting the input and its footer under the operator's hands every
    // time a job started or finished. At the top it displaces nothing they
    // are looking at.
    assert!(
        row(0).contains("background") && row(0).contains("indexing repository"),
        "the live job leads: {:?}",
        row(0)
    );
    assert!(
        row(2).contains('\u{276f}'),
        "the prompt sits below the activity row and the header: {:?}",
        row(2)
    );
}

#[test]
fn completed_background_job_has_no_indicator_row() {
    let first = BackgroundJob::start("indexing repository");
    let second = BackgroundJob::start("warming symbols");
    let text = |jobs: &[BackgroundJob]| {
        background_line(jobs, 0).map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
    };
    let both = text(&[first.clone(), second.clone()]).unwrap();
    assert!(both.contains("background (2)"), "{both}");
    assert!(both.contains(first.label()) && both.contains(second.label()));

    first.finish();
    let one = text(&[first, second.clone()]).unwrap();
    assert!(!one.contains("background (2)"), "{one}");
    assert!(!one.contains("indexing repository"), "{one}");
    assert!(one.contains(second.label()), "{one}");

    second.finish();
    assert!(text(&[second]).is_none());
}

fn row_text(editor: &Editor) -> String {
    prompt_line(editor, true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

fn footer_text(editor: &Editor, model: &str, endpoint: &str) -> String {
    footer_line(editor, model, endpoint, None, true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

fn header_text(session: &str, headline: &str) -> String {
    header_line(session, headline)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// **The header answers "which conversation is this", and nothing else.**
///
/// #1671's session name lives here. What used to sit beside it — the
/// timestamp, the mode word, the model, the gauge — moved to the footer,
/// because a row carrying both identity and machine state had no single
/// owner, and grew an echo of the draft that the line below it was already
/// showing.
#[test]
fn the_header_carries_identity_and_nothing_else() {
    let named = header_text("mesh docking", "");
    assert!(named.contains("[mesh docking]"), "{named}");

    // The untitled form (#shortid) and the ephemeral marker render too.
    assert!(header_text("#a1b2c3d4", "").contains("[#a1b2c3d4]"));
    assert!(header_text("ephemeral", "").contains("[ephemeral]"));

    // Prose sits beside the name, separated by a space — no bracket, no rule.
    let with_prose = header_text("mesh docking", "wiring the dock");
    assert_eq!(with_prose, "[mesh docking] wiring the dock");

    // Neither half is mandatory, and an absent half renders NOTHING rather
    // than an empty bracket or a stray separator.
    assert_eq!(header_text("", "just prose"), "just prose");
    assert_eq!(header_text("solo", ""), "[solo]");
    assert_eq!(header_text("", ""), "");

    // Machine state is the footer's, and must not have followed the name.
    for absent in ["vi", "@", "k/"] {
        assert!(
            !with_prose.contains(absent),
            "`{absent}` belongs to the footer: {with_prose}"
        );
    }
}

/// **No region is separated by a horizontal rule.**
///
/// A full-width `─────` run is a word-wrap hazard at every terminal width,
/// and adjacency already says these rows are regions. Asserted because a
/// rule is the obvious thing to reach for when someone later wants the
/// regions to "read as separate".
#[test]
fn no_region_draws_a_horizontal_rule() {
    let ed = vi_editor();
    let rows = [
        header_text("session", "headline"),
        footer_text(&ed, "model", "http://endpoint"),
    ];
    for row in rows {
        for rule in ['\u{2500}', '\u{2501}', '\u{2550}', '_'] {
            assert!(
                !row.contains(&rule.to_string().repeat(4)),
                "a run of `{rule}` is a rule: {row}"
            );
        }
    }
}

#[test]
fn header_shows_context_budget_gauge_when_known() {
    let ed = vi_editor();
    let text = |g| -> String {
        footer_line(&ed, "m", "e", g, true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    };
    assert!(
        text(Some((972_000, 1_024_000))).contains("972k/1024k"),
        "the gauge shows used/budget once the budget is known"
    );
    assert!(
        !text(None).contains("k/"),
        "no gauge until a budget is known"
    );
    assert!(
        !text(Some((100, 0))).contains("k/"),
        "a zero budget shows no gauge (no divide-by-zero, no noise)"
    );
}

#[test]
fn native_status_row_shows_the_insert_indicator() {
    // The input row carries the `❯` indicator; the header carries the clock,
    // mode word, and model @ endpoint (two-line layout, #527).
    let editor = vi_editor();
    assert!(row_text(&editor).contains('❯'), "insert indicator");
}

#[test]
fn blocking_modal_recedes_the_chat_chevron_without_changing_its_shape() {
    let editor = vi_editor();
    let active = prompt_line_with_focus(&editor, true, true);
    let inactive = prompt_line_with_focus(&editor, true, false);

    assert_eq!(active.spans[0].content, inactive.spans[0].content);
    assert_eq!(
        active.spans[0].style.fg,
        Some(Color::from(newt_core::tty::ACTIVE_INPUT_CT))
    );
    assert_eq!(inactive.spans[0].style.fg, Some(Color::DarkGray));
}

/// **Exactly one prompt on screen is live.** The receding chevron is
/// asserted above at line level; this asserts it at FRAME level, which is
/// the form the operator actually reported — two chevrons both painted in
/// the live accent, with nothing saying which one owned the keyboard.
/// Scanning every cell means a future row that reintroduces the accent
/// while a modal is up fails here, not in a screenshot.
#[test]
fn no_cell_carries_the_live_accent_while_a_modal_owns_the_keyboard() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let editor = vi_editor();
    let textarea = TextArea::new(vec!["a draft that survives".to_string()]);
    let accent = Color::from(newt_core::tty::ACTIVE_INPUT_CT);

    let render = |chat_inactive: bool| {
        let (width, height) = (60_u16, 4_u16);
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &textarea,
                &editor,
                Some(1),
                RichStatus {
                    chat_inactive,
                    ..RichStatus::default()
                },
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .filter(|&(x, y)| buf.cell((x, y)).unwrap().style().fg == Some(accent))
            .count()
    };

    assert!(
        render(false) > 0,
        "the mounted chat chevron is accented while it owns the keyboard"
    );
    assert_eq!(
        render(true),
        0,
        "a modal owns the keyboard: nothing beneath it may still read as live"
    );
}

#[test]
fn the_footer_shows_datetime_mode_and_model_endpoint() {
    let insert = vi_editor(); // starts in INSERT
    let h = footer_text(&insert, "nemotron-3-nano:30b", "http://REDACTED-HOST:11434");
    assert!(h.starts_with('['), "datetime stamp: {h:?}");
    assert!(h.contains("vi --INSERT--"), "{h:?}");
    assert!(
        h.contains("nemotron-3-nano:30b @ http://REDACTED-HOST:11434"),
        "{h:?}"
    );
    // NORMAL flips the mode word live.
    let mut normal = vi_editor();
    let mut ta = TextArea::default();
    normal.input(special(KeyCode::Esc), &mut ta);
    assert!(footer_text(&normal, "m", "e").contains("vi --NORMAL--"));
    // emacs/nano show the bare editor name; empty model omits the `@`.
    assert!(footer_text(&emacs_editor(), "m", "e").contains("emacs"));
    assert!(!footer_text(&insert, "", "").contains('@'));
}

#[test]
fn mode_hint_advertises_the_other_editor_modes() {
    assert!(vi_editor().mode_hint(false).contains("INSERT"));
    assert!(vi_editor().mode_hint(false).contains("/nano"));
    assert!(vi_editor().mode_hint(false).contains("/emacs"));
}

/// **The `:` belongs to an OPEN ex line, and to nothing else.**
///
/// vi's own semantics, which this surface used to contradict: NORMAL mode
/// showed a highlighted `:` as its prompt indicator, so an operator who
/// had pressed Esc was looking at what vi only ever shows for a command
/// line they had not opened. Worse, the way OUT of it read as backing out
/// of a command rather than as pressing `i`.
///
/// In vi the buffer looks the same in NORMAL as in INSERT; the mode lives
/// in the status line (`-- INSERT --`) and in the cursor. So the chevron
/// stays in both modes, and the `:` appears exactly when `:` is pressed.
#[test]
fn only_an_open_ex_line_shows_the_colon() {
    let mut ta = TextArea::default();

    // INSERT: the chevron.
    assert!(row_text(&vi_editor()).starts_with('❯'));

    // NORMAL: still the chevron, and NOT a command line.
    let mut normal = vi_editor();
    normal.input(special(KeyCode::Esc), &mut ta); // INSERT → NORMAL
    let row = row_text(&normal);
    assert!(
        row.starts_with('❯'),
        "NORMAL keeps the input chevron: {row:?}"
    );
    assert!(
        !row.trim_start().starts_with(':'),
        "NORMAL is not command-line mode: {row:?}"
    );

    // The mode is still discoverable — where vi keeps it.
    assert!(
        footer_text(&normal, "m", "e").contains("vi --NORMAL--"),
        "the header carries the mode"
    );
    assert!(
        normal.mode_hint(false).contains("i: insert"),
        "the hint says how to get back to typing"
    );

    // `:` opens the ex line, and THEN the colon shows with the command.
    let mut ex = vi_editor();
    ex.input(special(KeyCode::Esc), &mut ta);
    ex.input(key(':'), &mut ta);
    ex.input(key('w'), &mut ta);
    let exrow = row_text(&ex);
    assert!(exrow.contains(":w"), "ex line shows the command: {exrow:?}");
    assert!(!exrow.contains('❯'), "the ex line owns the row: {exrow:?}");

    // Esc closes the ex line and lands back in NORMAL — chevron, no colon.
    ex.input(special(KeyCode::Esc), &mut ta);
    let back = row_text(&ex);
    assert!(
        back.starts_with('❯') && !back.contains(":w"),
        "Esc closes the command line, back to the input row: {back:?}"
    );
}

use super::*;
use crate::rich_input::command::INACTIVE_COMMAND_BG;
use crossterm::event::Event;
use test_support::{emacs_editor, key, line_text, special, type_chars, vi_editor, RecordingSink};

fn rendered_row(
    textarea: &TextArea,
    editor: &Editor,
    width: u16,
    height: u16,
    row: u16,
) -> (String, Vec<Style>) {
    rendered_row_with(textarea, editor, width, height, row, Some(1), false)
}

fn rendered_row_with(
    textarea: &TextArea,
    editor: &Editor,
    width: u16,
    height: u16,
    row: u16,
    gutter: Option<u16>,
    chat_inactive: bool,
) -> (String, Vec<Style>) {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| {
        draw(
            f,
            textarea,
            editor,
            gutter,
            RichStatus {
                chat_inactive,
                ..RichStatus::default()
            },
        );
    })
    .unwrap();
    let buf = term.backend().buffer();
    let text = (0..width)
        .map(|x| buf.cell((x, row)).unwrap().symbol().to_string())
        .collect::<String>();
    let styles = (0..width)
        .map(|x| buf.cell((x, row)).unwrap().style())
        .collect();
    (text, styles)
}

fn rendered_cursor_with(
    textarea: &TextArea,
    editor: &Editor,
    width: u16,
    height: u16,
    gutter: Option<u16>,
) -> (u16, u16) {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| {
        draw(f, textarea, editor, gutter, RichStatus::default());
    })
    .unwrap();
    let cursor = term.get_cursor_position().unwrap();
    (cursor.x, cursor.y)
}

#[test]
fn special_command_rendering_live_bang_replaces_the_chat_chevron() {
    let editor = emacs_editor();
    let textarea = TextArea::new(vec!["! date".to_string()]);

    let (row, styles) = rendered_row(&textarea, &editor, 40, 3, 1);

    assert!(
        row.starts_with("! date"),
        "bang owns the prompt cell: {row:?}"
    );
    assert!(
        !row.contains('❯'),
        "no chat chevron on a shell escape: {row:?}"
    );
    assert!(
        styles[0].bg.is_some(),
        "the live command row is visually distinct from chat"
    );
}

#[test]
fn special_command_rendering_bang_never_inherits_the_chat_gutter() {
    let editor = emacs_editor();
    let textarea = TextArea::new(vec!["! date".to_string()]);

    for gutter in [None, Some(30)] {
        let (row, _) = rendered_row_with(&textarea, &editor, 80, 3, 1, gutter, false);
        assert!(
            row.starts_with("! date"),
            "gutter {gutter:?} split the marker from its command: {row:?}"
        );
    }
}

#[test]
fn special_command_rendering_bang_height_uses_command_geometry_for_every_gutter() {
    let chrome = Chrome {
        headline: "",
        modal: None,
        model: "",
        endpoint: "",
        gauge: None,
        session: "",
        background_jobs: &[],
        tabs: &[],
    };

    // 153 exposes a non-1 continuation indent; 159 exposes gutter=0.
    // Both expose the wide-gutter logical-line shortcut at 80 columns.
    for tail_len in [153, 159] {
        for gutter in [None, Some(0), Some(1), Some(7), Some(30)] {
            let body = format!("!{}", "x".repeat(tail_len));
            let mut mounted = MountedEditor::new(Edit::Emacs, gutter, Vec::new(), &body);
            let shown = bang_view(&mounted.textarea).expect("a real bang escape");
            let prompt = command_line(CommandKind::Bang, "");
            let drawn_rows = overhang_rows(
                &prompt,
                shown.textarea.lines(),
                shown.textarea.cursor(),
                1,
                80,
                None,
            )
            .0
            .len() as u16;
            // `+ 2`: the header above the input and the footer below it.
            let expected = drawn_rows.clamp(1, MAX_INPUT_ROWS) + 2;

            assert_eq!(
                mounted.wanted_rows(80, 30, &chrome),
                expected,
                "tail={tail_len}, gutter={gutter:?}: allocation must match draw_overhang(g=1)"
            );
        }
    }
}

#[test]
fn special_command_rendering_bang_cursor_uses_the_marker_for_hidden_prefix() {
    let editor = emacs_editor();
    let mut textarea = TextArea::new(vec!["  ! date".to_string()]);

    for hidden_col in 0..3 {
        textarea.move_cursor(CursorMove::Jump(0, hidden_col));
        assert_eq!(
            rendered_cursor_with(&textarea, &editor, 40, 3, Some(30)),
            (0, 1),
            "cursor on hidden prefix column {hidden_col} belongs on the visible ! marker"
        );
    }
    textarea.move_cursor(CursorMove::Jump(0, 3));
    assert_eq!(
        rendered_cursor_with(&textarea, &editor, 40, 3, Some(30)),
        (1, 1),
        "cursor immediately after the source ! belongs after the visible marker"
    );
}

#[test]
fn special_command_rendering_recedes_bang_and_ex_behind_a_modal() {
    let bang_editor = emacs_editor();
    let bang_textarea = TextArea::new(vec!["! date".to_string()]);
    let (_, bang_styles) = rendered_row_with(&bang_textarea, &bang_editor, 40, 3, 1, Some(1), true);
    assert_eq!(bang_styles[0].fg, Some(Color::DarkGray));
    assert_eq!(bang_styles[0].bg, Some(INACTIVE_COMMAND_BG));

    let mut ex_editor = vi_editor();
    let mut ex_textarea = TextArea::default();
    ex_editor.input(special(KeyCode::Esc), &mut ex_textarea);
    ex_editor.input(key(':'), &mut ex_textarea);
    type_chars(&mut ex_editor, &mut ex_textarea, "help");
    let (_, ex_styles) = rendered_row_with(&ex_textarea, &ex_editor, 40, 3, 1, Some(1), true);
    assert_eq!(ex_styles[0].fg, Some(Color::DarkGray));
    assert_eq!(ex_styles[0].bg, Some(INACTIVE_COMMAND_BG));
}

#[test]
fn special_command_rendering_committed_bang_uses_a_command_marker() {
    let mut sink = RecordingSink::default();
    echo_submitted(&mut sink, "! date", Some(1)).unwrap();

    let command = &sink.batches[0][1];
    let text = line_text(command);
    assert!(text.starts_with("! date"), "command echo: {text:?}");
    assert!(
        !text.contains(ECHO_CHEVRON),
        "a shell escape is not a chat turn: {text:?}"
    );
    assert!(
        command.spans.iter().all(|span| span.style.bg.is_some()),
        "the whole committed command row carries command chrome"
    );
}

#[test]
fn special_command_rendering_wide_echoes_fit_cells_without_losing_text() {
    let width = 10;
    for (kind, command, tail) in [
        (CommandKind::Bang, "!日本語版確認用", "日本語版確認用"),
        (CommandKind::Ex, ":日本語版確認用", "日本語版確認用"),
    ] {
        let rows = command_body_rows(command, kind, width);
        for row in &rows {
            let prefix = if row.lead { 1 } else { 2 };
            assert!(
                prefix + str_width(&row.text) <= width,
                "{kind:?} row escaped {width} terminal cells: {row:?}"
            );
        }
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<String>(),
            tail,
            "{kind:?} wrapping must preserve every source character"
        );

        let lines = command_echo_lines(command, kind, width);
        for line in &lines[1..] {
            assert_eq!(
                line.width(),
                width,
                "{kind:?} slab must be padded to exactly {width} cells: {line:?}"
            );
        }
    }
}

#[test]
fn special_command_rendering_contextual_emoji_echoes_fit_cells() {
    for command in ["!❤️a", "!👩\u{200D}💻a"] {
        let rows = command_body_rows(command, CommandKind::Bang, 3);
        for row in &rows {
            let prefix = if row.lead { 1 } else { 2 };
            assert!(
                prefix + str_width(&row.text) <= 3,
                "emoji row escaped three terminal cells: {row:?}"
            );
        }
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<String>(),
            command.trim_start_matches('!'),
            "emoji wrapping must preserve every source scalar"
        );
    }
}

#[test]
fn special_command_rendering_cancels_an_invisible_bang_selection_before_editing() {
    let mut mounted = MountedEditor::new(Edit::Emacs, Some(1), Vec::new(), "! date");
    let mut sink = RecordingSink::default();
    mounted.textarea.move_cursor(CursorMove::Jump(0, 2));
    mounted.textarea.start_selection();
    mounted.textarea.move_cursor(CursorMove::End);
    assert!(mounted.textarea.is_selecting());
    assert!(
        bang_view(&mounted.textarea).is_none(),
        "the renderer must not fabricate an unselected bang projection"
    );

    mounted.on_event(Event::Key(key('X')), &mut sink).unwrap();

    assert_eq!(mounted.textarea.lines(), ["! dateX"]);
    assert!(
        !mounted.textarea.is_selecting(),
        "display and editing state both remain unselected"
    );
    assert!(
        bang_view(&mounted.textarea).is_some(),
        "normal bang chrome resumes after selection normalization"
    );
}

#[test]
fn special_command_rendering_vi_ex_echoes_before_its_output() {
    let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
    let mut sink = RecordingSink::default();
    for code in [
        KeyCode::Esc,
        KeyCode::Char(':'),
        KeyCode::Char('h'),
        KeyCode::Char('e'),
        KeyCode::Char('l'),
        KeyCode::Char('p'),
        KeyCode::Enter,
    ] {
        mounted
            .on_event(Event::Key(special(code)), &mut sink)
            .unwrap();
    }

    assert_eq!(sink.batches.len(), 2, "command, then its output");
    let command = line_text(&sink.batches[0][1]);
    let output: Vec<String> = sink.batches[1].iter().map(line_text).collect();
    assert!(
        command.trim_end().starts_with(":help"),
        "the executed ex command is committed first: {command:?}"
    );
    assert!(
        output.iter().any(|line| line.contains("vi  Esc=NORMAL")),
        "the command output follows: {output:?}"
    );
    assert!(
        !command.contains(ECHO_CHEVRON),
        "true ex command has no chat chevron: {command:?}"
    );
}

#[test]
fn special_command_rendering_shift_enter_does_not_echo_an_open_ex_line() {
    let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
    let mut sink = RecordingSink::default();
    for code in [
        KeyCode::Esc,
        KeyCode::Char(':'),
        KeyCode::Char('h'),
        KeyCode::Char('e'),
        KeyCode::Char('l'),
        KeyCode::Char('p'),
    ] {
        mounted
            .on_event(Event::Key(special(code)), &mut sink)
            .unwrap();
    }

    mounted
        .on_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            &mut sink,
        )
        .unwrap();

    assert!(sink.batches.is_empty(), "Shift-Enter executes nothing");
    assert_eq!(mounted.editor.ex(), Some("help"));
    assert_eq!(mounted.textarea.lines().len(), 2, "it inserts a newline");
}

#[test]
fn special_command_rendering_does_not_commit_an_unconfirmed_ex_command() {
    let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "draft");
    let mut sink = RecordingSink::default();
    for code in [
        KeyCode::Esc,
        KeyCode::Char(':'),
        KeyCode::Char('w'),
        KeyCode::Char('q'),
        KeyCode::Enter,
    ] {
        mounted
            .on_event(Event::Key(special(code)), &mut sink)
            .unwrap();
    }

    assert!(mounted.editor.confirm_prompt().is_some());
    assert!(
        sink.batches.is_empty(),
        "requesting confirmation is not executing :wq"
    );
    mounted
        .on_event(Event::Key(special(KeyCode::Char('n'))), &mut sink)
        .unwrap();
    assert!(sink.batches.is_empty(), "a cancelled :wq stays ephemeral");
}

#[test]
fn special_command_rendering_confirmed_ex_preserves_and_echoes_its_spelling() {
    for command in ["wq", "x"] {
        let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "draft");
        let mut sink = RecordingSink::default();
        mounted
            .on_event(Event::Key(special(KeyCode::Esc)), &mut sink)
            .unwrap();
        mounted
            .on_event(Event::Key(special(KeyCode::Char(':'))), &mut sink)
            .unwrap();
        for c in command.chars() {
            mounted
                .on_event(Event::Key(special(KeyCode::Char(c))), &mut sink)
                .unwrap();
        }
        mounted
            .on_event(Event::Key(special(KeyCode::Enter)), &mut sink)
            .unwrap();

        assert!(mounted.editor.confirm_prompt().is_some());
        assert!(sink.batches.is_empty(), "confirmation is still ephemeral");
        assert_eq!(
            mounted
                .on_event(Event::Key(special(KeyCode::Char('y'))), &mut sink)
                .unwrap(),
            Some(EditorOutcome::LineThenQuit("draft".to_string()))
        );

        assert_eq!(
            sink.batches.len(),
            2,
            "confirmed command, then submitted draft"
        );
        let command_echo = line_text(&sink.batches[0][1]);
        assert!(
            command_echo.trim_end().starts_with(&format!(":{command}")),
            "confirmed spelling survives until execution: {command_echo:?}"
        );
        assert!(
            line_text(&sink.batches[1][1]).starts_with(ECHO_CHEVRON),
            "the submitted draft remains an ordinary model turn"
        );
    }
}

#[test]
fn special_command_rendering_insert_colon_remains_chat() {
    let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
    let mut sink = RecordingSink::default();
    for code in [
        KeyCode::Char(':'),
        KeyCode::Char('h'),
        KeyCode::Char('e'),
        KeyCode::Char('l'),
        KeyCode::Char('p'),
    ] {
        assert_eq!(
            mounted
                .on_event(Event::Key(special(code)), &mut sink)
                .unwrap(),
            None
        );
    }
    let outcome = mounted
        .on_event(Event::Key(special(KeyCode::Enter)), &mut sink)
        .unwrap();

    assert_eq!(outcome, Some(EditorOutcome::Line(":help".to_string())));
    let text = line_text(&sink.batches[0][1]);
    assert!(
        text.starts_with(ECHO_CHEVRON),
        "INSERT-mode colon text stays a model turn: {text:?}"
    );
}

#[test]
fn special_command_rendering_single_line_ex_hides_the_draft() {
    let mut editor = vi_editor();
    let mut textarea = TextArea::new(vec!["draft".to_string()]);
    editor.input(special(KeyCode::Esc), &mut textarea);
    editor.input(key(':'), &mut textarea);
    type_chars(&mut editor, &mut textarea, "help");

    let (row, styles) = rendered_row(&textarea, &editor, 40, 3, 1);
    assert!(row.starts_with(":help"), "ex command owns the row: {row:?}");
    assert!(
        !row.contains("draft"),
        "the hidden draft is not concatenated to the command: {row:?}"
    );
    assert!(
        styles[0].bg.is_some(),
        "the live ex row is visually distinct from chat"
    );
}

#[test]
fn special_command_rendering_vi_ex_cursor_uses_terminal_cells() {
    for (lines, height, expected_y) in [
        // height, expected cursor row. Both grow by one for the footer,
        // and the row itself is offset by the header above the input.
        (vec!["draft".to_string()], 3, 1),
        (vec!["draft".to_string(), "second".to_string()], 4, 2),
    ] {
        let mut editor = vi_editor();
        let mut textarea = TextArea::new(lines);
        editor.input(special(KeyCode::Esc), &mut textarea);
        editor.input(key(':'), &mut textarea);
        type_chars(&mut editor, &mut textarea, "日本");

        assert_eq!(
            rendered_cursor_with(&textarea, &editor, 40, height, Some(1)),
            (5, expected_y),
            "the ':' marker plus two double-cell characters place the cursor at column five"
        );
    }
}

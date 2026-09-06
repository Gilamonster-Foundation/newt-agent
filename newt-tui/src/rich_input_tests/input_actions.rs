use super::*;
use test_support::{ctrl, emacs_editor, key, nano_editor, special, type_chars, vi_editor};

/// Drive `:`-command `cmd` from NORMAL and return the Enter step.
fn run_ex(ed: &mut Editor, ta: &mut TextArea, cmd: &str) -> Step {
    ed.input(special(KeyCode::Esc), ta); // INSERT → NORMAL
    ed.input(key(':'), ta);
    for c in cmd.chars() {
        ed.input(key(c), ta);
    }
    ed.input(special(KeyCode::Enter), ta)
}

#[test]
fn vi_unbound_normal_key_emits_a_hint() {
    // #530: an unbound NORMAL key gives feedback instead of silently
    // swallowing the keypress.
    let mut ed = vi_editor();
    let mut ta = new_textarea(Edit::Vi);
    type_chars(&mut ed, &mut ta, "hi");
    ed.input(special(KeyCode::Esc), &mut ta); // → NORMAL
    let _ = ed.take_msg(); // drain anything prior
    ed.input(key('q'), &mut ta); // unbound in NORMAL
    let msg = ed
        .take_msg()
        .expect("an unbound NORMAL key should surface a hint");
    assert!(msg.contains("insert"), "hint nudges toward insert: {msg:?}");
    assert_eq!(ta.lines(), &["hi"], "`q` still types nothing in NORMAL");
}

#[test]
fn vi_w_submits_like_enter() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "hello");
    // `:w` = write = submit, no confirm.
    assert_eq!(run_ex(&mut ed, &mut ta, "w"), Step::Submit);
    assert!(ed.confirm_prompt().is_none());
}

#[test]
fn vi_wq_confirms_then_y_submit_quits() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "ship it");
    // `:wq` arms a [y/N] confirm rather than submitting outright.
    assert_eq!(
        run_ex(&mut ed, &mut ta, "wq"),
        Step::Continue,
        ":wq must not submit until confirmed"
    );
    assert!(
        ed.confirm_prompt().is_some(),
        "the [y/N] question is showing"
    );
    // `y` commits → submit-then-end-and-quit.
    assert_eq!(ed.input(key('y'), &mut ta), Step::SubmitQuit);
    assert!(
        ed.confirm_prompt().is_none(),
        "confirm cleared after answer"
    );
}

#[test]
fn vi_wq_confirm_cancels_on_n_or_enter() {
    for answer in [KeyCode::Char('n'), KeyCode::Enter, KeyCode::Esc] {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "keep editing");
        assert_eq!(run_ex(&mut ed, &mut ta, "wq"), Step::Continue);
        // Anything but y/Y dumps the user back into editing — no submit.
        assert_eq!(
            ed.input(special(answer), &mut ta),
            Step::Continue,
            "{answer:?} cancels the confirm"
        );
        assert!(ed.confirm_prompt().is_none(), "confirm cleared on cancel");
        // The buffer survived the aborted quit.
        assert_eq!(ta.lines(), &["keep editing".to_string()]);
    }
}

#[test]
fn vi_wq_bang_forces_without_confirm() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "no prompt");
    // The `!` form means "I'm sure" — straight to SubmitQuit.
    assert_eq!(run_ex(&mut ed, &mut ta, "wq!"), Step::SubmitQuit);
    assert!(ed.confirm_prompt().is_none());
}

#[test]
fn vi_q_quits_without_sending() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "discard me");
    assert_eq!(run_ex(&mut ed, &mut ta, "q"), Step::Eof);
}

#[test]
fn nano_is_modeless_and_labeled() {
    let mut ed = nano_editor();
    let mut ta = TextArea::default();
    assert_eq!(ed.label(), "nano");
    // Modeless like emacs: typing inserts text, no NORMAL mode.
    type_chars(&mut ed, &mut ta, "plain text");
    assert_eq!(ed.label(), "nano", "no mode flip");
    assert_eq!(ta.lines(), &["plain text".to_string()]);
    // Enter still submits; Ctrl-O still newlines (shared handling).
    assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
    assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);
    assert!(Edit::Nano.is_modeless() && Edit::Emacs.is_modeless());
    assert!(!Edit::Vi.is_modeless());
}

#[test]
fn emacs_enter_submits_and_ctrl_o_inserts_newline() {
    let mut ed = emacs_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "hello");
    // Ctrl-O adds a line without submitting.
    assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
    type_chars(&mut ed, &mut ta, "world");
    assert_eq!(ta.lines().len(), 2, "two lines after Ctrl-O");
    // Plain Enter submits.
    assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);
}

#[test]
fn shift_enter_inserts_newline_without_submitting() {
    let nl = || KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
    let mut ed = emacs_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "line one");
    assert_eq!(
        ed.input(nl(), &mut ta),
        Step::Continue,
        "Shift-Enter newline"
    );
    type_chars(&mut ed, &mut ta, "line two");
    assert_eq!(ta.lines().len(), 2, "Shift-Enter added a line");

    // Same in vi INSERT mode (shared handling runs before mode dispatch).
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "vi line");
    assert_eq!(ed.input(nl(), &mut ta), Step::Continue);
    assert_eq!(ta.lines().len(), 2, "Shift-Enter newline in vi too");

    // Ctrl-Enter is NOT bound (macOS intercepts it at the terminal layer);
    // a plain Enter with no continuation still submits, unaffected.
    let mut ed = emacs_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "x");
    assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);
}

#[test]
fn ctrl_o_is_newline_in_modeless_but_reserved_in_vi() {
    // Emacs / nano: Ctrl-O inserts a newline (idiomatic open-line).
    for ed_factory in [emacs_editor as fn() -> Editor, nano_editor] {
        let mut ed = ed_factory();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "a");
        assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
        assert_eq!(ta.lines().len(), 2, "Ctrl-O newline in modeless mode");
    }
    // Vi: Ctrl-O is reserved (jumplist / insert-normal) — it must NOT insert
    // a newline. In INSERT it is currently a no-op (a documented gap), so the
    // buffer stays a single line; vi users open lines with `o`/`O`.
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "vi");
    assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
    assert_eq!(ta.lines().len(), 1, "Ctrl-O does NOT newline in vi");
}

#[test]
fn enter_continues_an_open_bang_line() {
    let mut ed = emacs_editor();
    let mut ta = TextArea::default();
    // A `! …\` host-shell line is mid-continuation → Enter adds a line.
    type_chars(&mut ed, &mut ta, "! ls \\");
    assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Continue);
    assert_eq!(ta.lines().len(), 2, "Enter continued the bang line");
}

#[test]
fn ctrl_c_abandons_line_and_ctrl_d_empty_is_eof() {
    let mut ed = emacs_editor();
    let mut ta = TextArea::default();
    // Ctrl-C abandons the current line (clears it) and stays in the session.
    type_chars(&mut ed, &mut ta, "throwaway");
    assert_eq!(
        ed.input(ctrl('c'), &mut ta),
        Step::Continue,
        "Ctrl-C does not exit"
    );
    assert!(buffer_is_empty(&ta), "Ctrl-C cleared the buffer");
    // Ctrl-D on the now-empty buffer is EOF (exit).
    assert_eq!(
        ed.input(ctrl('d'), &mut ta),
        Step::Eof,
        "Ctrl-D empty → EOF"
    );
    // Ctrl-D with content submits instead.
    type_chars(&mut ed, &mut ta, "x");
    assert_eq!(ed.input(ctrl('d'), &mut ta), Step::Submit);
}

#[test]
fn vi_ex_wq_submits_and_q_is_eof() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "payload");
    ed.input(special(KeyCode::Esc), &mut ta);
    // `:wq` arms the send-then-end-and-quit confirm (see the dedicated
    // confirm tests); it does NOT submit outright.
    ed.input(key(':'), &mut ta);
    assert_eq!(ed.ex(), Some(""), "ex line is active");
    ed.input(key('w'), &mut ta);
    ed.input(key('q'), &mut ta);
    assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Continue);
    assert!(ed.confirm_prompt().is_some());

    // `:q` → EOF.
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    ed.input(special(KeyCode::Esc), &mut ta);
    ed.input(key(':'), &mut ta);
    ed.input(key('q'), &mut ta);
    assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Eof);
}

#[test]
fn mode_idiomatic_exit_keys() {
    // emacs: C-x C-c quits.
    let mut ed = emacs_editor();
    let mut ta = TextArea::default();
    assert_eq!(
        ed.input(ctrl('x'), &mut ta),
        Step::Continue,
        "C-x arms prefix"
    );
    assert_eq!(ed.input(ctrl('c'), &mut ta), Step::Eof, "C-x C-c → exit");
    // emacs: C-x then a non-C-c key cancels the prefix (no exit); a bare
    // Ctrl-C afterwards abandons the line (not exit), not part of a sequence.
    let mut ed = emacs_editor();
    let mut ta = TextArea::default();
    ed.input(ctrl('x'), &mut ta);
    type_chars(&mut ed, &mut ta, "a"); // cancels the prefix, inserts 'a'
    assert_eq!(ta.lines(), &["a".to_string()]);
    assert_eq!(
        ed.input(ctrl('c'), &mut ta),
        Step::Continue,
        "bare C-c abandons the line, does not exit"
    );
    assert!(buffer_is_empty(&ta), "bare C-c cleared the line");
    // nano: ^X exits directly.
    let mut ed = nano_editor();
    let mut ta = TextArea::default();
    assert_eq!(ed.input(ctrl('x'), &mut ta), Step::Eof, "nano ^X → exit");
    // vi: C-x is not an exit key (uses :q); it does nothing special here.
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    assert_eq!(
        ed.input(ctrl('x'), &mut ta),
        Step::Continue,
        "vi C-x → no exit"
    );
}

#[test]
fn mode_idiomatic_help_keys_queue_a_cheatsheet() {
    // nano: Ctrl-G.
    let mut ed = nano_editor();
    let mut ta = TextArea::default();
    assert_eq!(ed.input(ctrl('g'), &mut ta), Step::Continue);
    assert!(ed.take_msg().unwrap().starts_with("nano"));
    // emacs: Ctrl-h.
    let mut ed = emacs_editor();
    let mut ta = TextArea::default();
    assert_eq!(ed.input(ctrl('h'), &mut ta), Step::Continue);
    assert!(ed.take_msg().unwrap().starts_with("emacs"));
    // vi: `:help`.
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    ed.input(special(KeyCode::Esc), &mut ta);
    ed.input(key(':'), &mut ta);
    for c in "help".chars() {
        ed.input(key(c), &mut ta);
    }
    ed.input(special(KeyCode::Enter), &mut ta);
    assert!(ed.take_msg().unwrap().starts_with("vi"));
    // The help key in the wrong mode does nothing special: Ctrl-G in emacs
    // is not help (no message queued).
    let mut ed = emacs_editor();
    let mut ta = TextArea::default();
    ed.input(ctrl('g'), &mut ta);
    assert!(ed.take_msg().is_none());
}

#[test]
fn vi_colon_jumps_queues_a_note() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    ed.input(special(KeyCode::Esc), &mut ta);
    ed.input(key(':'), &mut ta);
    for c in "jumps".chars() {
        ed.input(key(c), &mut ta);
    }
    ed.input(special(KeyCode::Enter), &mut ta);
    assert!(ed.take_msg().is_some(), ":jumps queued a scrollback note");
    assert!(ed.take_msg().is_none(), "note is one-shot");
}

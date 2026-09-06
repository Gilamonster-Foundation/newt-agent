use super::*;
use test_support::{ctrl, key, special, type_chars, vi_editor};

// ── #1669 16.3: vim tab motions ────────────────────────────────────────

/// Drive chars from NORMAL and return the last `Step`.
///
/// The leading Esc is not decoration: `Editor::new(Edit::Vi)` starts in
/// INSERT, so a helper that skips it tests nothing but typing.
fn normal_keys(ed: &mut Editor, ta: &mut TextArea, keys: &str) -> Step {
    ed.input(special(KeyCode::Esc), ta); // INSERT → NORMAL
    let mut last = Step::Continue;
    for c in keys.chars() {
        last = ed.input(key(c), ta);
    }
    last
}

/// `gt` with no count is "next tab", not "go to tab 1".
///
/// The distinction is the whole reason the count is read as `0 == absent`
/// rather than through `take_count()`, which floors at 1.
#[test]
fn bare_gt_is_next_not_goto_one() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    assert_eq!(
        normal_keys(&mut ed, &mut ta, "gt"),
        Step::Tab(crate::tabs::TabAction::Next)
    );
}

/// **`{count}gt` is ABSOLUTE.** `2gt` is "go to tab 2", not "two tabs
/// forward" — unusual for a vi count, correct for vim, and exactly the
/// kind of thing a later refactor would "fix" into a relative motion.
#[test]
fn a_counted_gt_goes_to_that_tab_absolutely() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    assert_eq!(
        normal_keys(&mut ed, &mut ta, "2gt"),
        Step::Tab(crate::tabs::TabAction::Goto(2))
    );
    // Multi-digit counts accumulate the same way every other vi count does.
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    assert_eq!(
        normal_keys(&mut ed, &mut ta, "12gt"),
        Step::Tab(crate::tabs::TabAction::Goto(12))
    );
}

/// `gT` is relative in BOTH forms — bare is one back, counted is n back.
/// Deliberately different from `gt`, matching vim.
#[test]
fn gt_capital_is_relative_in_both_forms() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    assert_eq!(
        normal_keys(&mut ed, &mut ta, "gT"),
        Step::Tab(crate::tabs::TabAction::Prev(1))
    );
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    assert_eq!(
        normal_keys(&mut ed, &mut ta, "3gT"),
        Step::Tab(crate::tabs::TabAction::Prev(3))
    );
}

/// The regression that matters most: `gg` still goes to the top.
///
/// `g` is now a live prefix for three things, and `gg` is a hot key. If
/// adding the tab motions had made `gg` return a `Step::Tab`, or consumed
/// its count, the damage would be silent and constant.
#[test]
fn gg_still_jumps_to_the_top_and_is_not_a_tab_motion() {
    let mut ed = vi_editor();
    let mut ta = textarea_with(Edit::Vi, "one\ntwo\nthree");
    assert_eq!(
        normal_keys(&mut ed, &mut ta, "gg"),
        Step::Continue,
        "gg is a cursor jump, never a tab action"
    );
    assert_eq!(ta.cursor().0, 0, "cursor is on the first line");
}

/// An unknown `g`-suffix is swallowed, as before — it must not leak a tab
/// action or leave the count armed for the next keystroke.
#[test]
fn an_unknown_g_suffix_stays_inert() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    assert_eq!(normal_keys(&mut ed, &mut ta, "gz"), Step::Continue);
    // The count from a previous attempt must not survive into the next.
    assert_eq!(
        normal_keys(&mut ed, &mut ta, "gt"),
        Step::Tab(crate::tabs::TabAction::Next),
        "a stale count would have made this a Goto"
    );
}

/// Tab motions are NORMAL-mode only: typing `gt` while inserting is text.
///
/// A vi user types `g` and `t` constantly. If the tab motion fired from
/// INSERT, every word containing "gt" would fling the operator into
/// another agent's tab mid-sentence.
#[test]
fn gt_in_insert_mode_is_just_text() {
    let mut ed = vi_editor(); // starts in INSERT
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "gt");
    assert_eq!(
        ta.lines(),
        &["gt".to_string()],
        "insert mode types the characters"
    );
}

#[test]
fn vi_o_opens_line_below_and_enters_insert() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    // Start in INSERT (vi default), type, Esc to NORMAL.
    type_chars(&mut ed, &mut ta, "first");
    ed.input(special(KeyCode::Esc), &mut ta);
    assert_eq!(ed.label(), "vi N");
    // `o` opens a line below and returns to INSERT.
    ed.input(key('o'), &mut ta);
    assert_eq!(ed.label(), "vi I");
    type_chars(&mut ed, &mut ta, "second");
    assert_eq!(ta.lines(), &["first".to_string(), "second".to_string()]);
}

#[test]
fn vi_uppercase_o_opens_line_above() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "below");
    ed.input(special(KeyCode::Esc), &mut ta);
    ed.input(key('O'), &mut ta);
    type_chars(&mut ed, &mut ta, "above");
    assert_eq!(ta.lines(), &["above".to_string(), "below".to_string()]);
}

#[test]
fn vi_dd_deletes_the_line() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "doomed");
    ed.input(special(KeyCode::Esc), &mut ta);
    ed.input(key('d'), &mut ta);
    ed.input(key('d'), &mut ta);
    assert_eq!(ta.lines(), &[String::new()], "dd cleared the only line");
}

#[test]
fn vi_x_with_count_deletes_n_chars() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "abcdef");
    ed.input(special(KeyCode::Esc), &mut ta); // NORMAL, cursor on 'f'
    ed.input(key('0'), &mut ta); // head
    type_chars(&mut ed, &mut ta, "3x"); // delete 3 chars
    assert_eq!(ta.lines(), &["def".to_string()]);
}

/// Build a multi-line buffer in vi: type lines separated by Shift-Enter
/// (which inserts a newline in every mode), then Esc to NORMAL at the top.
fn vi_buffer(lines: &[&str]) -> (Editor, TextArea<'static>) {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            ed.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &mut ta);
        }
        type_chars(&mut ed, &mut ta, l);
    }
    ed.input(special(KeyCode::Esc), &mut ta); // NORMAL
    ed.input(key('g'), &mut ta);
    ed.input(key('g'), &mut ta); // top
    (ed, ta)
}

#[test]
fn vi_uppercase_j_joins_line_below() {
    let (mut ed, mut ta) = vi_buffer(&["foo", "bar"]);
    ed.input(key('J'), &mut ta);
    assert_eq!(ta.lines(), &["foo bar".to_string()], "J joins with a space");
    // J on the only remaining line is a no-op (nothing below).
    ed.input(key('J'), &mut ta);
    assert_eq!(ta.lines(), &["foo bar".to_string()]);
}

#[test]
fn vi_count_j_joins_multiple_lines() {
    let (mut ed, mut ta) = vi_buffer(&["a", "b", "c"]);
    // 3J joins this line + 2 below → one line.
    ed.input(key('3'), &mut ta);
    ed.input(key('J'), &mut ta);
    assert_eq!(ta.lines(), &["a b c".to_string()]);
}

#[test]
fn vi_insert_normal_ctrl_o_runs_one_command() {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, "hello"); // INSERT, cursor at end
                                           // i_CTRL-O: one Normal command (`0` → head) then back to INSERT.
    ed.input(ctrl('o'), &mut ta);
    assert_eq!(ed.label(), "vi N", "Ctrl-O drops to NORMAL");
    ed.input(key('0'), &mut ta);
    assert_eq!(ed.label(), "vi I", "resumes INSERT after one command");
    type_chars(&mut ed, &mut ta, "X");
    assert_eq!(ta.lines(), &["Xhello".to_string()], "inserted at head");
}

#[test]
fn vi_esc_cancels_incomplete_command() {
    // A pending operator is cancelled by Esc — the next key is a fresh
    // command, not the operator's motion.
    let (mut ed, mut ta) = vi_line("hello world");
    ed.input(key('d'), &mut ta); // pending d
    ed.input(special(KeyCode::Esc), &mut ta); // cancel
    ed.input(key('w'), &mut ta); // plain motion now, not `dw`
    assert_eq!(
        ta.lines(),
        &["hello world".to_string()],
        "Esc cancelled the d operator"
    );

    // A building count is cancelled by Esc.
    let (mut ed, mut ta) = vi_line("abcdef");
    ed.input(key('3'), &mut ta); // count = 3
    ed.input(special(KeyCode::Esc), &mut ta); // cancel count
    ed.input(key('x'), &mut ta); // deletes 1, not 3
    assert_eq!(
        ta.lines(),
        &["bcdef".to_string()],
        "Esc cancelled the count"
    );
}

#[test]
fn vi_jumplist_back_and_forward() {
    let (mut ed, mut ta) = vi_buffer(&["one", "two", "three"]);
    // We're at the top (gg recorded a jump from the bottom line).
    assert_eq!(ta.cursor().0, 0, "gg → row 0");
    // Ctrl-O jumps back to the pre-gg position (the last line).
    ed.input(ctrl('o'), &mut ta);
    assert_eq!(ta.cursor().0, 2, "Ctrl-O → back to row 2");
    // Ctrl-I (Tab) jumps forward again to the top.
    ed.input(special(KeyCode::Tab), &mut ta);
    assert_eq!(ta.cursor().0, 0, "Ctrl-I → forward to row 0");
}

/// A single-line vi buffer at NORMAL, cursor at head.
fn vi_line(s: &str) -> (Editor, TextArea<'static>) {
    let mut ed = vi_editor();
    let mut ta = TextArea::default();
    type_chars(&mut ed, &mut ta, s);
    ed.input(special(KeyCode::Esc), &mut ta);
    ed.input(key('0'), &mut ta); // head
    (ed, ta)
}

#[test]
fn vi_f_and_semicolon_and_comma_char_search() {
    let (mut ed, mut ta) = vi_line("a.b.c.d");
    // f. → first dot (col 1).
    ed.input(key('f'), &mut ta);
    ed.input(key('.'), &mut ta);
    assert_eq!(ta.cursor().1, 1, "f. → first dot");
    // ; → next dot (col 3).
    ed.input(key(';'), &mut ta);
    assert_eq!(ta.cursor().1, 3, "; → next dot");
    // , → previous dot (col 1).
    ed.input(key(','), &mut ta);
    assert_eq!(ta.cursor().1, 1, ", → previous dot");
}

#[test]
fn vi_t_and_capital_f_char_search() {
    let (mut ed, mut ta) = vi_line("abcXdef");
    // tX → just before X (col 2).
    ed.input(key('t'), &mut ta);
    ed.input(key('X'), &mut ta);
    assert_eq!(ta.cursor().1, 2, "tX → col before X");
    // Move to end, then FX → back onto X (col 3).
    ed.input(key('$'), &mut ta);
    ed.input(key('F'), &mut ta);
    ed.input(key('X'), &mut ta);
    assert_eq!(ta.cursor().1, 3, "FX → onto X");
}

#[test]
fn vi_operator_motions_dw_d_dollar_d0_yy() {
    // dw deletes to the start of the next word.
    let (mut ed, mut ta) = vi_line("foo bar baz");
    ed.input(key('d'), &mut ta);
    ed.input(key('w'), &mut ta);
    assert_eq!(ta.lines(), &["bar baz".to_string()], "dw");

    // d$ deletes to end of line.
    let (mut ed, mut ta) = vi_line("keep DROP this");
    ed.input(key('f'), &mut ta);
    ed.input(key('D'), &mut ta); // f D → cursor on the 'D' of DROP
    ed.input(key('d'), &mut ta);
    ed.input(key('$'), &mut ta);
    assert_eq!(ta.lines(), &["keep ".to_string()], "d$");

    // d0 deletes from the cursor back to the beginning of the line.
    let (mut ed, mut ta) = vi_line("alpha beta");
    ed.input(key('f'), &mut ta);
    ed.input(key('b'), &mut ta); // f b → col 6 ('b' of "beta")
    ed.input(key('d'), &mut ta);
    ed.input(key('0'), &mut ta);
    assert_eq!(ta.lines(), &["beta".to_string()], "d0 deletes to BOL");

    // yy then p duplicates the line.
    let (mut ed, mut ta) = vi_line("dup");
    ed.input(key('y'), &mut ta);
    ed.input(key('y'), &mut ta);
    ed.input(key('p'), &mut ta);
    assert!(
        ta.lines().iter().filter(|l| l.contains("dup")).count() >= 1,
        "yy+p yanks and pastes the line"
    );
}

use super::*;
use crossterm::event::Event;
use test_support::{
    ctrl, emacs_editor, key, line_text, nano_editor, special, type_chars, vi_editor, RecordingSink,
};

// ── #2006: vi state is SESSION state, not per-line state ───────────────

/// Drive one key into a mounted editor, discarding the scrollback it emits.
fn mounted_key(mounted: &mut MountedEditor, sink: &mut RecordingSink, key: KeyEvent) {
    mounted.on_event(Event::Key(key), sink).unwrap();
}

/// Drive a run of plain chars into a mounted editor.
fn mounted_chars(mounted: &mut MountedEditor, sink: &mut RecordingSink, s: &str) {
    for c in s.chars() {
        mounted_key(mounted, sink, key(c));
    }
}

fn sink_text(sink: &RecordingSink) -> String {
    sink.batches
        .iter()
        .flatten()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn vi_mode_survives_a_submit() {
    // #2006: Enter sends a line; it does not put the operator back in
    // INSERT behind their back.
    let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hi");
    let mut sink = RecordingSink::default();
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
    assert_eq!(mounted.editor.label(), "vi N");
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter)); // submit
    assert_eq!(
        mounted.editor.label(),
        "vi N",
        "the mode the operator chose outlives the line they sent"
    );
}

#[test]
fn vi_jumplist_survives_a_submit() {
    // #2006: `Editor::new` threw the jumplist away with the mode.
    let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "a\nb\nc");
    let mut sink = RecordingSink::default();
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
    mounted_chars(&mut mounted, &mut sink, "gg"); // records a jump origin
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter)); // submit
    sink.batches.clear();
    mounted_chars(&mut mounted, &mut sink, ":jumps");
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter));
    let text = sink_text(&sink);
    assert!(text.contains("jumps  back:"), ":jumps reported: {text:?}");
    assert!(
        !text.contains("back: —"),
        "the jump recorded before the submit is still there: {text:?}"
    );
}

#[test]
fn vi_last_find_survives_a_submit() {
    // #2006: `;` repeats the last `f`/`t` — across a submit too.
    let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hello world");
    let mut sink = RecordingSink::default();
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
    mounted_chars(&mut mounted, &mut sink, "0fw"); // find 'w'
    assert_eq!(mounted.textarea.cursor().1, 6, "`fw` landed on the 'w'");
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter)); // submit
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // NORMAL either way
    mounted_chars(&mut mounted, &mut sink, "i"); // → INSERT
    mounted_chars(&mut mounted, &mut sink, "hello world");
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
    mounted_chars(&mut mounted, &mut sink, "0;"); // repeat the find
    assert_eq!(
        mounted.textarea.cursor().1,
        6,
        "`;` still knows what `f` was looking for"
    );
}

#[test]
fn vi_pending_sequence_does_not_survive_a_submit() {
    // The other half of #2006's decision: mode/jumplist/last_find are
    // session state, but a half-typed `f`/`d`/count belongs to the line
    // that was just sent. An `f` left armed would eat the next `i`.
    let mut mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hi");
    let mut sink = RecordingSink::default();
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Esc)); // → NORMAL
    mounted_chars(&mut mounted, &mut sink, "f"); // awaiting a search target
    mounted_key(&mut mounted, &mut sink, special(KeyCode::Enter)); // submit
    mounted_chars(&mut mounted, &mut sink, "iabc");
    assert_eq!(
        mounted.textarea.lines(),
        ["abc"],
        "`i` opened INSERT; it was not swallowed as a stale search target"
    );
}

#[test]
fn vi_ctrl_c_at_an_idle_prompt_keeps_the_mode() {
    // #2006: `self.vi = Vi::new()` flipped a NORMAL operator into INSERT.
    // Real vim's `i_CTRL-C` is insert→normal; it is never normal→insert.
    let mut ed = vi_editor();
    let mut ta = new_textarea(Edit::Vi);
    type_chars(&mut ed, &mut ta, "hi");
    ed.input(special(KeyCode::Esc), &mut ta); // → NORMAL
    ed.input(ctrl('c'), &mut ta);
    assert_eq!(ta.lines(), [""], "Ctrl-C still clears the draft");
    assert_eq!(ed.label(), "vi N", "…and leaves the mode alone");
}

#[test]
fn vi_ctrl_c_still_cancels_a_pending_sequence() {
    // Guards the replacement for the deleted `Vi::new()`: the draft is
    // gone, so an operator/search pending against it must go too.
    let mut ed = vi_editor();
    let mut ta = new_textarea(Edit::Vi);
    type_chars(&mut ed, &mut ta, "hi");
    ed.input(special(KeyCode::Esc), &mut ta); // → NORMAL
    ed.input(key('f'), &mut ta); // awaiting a search target
    ed.input(ctrl('c'), &mut ta);
    type_chars(&mut ed, &mut ta, "iabc");
    assert_eq!(
        ta.lines(),
        ["abc"],
        "`i` opened INSERT; the pending `f` did not eat it"
    );
}

#[test]
fn vi_a_fresh_mount_still_starts_in_insert() {
    // The twin of the tests above: "persists" must not become "always
    // NORMAL". A session that has never pressed Esc opens in INSERT.
    let mounted = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "");
    assert_eq!(mounted.editor.label(), "vi I");
}

#[test]
fn vi_state_hands_off_across_a_remount() {
    // The seam the classic per-read driver and `SurfaceRequest::Reload`
    // use: a rebuilt mount adopts the outgoing one's vi state.
    let mut old = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hello world");
    let mut sink = RecordingSink::default();
    mounted_key(&mut old, &mut sink, special(KeyCode::Esc)); // → NORMAL
    mounted_chars(&mut old, &mut sink, "0fw"); // find 'w'

    let mut new = MountedEditor::new(Edit::Vi, Some(1), Vec::new(), "hello world");
    new.adopt_vi(old.take_vi());
    assert_eq!(new.editor.label(), "vi N", "the mode came across");
    mounted_chars(&mut new, &mut sink, "0;");
    assert_eq!(
        new.textarea.cursor().1,
        6,
        "…and so did the `;` repeat target"
    );
    assert_eq!(old.editor.label(), "vi I", "the outgoing mount is spent");
}

#[test]
fn mode_hint_promises_an_interrupt_only_while_a_turn_runs() {
    // Contract doc §3 item 4: at an idle prompt Ctrl-C clears the draft,
    // it does not interrupt anything. The affordance and the behavior
    // share one condition.
    for editor in [vi_editor(), emacs_editor(), nano_editor()] {
        let running = editor.mode_hint(true);
        let idle = editor.mode_hint(false);
        assert!(
            running.contains("^C interrupt"),
            "a running turn advertises the interrupt: {running:?}"
        );
        assert!(
            idle.contains("^C clear") && !idle.contains("interrupt"),
            "an idle prompt advertises what Ctrl-C actually does: {idle:?}"
        );
    }
    let mut normal = vi_editor();
    let mut ta = new_textarea(Edit::Vi);
    normal.input(special(KeyCode::Esc), &mut ta);
    assert!(normal.mode_hint(false).contains("^C clear"));
}

/// #2010: the `^D` half is idle-only, by the same rule as `^C`. During a
/// turn the session is not reading, so Ctrl-D exits nothing — a hint
/// that promised `^D exit` there was the invisible behaviour the
/// operator reported.
#[test]
fn mode_hint_promises_an_exit_only_while_idle() {
    for editor in [vi_editor(), emacs_editor(), nano_editor()] {
        let idle = editor.mode_hint(false);
        let running = editor.mode_hint(true);
        assert!(
            idle.contains("^D exit"),
            "an idle prompt advertises the exit: {idle:?}"
        );
        assert!(
            !running.contains("^D"),
            "a running turn must not promise an exit it cannot take: {running:?}"
        );
    }
}

/// #2010: Ctrl-D while a turn runs is acknowledged AT PRESS TIME — a
/// scrollback note saying where exit lives — and is NOT an `Eof` for the
/// presenter to drop on the floor. Idle, the same key is the EOF it
/// always was. (Whether a mid-turn Ctrl-D should escalate to an
/// interrupt is the operator's call; this pins only that it is heard.)
#[test]
fn ctrl_d_during_a_turn_is_acknowledged_not_dropped() {
    let mut mounted = MountedEditor::new(Edit::Nano, Some(1), Vec::new(), "");
    let mut sink = RecordingSink::default();
    // The field, not `set_turn_running`: that setter is unix-only (its
    // one caller is the cockpit), and this rule holds on every platform.
    mounted.turn_running = true;
    let outcome = mounted.on_event(Event::Key(ctrl('d')), &mut sink).unwrap();
    assert_eq!(outcome, None, "mid-turn Ctrl-D is not an EOF");
    let notes: Vec<String> = sink.batches.iter().flatten().map(line_text).collect();
    assert!(
        notes.iter().any(|l| l.contains("Ctrl-C interrupts")),
        "the press is answered with where exit and interrupt live: {notes:?}"
    );

    mounted.turn_running = false;
    let outcome = mounted.on_event(Event::Key(ctrl('d')), &mut sink).unwrap();
    assert_eq!(
        outcome,
        Some(EditorOutcome::Eof),
        "idle Ctrl-D is still EOF"
    );
}

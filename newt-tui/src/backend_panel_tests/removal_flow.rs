use super::*;

/// **`d` asks before it deletes.**
///
/// It used to prefill the ex-command `d <name>` and let Enter run it —
/// a confirmation only in the sense that a keystroke stood between you and
/// the deletion. It named no consequence, and the row it would remove was
/// already under the cursor, so the prefill read as a label rather than a
/// question.
#[test]
fn d_asks_before_deleting_and_names_what_is_lost() {
    let mut s = panel();
    s.cycle(1); // gpu-runner
    s.begin_remove();

    let Mode::Confirm(confirm) = &s.mode else {
        panic!("d must open a confirmation, got {:?}", s.mode);
    };
    assert_eq!(
        confirm.action,
        Pending::RemoveBackend("gpu-runner".to_string())
    );
    assert!(confirm.prompt.contains("gpu-runner"), "{}", confirm.prompt);
    assert!(
        confirm.prompt.contains("cannot be undone"),
        "the question states the consequence: {}",
        confirm.prompt
    );
    assert!(
        confirm.prompt.contains("[y/N]"),
        "and states that the default is no: {}",
        confirm.prompt
    );

    // On a kind fallback there is nothing to remove, and nothing is asked.
    let mut k = panel();
    k.cycle(1);
    k.cycle(1);
    k.cycle(1); // ollama
    k.begin_remove();
    assert!(matches!(k.mode, Mode::Choose));
    assert!(k.status.as_deref().unwrap().contains("named backend"));
}

/// **The default is no**, and every key that is not `y` takes it —
/// including Enter, which is the one most likely to be pressed by reflex.
#[test]
fn anything_but_y_declines_and_deletes_nothing() {
    for answer in [false, true] {
        let mut s = panel();
        s.cycle(1);
        s.begin_remove();
        let mut removed: Vec<String> = Vec::new();
        let mut remove = |name: &str| -> Result<String, String> {
            removed.push(name.to_string());
            Ok(format!("removed {name}"))
        };
        s.answer_confirm(answer, &mut remove);
        let removed = removed;

        assert!(
            matches!(s.mode, Mode::Choose),
            "the question closes either way"
        );
        if answer {
            assert_eq!(removed, ["gpu-runner"], "y performs the delete");
        } else {
            assert!(removed.is_empty(), "no answer but y may delete");
            assert!(
                s.status.as_deref().unwrap().contains("nothing was deleted"),
                "and declining says so: {:?}",
                s.status
            );
        }
    }
}

#[test]
fn remove_nonactive_deletes_via_injected_closure_and_stays_open() {
    let mut s = panel();
    let mut removed: Vec<String> = Vec::new();
    let mut remove = |name: &str| {
        removed.push(name.to_string());
        Ok(format!("removed backend '{name}'"))
    };
    s.begin_command("d gpu-runner");
    assert_eq!(s.run_command(&mut remove), None, "stays open");
    assert_eq!(removed, vec!["gpu-runner"]);
    assert!(s.named_index("gpu-runner").is_none(), "chooser entry gone");
    assert_eq!(s.changes, vec!["removed backend 'gpu-runner'"]);
    // Active marker (dgx1, index 0) survives the shift.
    assert!(s.pick_label().contains("dgx1") && s.pick_label().contains("(active)"));
}

#[test]
fn removing_the_entry_under_a_dirty_cursor_resets_the_pick() {
    let mut s = panel();
    s.cycle(1); // dial to gpu-runner (dirty)
    let mut remove = ok_remove();
    s.begin_command("d gpu-runner");
    assert_eq!(s.run_command(&mut remove), None);
    assert!(
        s.is_noop(),
        "the removed pick no longer exists — back to a clean spinner"
    );
    assert!(s.pick_label().contains("dgx1") && s.pick_label().contains("(active)"));
}

#[test]
fn remove_failure_keeps_the_option_and_shows_why() {
    let mut s = panel();
    let mut remove = |_: &str| Err("permission denied".to_string());
    s.begin_command("d gpu-runner");
    assert_eq!(s.run_command(&mut remove), None);
    assert!(s.named_index("gpu-runner").is_some(), "nothing dropped");
    assert!(s.changes.is_empty());
    assert!(s.status.as_deref().unwrap().contains("permission denied"));
}

#[test]
fn remove_active_with_a_dirty_named_selection_closes_as_one_transaction() {
    let mut s = panel();
    s.cycle(1); // dial to gpu-runner (a different NAMED backend)
    let mut called = false;
    let mut remove = |_: &str| {
        called = true;
        Ok(String::new())
    };
    s.begin_command("d dgx1");
    assert_eq!(
        s.run_command(&mut remove),
        Some(true),
        "closes applying the new selection"
    );
    assert!(
        !called,
        "the delete is deferred: the caller applies the switch FIRST, then removes"
    );
    // The REAL exit path carries both halves of the transaction.
    let close = close_outcome(true, &s);
    assert_eq!(
        close.apply,
        Some(BackendSelection::Named("gpu-runner".to_string()))
    );
    assert_eq!(close.remove_after_apply.as_deref(), Some("dgx1"));
}

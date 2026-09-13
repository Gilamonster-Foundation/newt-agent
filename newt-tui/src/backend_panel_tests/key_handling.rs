use super::*;

/// An unmapped terminal key still reaches the default-no confirmation arm.
/// Dropping it in the host decoder would leave the deletion pending.
#[test]
fn unmapped_keys_decline_a_pending_removal_without_running_writers() {
    for key in [Key::Other, Key::Enter, Key::Esc, Key::Ctrl('y')] {
        let mut state = panel();
        state.cycle(1);
        state.begin_remove();
        assert!(state.in_confirm());
        let mut screen = BackendScreen {
            state,
            persist: |_: &BackendEdit| panic!("declining must not save"),
            remove: |_: &str| panic!("declining must not delete"),
        };

        assert_eq!(
            crate::panel::Screen::key(&mut screen, key),
            crate::panel::Flow::Stay
        );
        assert!(!screen.state.in_confirm());
        assert_eq!(
            screen.state.status.as_deref(),
            Some("cancelled — nothing was deleted")
        );
        assert!(screen.state.named_index("gpu-runner").is_some());
    }
}

/// Ctrl-E is not `e`. The chooser's action keys were unguarded, so a
/// control chord opened the edit form — while its command and form arms
/// WERE guarded. That divergence inside one file is what the folded key
/// vocabulary ends.
#[test]
fn a_control_chord_does_not_trigger_the_action_keys() {
    for chord in [Key::Ctrl('e'), Key::Ctrl('a'), Key::Ctrl('d')] {
        let mut screen = BackendScreen {
            state: panel(),
            persist: ok_persist(),
            remove: ok_remove(),
        };
        assert_eq!(
            crate::panel::Screen::key(&mut screen, chord),
            crate::panel::Flow::Stay
        );
        assert!(
            !screen.state.in_form() && !screen.state.in_command(),
            "{chord:?} must not open a mode"
        );
    }
}

#[test]
fn ex_commands_validate_visibly() {
    let mut s = panel();
    let mut remove = ok_remove();
    for (cmd, want) in [
        ("d", "needs a name"),
        ("d ghost", "no configured backend"),
        ("d relic", "inline"),
        ("banana", "unknown command"),
    ] {
        s.begin_command(cmd);
        assert_eq!(s.run_command(&mut remove), None, "{cmd:?} stays open");
        assert!(
            s.status.as_deref().unwrap().contains(want),
            "{cmd:?} → {:?}",
            s.status
        );
    }
    s.begin_command("q");
    assert_eq!(s.run_command(&mut remove), Some(false), ":q cancels");
}

#[test]
fn arrows_open_editor_discover_models_and_choose_without_typing() {
    let mut screen = BackendScreen {
        state: panel(),
        persist: ok_persist(),
        remove: ok_remove(),
    };
    use crate::panel::{Flow, Screen};
    assert_eq!(screen.key(Key::Down), Flow::Stay);
    assert!(screen.state.in_form());
    screen.key(Key::Down); // kind
    screen.key(Key::Down); // URL
    assert_eq!(screen.key(Key::Down), Flow::Close(false)); // fetch outside driver
    let Mode::Form(form) = &mut screen.state.mode else {
        panic!("form")
    };
    form.models = Some(Ok(vec![
        ModelChoice {
            name: "qwen3:30b".into(),
            tag: "[loaded]".into(),
        },
        ModelChoice {
            name: "another-model".into(),
            tag: "[unloaded]".into(),
        },
    ]));
    screen.key(Key::Right);
    screen.key(Key::Char('x')); // model field is a dial
    screen.key(Key::Backspace);
    let Mode::Form(form) = &screen.state.mode else {
        panic!("form")
    };
    assert_eq!(form.model, "another-model");
    screen.key(Key::Left);
    let Mode::Form(form) = &screen.state.mode else {
        panic!("form")
    };
    assert_eq!(form.model, "qwen3:30b");
    screen.key(Key::Right);
    screen.key(Key::Enter);
    assert_eq!(
        screen.state.options[0].model.as_deref(),
        Some("another-model")
    );
}

#[test]
fn editing_endpoint_invalidates_discovered_models() {
    let mut s = panel();
    s.begin_edit();
    let Mode::Form(form) = &mut s.mode else {
        panic!("form")
    };
    form.models = Some(Ok(vec![]));
    form.sel = 2;
    s.form_input('/');
    let Mode::Form(form) = &s.mode else {
        panic!("form")
    };
    assert!(form.models.is_none());
}

#[test]
fn model_dial_can_restore_the_server_default() {
    let mut s = panel();
    s.begin_edit();
    let Mode::Form(form) = &mut s.mode else {
        panic!("form")
    };
    form.sel = 3;
    form.models = Some(Ok(vec![ModelChoice {
        name: "qwen3:30b".into(),
        tag: String::new(),
    }]));
    s.form_cycle(-1);
    let Mode::Form(form) = &s.mode else {
        panic!("form")
    };
    assert!(form.model.is_empty());
    assert_eq!(form_rows(form)[3].value, "(server default)");
    assert!(s.submit_form(&mut ok_persist()));
    assert_eq!(s.options[0].model, None);
}

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

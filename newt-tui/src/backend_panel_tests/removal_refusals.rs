use super::*;

/// **A confirm is never posed for a delete that would then be refused.**
///
/// Being asked "are you sure?" and then told it was never allowed teaches
/// the operator that the question is decoration, and the next one gets
/// answered without reading it.
#[test]
fn a_delete_that_would_be_refused_is_never_asked_about() {
    // The active backend cannot go without a replacement dialled first.
    let mut s = panel();
    s.begin_remove();
    assert!(
        matches!(s.mode, Mode::Choose),
        "no question for a refused delete, got {:?}",
        s.mode
    );
    let status = s.status.as_deref().unwrap_or_default();
    assert!(
        status.contains("active backend") || status.contains("default_backend"),
        "the refusal is shown instead: {status}"
    );
}

/// The refusal rules have ONE statement, asked twice: once to decide
/// whether to pose the question, once to enforce. A `:d <name>` typed
/// directly must still hit every rule the `d` key does.
#[test]
fn the_typed_command_enforces_the_same_refusals_the_confirm_checks() {
    let mut s = panel();
    let active = s.selected().selection.clone();
    let BackendSelection::Named(name) = active else {
        panic!("the fixture's first row is a named backend");
    };
    assert!(
        s.remove_refusal(&name).is_some(),
        "the active backend is refused"
    );

    let mut removed: Vec<String> = Vec::new();
    let mut remove = |n: &str| -> Result<String, String> {
        removed.push(n.to_string());
        Ok(String::new())
    };
    s.begin_command(&format!("d {name}"));
    s.run_command(&mut remove);
    let removed = removed;
    assert!(removed.is_empty(), "the typed path refuses it too");
}

/// The refusal that makes invariant 1 hold: a kind fallback cannot be
/// removed, so the list has a floor of two.
#[test]
fn a_kind_fallback_cannot_be_removed() {
    let mut state = panel();
    let mut remove = ok_remove();
    state.begin_command("");
    for c in "d ollama".chars() {
        state.command_char(c);
    }
    assert_eq!(state.run_command(&mut remove), None, "stays open");
    assert!(
        state.options.iter().any(|o| o.name == "ollama"),
        "the fallback survives the delete"
    );
    assert!(
        state.status.as_deref().is_some_and(|s| !s.is_empty()),
        "and the refusal is explained"
    );
}

#[test]
fn remove_refuses_the_active_backend_without_a_new_selection() {
    let mut s = panel();
    let called = std::cell::Cell::new(false);
    let mut remove = |_: &str| {
        called.set(true);
        Ok(String::new())
    };
    s.begin_command("d dgx1");
    assert_eq!(s.run_command(&mut remove), None, "refused, stays open");
    assert!(!called.get(), "the file is never touched");
    assert!(s.status.as_deref().unwrap().contains("active backend"));
    // A dirty pick on a KIND fallback is not a valid replacement either.
    s.cycle(1);
    s.cycle(1);
    s.cycle(1); // → ollama kind
    s.begin_command("d dgx1");
    assert_eq!(s.run_command(&mut remove), None);
    assert!(!called.get());
    assert!(s.pending_remove.is_none());
}

/// §2/§7/§11 REGRESSION: `:d <name>` on config.toml's `default_backend` is
/// refused unless the same transaction applies another named backend — a
/// dangling `default_backend` is a hard `UnknownNamed` error for
/// `newt solve` / the ACP worker, which have no settings.toml mask.
#[test]
fn remove_refuses_the_config_default_even_when_it_is_not_active() {
    // Active = gpu-runner (a NEWT_PROVIDER pin), default_backend = dgx1.
    let mut s = PanelState::new(PanelSeed {
        options: vec![
            named("dgx1", BackendSource::UserDropIn),
            named("gpu-runner", BackendSource::UserDropIn),
            BackendOption::kind_fallback("ollama"),
        ],
        active: Some(1),
        default_backend: Some("dgx1".to_string()),
    });
    let called = std::cell::Cell::new(false);
    let mut remove = |_: &str| {
        called.set(true);
        Ok(String::new())
    };
    s.begin_command("d dgx1");
    assert_eq!(s.run_command(&mut remove), None, "refused, stays open");
    assert!(!called.get(), "the file is never touched");
    assert!(
        s.status.as_deref().unwrap().contains("default_backend"),
        "{:?}",
        s.status
    );
    assert!(s.named_index("dgx1").is_some());
    // Dialing another NAMED backend makes it one transaction: the caller
    // applies gpu-runner, repoints default_backend at it, then deletes dgx1.
    s.cycle(-1); // → dgx1
    s.cycle(1); // → gpu-runner (dirty)
    s.begin_command("d dgx1");
    assert_eq!(s.run_command(&mut remove), Some(true));
    assert!(!called.get(), "the delete is deferred to the caller");
    let close = close_outcome(true, &s);
    assert_eq!(
        close.apply,
        Some(BackendSelection::Named("gpu-runner".to_string()))
    );
    assert_eq!(close.remove_after_apply.as_deref(), Some("dgx1"));
}

/// §3 REGRESSION: a name a PROJECT `.newt/backends` drop-in also defines
/// resolves to the project file (merge is last-wins), so editing or
/// removing the user drop-in from here would be a silent no-op and a
/// phantom delete. Both are refused, and the row says why.
#[test]
fn a_project_shadowed_backend_is_neither_editable_nor_removable() {
    let mut s = PanelState::new(PanelSeed {
        options: vec![
            named("dgx1", BackendSource::ShadowedByProject),
            named("gpu-runner", BackendSource::UserDropIn),
        ],
        active: Some(1),
        default_backend: None,
    });
    s.cycle(-1); // dial onto the shadowed row
    assert_eq!(
        s.chooser_rows()[0].provenance,
        "shadowed by project config",
        "the row admits the shadow"
    );
    s.begin_edit();
    assert_eq!(s.mode, Mode::Choose, "no form for a shadowed entry");
    assert!(s.status.as_deref().unwrap().contains("shadowed"));
    let called = std::cell::Cell::new(false);
    let mut remove = |_: &str| {
        called.set(true);
        Ok(String::new())
    };
    s.begin_command("d dgx1");
    assert_eq!(s.run_command(&mut remove), None);
    assert!(!called.get(), "no phantom delete");
    assert!(s.status.as_deref().unwrap().contains("shadowed"));
    assert!(s.named_index("dgx1").is_some(), "nothing dropped");
}

use super::*;

#[test]
fn untouched_spinner_is_a_noop_and_enter_closes_silently() {
    let s = panel();
    assert!(s.is_noop(), "fresh panel: nothing to apply");
    // Enter and Esc are indistinguishable on a noop visit.
    assert_eq!(close_outcome(true, &s), PanelClose::cancelled());
    assert_eq!(close_outcome(false, &s), PanelClose::cancelled());
}

#[test]
fn spinner_dials_named_then_kind_fallbacks_with_markers_and_clamp() {
    let mut s = panel();
    assert!(
        s.pick_label().contains("dgx1") && s.pick_label().contains("(active)"),
        "opens on the active backend: {}",
        s.pick_label()
    );
    s.cycle(1); // → gpu-runner
    assert!(!s.is_noop(), "a touched spinner is a pick");
    assert!(s.pick_label().contains("gpu-runner") && s.pick_label().contains("(pending)"));
    assert_eq!(
        close_outcome(true, &s).apply,
        Some(BackendSelection::Named("gpu-runner".to_string()))
    );
    // Through relic and onto the kind fallbacks…
    s.cycle(1);
    s.cycle(1);
    assert_eq!(
        close_outcome(true, &s).apply,
        Some(BackendSelection::Kind("ollama"))
    );
    // …and the right edge clamps (no wrap).
    s.cycle(1);
    s.cycle(1);
    s.cycle(1);
    assert_eq!(
        close_outcome(true, &s).apply,
        Some(BackendSelection::Kind("openai"))
    );
    // Cancelling a dirty spinner applies nothing.
    assert_eq!(close_outcome(false, &s), PanelClose::cancelled());
}

#[test]
fn touched_back_to_active_is_still_a_deliberate_reapply() {
    // Same semantics as the psyche model spinner (#1666): cycling away and
    // back is a deliberate same-value re-apply, not a noop.
    let mut s = panel();
    s.cycle(1);
    s.cycle(-1);
    assert!(!s.is_noop());
    assert_eq!(
        close_outcome(true, &s).apply,
        Some(BackendSelection::Named("dgx1".to_string()))
    );
}

/// **`selected()` indexes without a bounds check, and this is why that is
/// safe.**
///
/// `&self.options[self.pick.value()]` (`:466`) is reached from `draw` on
/// every repaint. Two invariants keep it in range, and neither was
/// asserted anywhere — they were true by accident of construction, which
/// is the state a helper is in right before someone changes it:
///
/// 1. **The option list is never empty.** `run` refuses an empty seed, and
///    the two kind fallbacks are `KindToggle`, which `editable()` excludes
///    and `remove_command` refuses — as is any `Inline` entry. So `len >= 2`
///    no matter how many drop-ins are deleted.
/// 2. **`remove_option` keeps the spinner in range**, repositioning it
///    when the removed row was at or before it.
///
/// Driven to exhaustion rather than argued: remove every removable option,
/// checking both invariants after each one.
#[test]
fn the_spinner_stays_in_range_however_many_options_are_removed() {
    let mut state = panel();
    let mut removals = 0;

    // Walk the spinner to the end first, so removals happen with the
    // cursor ABOVE the removed index — the arm that repositions.
    for _ in 0..state.options.len() {
        state.cycle(1);
    }

    while let Some(name) = state
        .options
        .iter()
        .find(|o| o.editable())
        .map(|o| o.name.clone())
    {
        let idx = state.named_index(&name).expect("just found it");
        state.remove_option(idx);
        removals += 1;

        assert!(
            !state.options.is_empty(),
            "the kind fallbacks are unremovable, so the list cannot empty"
        );
        assert!(
            state.pick.value() < state.options.len(),
            "the spinner points past the end after removing `{name}` \
             ({} of {})",
            state.pick.value(),
            state.options.len()
        );
        // The unguarded index in `selected()`, exercised for real.
        let _ = state.selected();
    }

    assert!(
        removals >= 2,
        "the fixture has removable options to exhaust"
    );
    assert!(
        state.options.iter().all(|o| !o.editable()),
        "what remains is exactly what cannot be removed"
    );
    // Which is MORE than the kind fallbacks: an `Inline` entry is not
    // editable either, so it survives too. The floor is "every
    // non-editable option", and writing `2` here would have pinned a
    // fixture detail rather than the invariant.
    assert!(state.options.len() >= 2, "at least the two kind fallbacks");
    // And the dial still moves at the floor of that list.
    state.cycle(1);
    state.cycle(-1);
    let _ = state.selected();
}

#[test]
fn chooser_rows_render_the_selected_backend_and_kind_fallback_details() {
    let mut s = panel();
    let rows = s.chooser_rows();
    assert_eq!(rows[0].label, "backend");
    assert!(rows[0].value.contains("dgx1"));
    assert!(rows
        .iter()
        .any(|r| r.label == "url" && r.value == "http://dgx1:11434"));
    assert!(rows.iter().any(|r| r.label == "auth" && r.value == "—"));
    // Kind fallback: no url/model/auth pretence, a session-only note.
    s.cycle(1);
    s.cycle(1);
    s.cycle(1); // ollama
    let rows = s.chooser_rows();
    assert!(rows[0].value.contains("wire kind"));
    assert_eq!(rows[0].provenance, "session-only toggle");
    assert!(!rows.iter().any(|r| r.label == "url"));
}

// ── Review fixes (#1683 adversarial review) ──────────────────────────

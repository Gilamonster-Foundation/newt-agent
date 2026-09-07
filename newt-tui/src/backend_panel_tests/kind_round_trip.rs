use super::*;

#[test]
fn editing_an_anthropic_backend_round_trips_its_pinned_kind() {
    // Regression (#1683 retarget review): `BackendKind::Anthropic` was
    // missing from KIND_LADDER, so editing an anthropic drop-in prefilled
    // the kind dial at "auto (probe)" and an unrelated save (e.g. a model
    // change) silently downgraded the PINNED kind to probe-at-connect —
    // exactly the config corruption the six-field overlay contract forbids.
    let mut s = PanelState::new(PanelSeed {
        options: vec![BackendOption {
            name: "claude".to_string(),
            selection: BackendSelection::Named("claude".to_string()),
            source: BackendSource::UserDropIn,
            kind: Some(BackendKind::Anthropic),
            endpoint: "https://api.anthropic.com".to_string(),
            model: None,
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
            api_key_file: None,
        }],
        active: Some(0),
        default_backend: None,
    });
    s.begin_edit();
    let Mode::Form(form) = &s.mode else {
        panic!("edit should open the form");
    };
    assert_eq!(
        kind_label(KIND_LADDER[form.kind_idx]),
        "anthropic",
        "the kind dial prefills at the drop-in's pinned kind"
    );
    // Change ONLY the model — the dial is never touched.
    s.form_nav(1); // kind
    s.form_nav(1); // url
    s.form_nav(1); // model
    type_text(&mut s, "claude-opus-4");
    let mut seen: Vec<BackendEdit> = Vec::new();
    let mut persist = |edit: &BackendEdit| {
        seen.push(edit.clone());
        BackendSaveResult::Saved {
            note: String::new(),
        }
    };
    assert!(s.submit_form(&mut persist));
    assert_eq!(
        seen[0].kind,
        Some(BackendKind::Anthropic),
        "an untouched kind dial round-trips the pinned kind"
    );
    // …and the SAVE never even names the kind key: an untouched dial is
    // not written, so the drop-in's own value survives whatever the ladder
    // can or cannot express (review §1/§6, the persistence half).
    assert!(!seen[0].dirty.kind, "an untouched kind is not written back");
    assert!(
        !seen[0].dirty.endpoint,
        "an untouched url is not written back"
    );
    assert!(seen[0].dirty.model, "the typed model IS written");
}

#[test]
fn editing_a_kind_the_dial_cannot_represent_is_refused_not_downgraded() {
    // The same class of bug, fail-closed for the kinds the panel does not
    // model: `embedded` has no endpoint, so the six-field form could only
    // ever save it as a corruption. Refuse with a visible status instead.
    let mut s = PanelState::new(PanelSeed {
        options: vec![BackendOption {
            name: "local".to_string(),
            selection: BackendSelection::Named("local".to_string()),
            source: BackendSource::UserDropIn,
            kind: Some(BackendKind::Embedded),
            endpoint: String::new(),
            model: None,
            api_key_env: None,
            api_key_file: None,
        }],
        active: Some(0),
        default_backend: None,
    });
    s.begin_edit();
    assert!(matches!(s.mode, Mode::Choose), "no form opens");
    let status = s.status.as_deref().unwrap_or_default();
    assert!(status.contains("embedded"), "{status}");
    assert!(status.contains("http backends only"), "{status}");
}

#[test]
fn edit_is_blocked_for_kind_fallbacks_and_inline_entries() {
    let mut s = panel();
    s.cycle(1); // gpu-runner
    s.cycle(1); // relic (inline)
    s.begin_edit();
    assert!(matches!(s.mode, Mode::Choose), "inline entry: no form");
    assert!(s.status.as_deref().unwrap().contains("inline"));
    s.cycle(1); // ollama kind fallback
    s.begin_edit();
    assert!(matches!(s.mode, Mode::Choose));
    assert!(s.status.as_deref().unwrap().contains("aren't editable"));
}

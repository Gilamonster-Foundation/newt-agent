use super::*;

fn served(models: &[&str]) -> Served {
    Served::from_models(models.iter().map(|m| m.to_string()).collect())
}
#[test]
fn serving_and_openai_window_via_the_trait() {
    // #backend-trait + #1195: the OpenAi impl derives serving from the
    // served count and reads max_model_len; Ollama is always a multiplexer.
    assert_eq!(OpenAiApi.serving(1), Serving::Instance);
    assert_eq!(OpenAiApi.serving(3), Serving::Multiplexer);
    assert_eq!(OllamaApi.serving(1), Serving::Multiplexer);
    assert_eq!(EmbeddedApi.serving(1), Serving::Instance);
    // The vLLM 256k window is read, not defaulted away.
    let json = serde_json::json!({
        "data": [{ "id": "ornith", "max_model_len": 262144u64 }]
    });
    assert_eq!(parse_openai_models_window(&json, "ornith"), Some(262144));
    assert_eq!(
        parse_openai_models_window(&json, "other"),
        Some(262144),
        "single-instance fallback"
    );
    assert_eq!(
        parse_openai_models_window(&serde_json::json!({"data":[{"id":"m"}]}), "m"),
        None
    );
}

#[test]
fn openai_window_accepts_common_gateway_metadata_fields() {
    for field in ["context_window", "context_length", "max_input_tokens"] {
        let mut entry = serde_json::json!({"id": "hosted/model"});
        entry[field] = serde_json::json!(1_000_000u64);
        let json = serde_json::json!({"data": [entry]});
        assert_eq!(
            parse_openai_models_window(&json, "hosted/model"),
            Some(1_000_000),
            "field {field}"
        );
    }
}

#[test]
fn instance_adopts_the_served_model_unconditionally() {
    // The DGX case: config says one thing, vLLM on :8000 serves another —
    // the server dictates, and the override is FLAGGED for honest UX.
    let b = openai_backend(Some("configured-model"), None);
    let a = adopt(&b, &served(&["ornith-1.0-35b"]), None);
    assert_eq!(a.serving, Serving::Instance, "derived: one served id");
    assert_eq!(a.model.as_deref(), Some("ornith-1.0-35b"));
    assert!(a.requested_ignored, "config disagreed and was overridden");

    // Agreement is not an override.
    let agree = adopt(&openai_backend(Some("m"), None), &served(&["m"]), None);
    assert!(!agree.requested_ignored);
}

#[test]
fn instance_ignores_a_session_request_too() {
    let b = openai_backend(None, Some(Serving::Instance));
    let a = adopt(&b, &served(&["real"]), Some("wish"));
    assert_eq!(a.model.as_deref(), Some("real"));
    assert!(a.requested_ignored);
}

#[test]
fn multiplexer_precedence_requested_then_declared_then_served() {
    let b = BackendConfig {
        name: "o".into(),
        endpoint: "http://h:11434".into(),
        model: Some("declared".into()),
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    };
    // (C3/#1122: a request must be SERVED to win — unserved requests
    // drop fail-soft, covered by its own test below.)
    // #2400: the fixture must actually SERVE "declared" for this to test
    // precedence honestly — the fixture used to omit it entirely, which
    // meant this test passed for the wrong reason (an unvalidated pin
    // string, not a real precedence win). See
    // `multiplexer_drops_an_unserved_declared_model_fail_soft` for the
    // now-covered unserved case.
    let s = served(&["asked", "declared", "first", "second"]);
    assert_eq!(
        adopt(&b, &s, Some("asked")).model.as_deref(),
        Some("asked"),
        "a served session request wins on a multiplexer"
    );
    assert_eq!(adopt(&b, &s, None).model.as_deref(), Some("declared"));
    let bare = BackendConfig {
        model: None,
        ..b.clone()
    };
    // First-SERVED (server order) when nothing is requested or declared.
    assert_eq!(adopt(&bare, &s, None).model.as_deref(), Some("asked"));
    assert!(!adopt(&b, &s, Some("asked")).requested_ignored);
}

#[test]
fn openai_gateway_with_many_models_is_a_multiplexer() {
    let b = openai_backend(None, None);
    let a = adopt(&b, &served(&["a", "b", "c"]), Some("b"));
    assert_eq!(a.serving, Serving::Multiplexer);
    assert_eq!(a.model.as_deref(), Some("b"));
}

#[test]
fn declared_serving_beats_derivation() {
    // A file that pins serving="instance" stays an instance even when the
    // gateway lists several models (operator knows best; doctor shows drift).
    let b = openai_backend(None, Some(Serving::Instance));
    let a = adopt(&b, &served(&["x", "y"]), None);
    assert_eq!(a.serving, Serving::Instance);
    assert_eq!(a.model.as_deref(), Some("x"));
}

#[test]
fn multiplexer_drops_an_unserved_requested_model_fail_soft() {
    // #1122 (C3): the kid's-account case — a typo persisted to
    // settings.toml restores as `requested` forever. The endpoint doesn't
    // serve it → drop it (flagged), fall back to declared/first-served,
    // and the session comes up usable instead of 404ing every launch.
    let b = BackendConfig {
        name: "o".into(),
        endpoint: "http://h:11434".into(),
        model: Some("declared".into()),
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    };
    let s = served(&["declared", "other"]);
    let a = adopt(&b, &s, Some("quen2.5-coder:7b"));
    assert_eq!(a.model.as_deref(), Some("declared"));
    assert!(a.requested_unavailable);
    // A SERVED requested model is honored, unflagged.
    let ok = adopt(&b, &s, Some("other"));
    assert_eq!(ok.model.as_deref(), Some("other"));
    assert!(!ok.requested_unavailable);
    // Empty served list (mid-restart): trust the request, unflagged.
    let trust = adopt(&b, &served(&[]), Some("anything"));
    assert_eq!(trust.model.as_deref(), Some("anything"));
    assert!(!trust.requested_unavailable);
}

#[test]
fn declared_unavailable_is_false_when_nothing_is_declared() {
    // No declared model AND no session request: there is nothing to be
    // "unavailable" — `declared_unavailable` must stay false. This is the
    // mirror image of `multiplexer_drops_an_unserved_declared_model_fail_soft`,
    // which checks the opposite shape (a declared model the server no longer
    // serves flags true here).
    let b = BackendConfig {
        name: "o".into(),
        endpoint: "http://h:11434".into(),
        model: None,
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    };
    let s = served(&["first-served", "second", "third"]);
    let a = adopt(&b, &s, None);
    // Nothing was declared or requested, so the declared-model validity check
    // never fires — it stays false (a real bug would set it true and wrongly
    // warn about a model that was never named).
    assert!(
        !a.declared_unavailable,
        "nothing declared → nothing unavailable"
    );
    // With nothing requested or declared on a multiplexer, fall back to the
    // first served model (server order), never to a dead declared name.
    assert_eq!(
        a.model.as_deref(),
        Some("first-served"),
        "falls back to the first served model"
    );
}

/// #2400: the codex-dgx1 comparison that found this gap — that tool re-probes
/// which model the server reports `"loaded"` on every launch rather than
/// trusting a persisted name. Before this fix, a stale `[[backends]] model =`
/// naming a model the server no longer serves (renamed, unloaded, removed)
/// was used completely unvalidated whenever no session `/model` override was
/// active — the exact shape of the #1122 bug, but for the FILE-declared model
/// instead of a restored/typo'd request, and with no fail-soft flag at all.
#[test]
fn multiplexer_drops_an_unserved_declared_model_fail_soft() {
    let b = BackendConfig {
        name: "o".into(),
        endpoint: "http://h:11434".into(),
        model: Some("qwen3.8-flash-next".into()), // renamed/removed on the server
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    };
    let s = served(&["qwen3-coder_30b", "other"]);
    let a = adopt(&b, &s, None);
    assert_eq!(
        a.model.as_deref(),
        Some("qwen3-coder_30b"),
        "falls through to first-served, never dispatches to the dead name"
    );
    assert!(a.declared_unavailable);
    assert!(
        !a.requested_unavailable,
        "no request was made at all — only the declared model failed"
    );

    // A SERVED declared model is honored, unflagged (unchanged behavior).
    let ok = adopt(
        &BackendConfig {
            model: Some("other".into()),
            ..b.clone()
        },
        &s,
        None,
    );
    assert_eq!(ok.model.as_deref(), Some("other"));
    assert!(!ok.declared_unavailable);

    // When BOTH an unserved request and an unserved declared model are in
    // play, both facts are reported — declared genuinely is a fallback
    // candidate whenever the request doesn't win outright, so suppressing
    // one flag because the other also fired would hide a true statement.
    let both_bad = adopt(&b, &s, Some("also-missing"));
    assert!(both_bad.requested_unavailable);
    assert!(both_bad.declared_unavailable);
    // But a SUCCESSFUL request makes the (bypassed) declared model's own
    // servedness irrelevant to the outcome — it never becomes a candidate,
    // so it is never flagged even when it, too, would have failed.
    let request_wins = adopt(&b, &s, Some("other"));
    assert!(!request_wins.requested_unavailable);
    assert!(!request_wins.declared_unavailable);

    // Empty served list (mid-restart): trust the declared model too, unflagged
    // — matches the existing empty-list trust the request already gets.
    let trust = adopt(&b, &served(&[]), None);
    assert_eq!(trust.model.as_deref(), Some("qwen3.8-flash-next"));
    assert!(!trust.declared_unavailable);
}

#[test]
fn empty_probe_falls_back_without_flagging() {
    // Reachable but nothing listed (vLLM mid-restart): fall back to the
    // request/config; nothing was overridden.
    let b = openai_backend(Some("hint"), Some(Serving::Instance));
    let a = adopt(&b, &served(&[]), None);
    assert_eq!(a.model.as_deref(), Some("hint"));
    assert!(!a.requested_ignored);
    // Nothing anywhere → None; the caller surfaces it.
    let bare = openai_backend(None, Some(Serving::Instance));
    assert_eq!(adopt(&bare, &served(&[]), None).model, None);
}

#[test]
fn responses_only_error_recognizes_openai_wording() {
    assert!(is_responses_only_error(
        r#"{"error":{"message":"This model is only supported in v1/responses","code":"unsupported_api"}}"#
    ));
    // gpt-5.6-era phrasing, hit in field testing: tools work, but only on
    // the Responses surface.
    assert!(is_responses_only_error(
        r#"{"error":{"message":"Function tools with reasoning_effort are not supported for gpt-5.6-sol in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.","type":"invalid_request_error","param":"reasoning_effort"}}"#
    ));
    assert!(!is_responses_only_error(
        r#"{"error":{"message":"model not found"}}"#
    ));
    // "not supported" alone (no mention of /v1/responses) must NOT flip
    // the surface — plain tools-unsupported models stay on chat.
    assert!(!is_responses_only_error(
        r#"{"error":{"message":"tools are not supported by this model"}}"#
    ));
    assert!(!is_responses_only_error("HTTP 404"));
}

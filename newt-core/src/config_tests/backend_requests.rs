use super::*;

// Backend override requests and destination/binding invariants.

#[test]
fn cli_backend_override_with_endpoint_is_exclusive_and_defaults_tiers() {
    // A CLI-pinned endpoint defines the ONLY backend, discarding whatever
    // discovery/drop-ins produced (the ollama-fallback escape hatch), and
    // its tiers default to all four so it actually serves.
    let mut cfg = Config {
        backends: vec![
            BackendConfig {
                name: "discovered-ollama".into(),
                endpoint: "http://localhost:11434".into(),
                kind: Some(BackendKind::Ollama),
                tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
                ..Default::default()
            },
            fallback_localhost_backend(),
        ],
        ..Default::default()
    };
    let over = BackendOverride {
        endpoint: Some("http://router:8080".into()),
        model: Some("big-30b".into()),
        kind: Some(BackendKind::Openai),
        ..Default::default()
    };
    over.apply(&mut cfg);
    assert_eq!(cfg.backends.len(), 1, "CLI endpoint is exclusive");
    let b = &cfg.backends[0];
    assert_eq!(b.name, "cli");
    assert_eq!(b.endpoint, "http://router:8080");
    assert_eq!(b.model.as_deref(), Some("big-30b"));
    assert_eq!(b.kind, Some(BackendKind::Openai));
    assert_eq!(
        b.tiers,
        vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
        "an exclusive CLI backend defaults to all tiers so it serves"
    );
}

#[test]
fn cli_backend_override_field_only_edits_first_backend_in_place() {
    // With no endpoint/model_path the override is a field edit, not a new
    // backend: `--backend-model` swaps only the model of the primary backend.
    //
    // #1850: an UNNAMED field-only edit targets "the backend the shared
    // selection precedence picks", and that precedence reads
    // `$NEWT_PROVIDER` (`select_backend_slot`). Sibling tests in this
    // binary set it to `hollow`/`ghost`/`acme`, and when one of them
    // overlaps this test the selection misses, `apply` swallows the error
    // into a `tracing::warn!`, and the model silently stays `old`.
    // Reproduce with `NEWT_PROVIDER=hollow cargo test -p newt-core --lib
    // cli_backend_override_field_only_edits_first_backend_in_place`.
    // The named-target siblings are unaffected, which is why this is the
    // only one that needs this.
    //
    // The guard alone is not enough: it SERIALIZES and restores, it does
    // not sanitize, so an operator's exported `NEWT_PROVIDER` would still
    // reach the selection. Clear it too — the guard puts it back on drop,
    // including through a panic.
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    crate::process_env::remove_var("NEWT_PROVIDER");
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "eval".into(),
            endpoint: "http://router:8080".into(),
            model: Some("old".into()),
            kind: Some(BackendKind::Openai),
            tiers: vec![Tier::Fast],
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        model: Some("new-model".into()),
        ..Default::default()
    };
    over.apply(&mut cfg);
    assert_eq!(cfg.backends.len(), 1, "no new backend added");
    assert_eq!(cfg.backends[0].name, "eval", "existing backend kept");
    assert_eq!(cfg.backends[0].endpoint, "http://router:8080");
    assert_eq!(cfg.backends[0].model.as_deref(), Some("new-model"));
}

#[test]
fn cli_backend_override_empty_is_a_noop() {
    let mut cfg = Config {
        backends: vec![fallback_localhost_backend()],
        ..Default::default()
    };
    let before: Vec<(String, String)> = cfg
        .backends
        .iter()
        .map(|b| (b.name.clone(), b.endpoint.clone()))
        .collect();
    BackendOverride::default().apply(&mut cfg);
    let after: Vec<(String, String)> = cfg
        .backends
        .iter()
        .map(|b| (b.name.clone(), b.endpoint.clone()))
        .collect();
    assert_eq!(after, before, "an empty override changes nothing");
}

/// A requested destination CHANGE clears the cached observation — E1
/// truth (kind/serving/model) must not ride to E2 in the receipt OR the
/// flattened backend — while the declared binding stands untouched at
/// its declared destination (typed InactiveDestination downstream, not
/// erasure). A near-collision (trailing slash) is a change.
#[test]
fn a_requested_destination_change_clears_the_cached_observation() {
    for e2 in ["http://e2:9000", "http://dgx:8000/"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
                dir.path().join("dgx1.toml"),
                "record = \"probe_v1\"\nendpoint = \"http://dgx:8000\"\nkind = \"openai\"\nserving = \"instance\"\nmodel = \"probed-b\"\n",
            )
            .unwrap();
        let mut cfg = Config {
            backends: vec![BackendConfig {
                name: "dgx1".into(),
                endpoint: "http://dgx:8000".into(),
                model: Some("declared-a".into()),
                card: Some("card-a".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let over = BackendOverride {
            name: Some("dgx1".into()),
            endpoint: Some(e2.into()),
            ..Default::default()
        };
        let receipts = resolve_for_test(&mut cfg, &[dir.path()], Some(over)).unwrap();
        let receipt = &receipts[0];
        assert!(
            receipt.observation.is_none(),
            "`{e2}`: cached E1 observation must not ride to a new destination"
        );
        let b = &cfg.backends[0];
        assert_eq!(b.endpoint, e2);
        assert_eq!(b.kind, None, "`{e2}`: no probed kind leaks");
        assert_eq!(b.serving, None, "`{e2}`: no probed serving leaks");
        assert_eq!(
            b.model.as_deref(),
            Some("declared-a"),
            "`{e2}`: the declaration, never the probed model"
        );
        assert_eq!(
            receipt.binding.card.as_deref(),
            Some("card-a"),
            "`{e2}`: binding evidence preserved, not erased"
        );
        assert_eq!(
            receipt.binding.bound_destination,
            BackendDestination::new(Some("http://dgx:8000".into()), None),
            "`{e2}`: still bound at the DECLARED destination"
        );
    }
}

/// An IDENTICAL requested destination retains the observation — the
/// request re-states where the backend already points, so cached truth
/// still describes the same server.
#[test]
fn an_identical_requested_destination_retains_the_observation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("dgx1.toml"),
            "record = \"probe_v1\"\nendpoint = \"http://dgx:8000\"\nkind = \"openai\"\nserving = \"multiplexer\"\n",
        )
        .unwrap();
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "dgx1".into(),
            endpoint: "http://dgx:8000".into(),
            model: Some("declared-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        name: Some("dgx1".into()),
        endpoint: Some("http://dgx:8000".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[dir.path()], Some(over)).unwrap();
    assert!(
        receipts[0].observation.is_some(),
        "same destination retains"
    );
    assert_eq!(cfg.backends[0].kind, Some(BackendKind::Openai));
    assert_eq!(cfg.backends[0].serving, Some(Serving::Multiplexer));
}

/// A model-only request routes the session to B but RETAINS the
/// declared binding (cardA bound to A at the declared destination) —
/// association is decided downstream, never silently rebound.
#[test]
fn a_model_only_request_retains_the_declared_binding() {
    // The unnamed field-only path reads $NEWT_PROVIDER through the
    // shared selector — pin it unset (guard-restored) so the sole
    // backend is selected deterministically.
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "dgx1".into(),
            endpoint: "http://dgx:8000".into(),
            model: Some("declared-a".into()),
            card: Some("card-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        model: Some("requested-b".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    let receipt = &receipts[0];
    assert_eq!(cfg.backends[0].model.as_deref(), Some("requested-b"));
    let request = receipt.request.as_ref().expect("recorded as a request");
    assert_eq!(request.mode, RequestMode::FieldOnly);
    assert_eq!(request.model.as_deref(), Some("requested-b"));
    assert_eq!(
        receipt.declaration.model.as_deref(),
        Some("declared-a"),
        "the request never masquerades as declaration"
    );
    assert_eq!(receipt.binding.card.as_deref(), Some("card-a"));
    assert_eq!(receipt.binding.bound_model.as_deref(), Some("declared-a"));
}

/// A card-only request rebinds to the DECLARED model — never to a
/// probed one, even when a cached Instance observation routed the
/// session to B.
#[test]
fn a_card_only_request_binds_to_the_declared_model_never_the_probed_one() {
    // Unnamed field-only request — same $NEWT_PROVIDER pin as above.
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("dgx1.toml"),
            "record = \"probe_v1\"\nendpoint = \"http://dgx:8000\"\nserving = \"instance\"\nmodel = \"probed-b\"\n",
        )
        .unwrap();
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "dgx1".into(),
            endpoint: "http://dgx:8000".into(),
            model: Some("declared-a".into()),
            card: Some("card-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        card: Some("card-c".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[dir.path()], Some(over)).unwrap();
    let receipt = &receipts[0];
    assert_eq!(
        cfg.backends[0].model.as_deref(),
        Some("probed-b"),
        "the route still adopts the cached instance model"
    );
    assert_eq!(receipt.binding.card.as_deref(), Some("card-c"));
    assert_eq!(
        receipt.binding.bound_model.as_deref(),
        Some("declared-a"),
        "an explicit rebind binds to requested-or-DECLARED, never probed"
    );
}

/// An explicit card + destination request rebinds AT the new
/// destination, to the requested model (else the declared one).
#[test]
fn an_explicit_card_and_destination_request_rebinds_at_the_new_destination() {
    let base = || Config {
        backends: vec![BackendConfig {
            name: "dgx1".into(),
            endpoint: "http://dgx:8000".into(),
            model: Some("declared-a".into()),
            card: Some("card-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let e2 = BackendDestination::new(Some("http://e2:9000".into()), None);
    // With a requested model: card-c bound to requested-m at E2.
    let mut cfg = base();
    let over = BackendOverride {
        name: Some("dgx1".into()),
        endpoint: Some("http://e2:9000".into()),
        model: Some("requested-m".into()),
        card: Some("card-c".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    let binding = &receipts[0].binding;
    assert_eq!(binding.card.as_deref(), Some("card-c"));
    assert_eq!(binding.bound_model.as_deref(), Some("requested-m"));
    assert_eq!(binding.bound_destination, e2);
    // Without a requested model: the declared one.
    let mut cfg = base();
    let over = BackendOverride {
        name: Some("dgx1".into()),
        endpoint: Some("http://e2:9000".into()),
        card: Some("card-c".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    let binding = &receipts[0].binding;
    assert_eq!(binding.bound_model.as_deref(), Some("declared-a"));
    assert_eq!(binding.bound_destination, e2);
}

/// An exclusive destination request keeps exactly ONE slot: the
/// uniquely named existing one (declaration intact), else a brand-new
/// slot with no declaration layer.
#[test]
fn an_exclusive_destination_request_keeps_one_chosen_slot() {
    let backends = || {
        vec![
            BackendConfig {
                name: "first".into(),
                endpoint: "http://a:1".into(),
                ..Default::default()
            },
            BackendConfig {
                name: "second".into(),
                endpoint: "http://b:2".into(),
                model: Some("declared-b".into()),
                ..Default::default()
            },
        ]
    };
    // Named: the chosen slot survives with its declaration.
    let mut cfg = Config {
        backends: backends(),
        ..Default::default()
    };
    let over = BackendOverride {
        name: Some("second".into()),
        endpoint: Some("http://new:9".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    assert_eq!(cfg.backends.len(), 1);
    assert_eq!(receipts.len(), 1, "receipts stay 1:1");
    assert_eq!(cfg.backends[0].name, "second");
    assert_eq!(
        receipts[0].declaration.model.as_deref(),
        Some("declared-b"),
        "the chosen slot's declaration layer survives"
    );
    // Unnamed: a brand-new `cli` slot, declaration layer empty.
    let mut cfg = Config {
        backends: backends(),
        ..Default::default()
    };
    let over = BackendOverride {
        endpoint: Some("http://new:9".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    assert_eq!(cfg.backends.len(), 1);
    assert_eq!(cfg.backends[0].name, "cli");
    assert_eq!(receipts[0].declaration, DeclaredBackend::default());
    assert_eq!(
        receipts[0].request.as_ref().map(|r| r.mode),
        Some(RequestMode::ExclusiveDestination)
    );
}

/// A destination request holds exactly ONE nonempty destination:
/// both-set and empty-string requests are hard errors, before anything
/// mutates.
#[test]
fn a_destination_request_is_exactly_one_nonempty_destination() {
    let base = || Config {
        backends: vec![BackendConfig {
            name: "a".into(),
            endpoint: "http://a:1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    for (what, over) in [
        (
            "both destinations",
            BackendOverride {
                endpoint: Some("http://h:1".into()),
                model_path: Some("/m.gguf".into()),
                ..Default::default()
            },
        ),
        (
            "empty endpoint",
            BackendOverride {
                endpoint: Some(String::new()),
                ..Default::default()
            },
        ),
        (
            "empty model_path",
            BackendOverride {
                model_path: Some(String::new()),
                ..Default::default()
            },
        ),
    ] {
        let mut cfg = base();
        let err = resolve_for_test(&mut cfg, &[], Some(over)).expect_err(what);
        assert!(err.contains("--backend-"), "{what}: {err}");
    }
}

/// A destination retarget replaces the destination AXIS whole:
/// HTTP→embedded clears the endpoint, embedded→HTTP clears the
/// model_path — through the assembly AND the compatibility
/// `BackendOverride::apply` alike. And an explicit card rebind's
/// destination is one value in three places: the flattened backend, the
/// receipt's request, and the binding.
#[test]
fn a_destination_retarget_replaces_the_destination_axis_whole() {
    // HTTP-declared backend, embedded request — assembly path.
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "a".into(),
            endpoint: "http://a:1".into(),
            model: Some("declared".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        name: Some("a".into()),
        model_path: Some("/models/x.gguf".into()),
        card: Some("card-x".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    let b = &cfg.backends[0];
    assert_eq!(b.endpoint, "", "HTTP endpoint cleared by embedded retarget");
    assert_eq!(b.model_path.as_deref(), Some("/models/x.gguf"));
    let flat = BackendDestination::of(b);
    let receipt = &receipts[0];
    let requested = receipt
        .request
        .as_ref()
        .unwrap()
        .destination_over(&receipt.declaration.destination);
    assert_eq!(flat, requested, "flattened == requested destination");
    assert_eq!(
        receipt.binding.bound_destination, requested,
        "explicit card rebind binds AT the requested destination"
    );
    // Embedded-declared backend, HTTP request — compat apply path.
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "emb".into(),
            model_path: Some("/models/x.gguf".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    BackendOverride {
        name: Some("emb".into()),
        endpoint: Some("http://h:1".into()),
        ..Default::default()
    }
    .apply(&mut cfg);
    let b = &cfg.backends[0];
    assert_eq!(b.endpoint, "http://h:1");
    assert_eq!(b.model_path, None, "embedded path cleared by HTTP retarget");
}

/// `--backend-name` naming nothing is a hard, actionable error — never
/// a silent no-op that edits nothing and selects something else.
#[test]
fn a_named_field_only_request_missing_its_slot_is_a_hard_error() {
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "real".into(),
            endpoint: "http://a:1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        name: Some("ghost".into()),
        model: Some("m".into()),
        ..Default::default()
    };
    let err = resolve_for_test(&mut cfg, &[], Some(over)).expect_err("no fallback");
    assert!(
        err.contains("ghost") && err.contains("real"),
        "names the miss and the configured set: {err}"
    );
}

/// An unnamed field-only request targets the slot the SHARED selector
/// picks (`$NEWT_PROVIDER` / `default_backend` / preference) — never
/// index 0 — and the receipt lands on that same slot.
#[test]
#[serial_test::serial(real_fs)] // mutates NEWT_PROVIDER (guard-restored)
fn an_unnamed_field_only_request_targets_the_selected_slot_not_index_zero() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    let base = || Config {
        backends: vec![
            BackendConfig {
                name: "a".into(),
                endpoint: "http://a:1".into(),
                model: Some("model-a".into()),
                ..Default::default()
            },
            BackendConfig {
                name: "b".into(),
                endpoint: "http://b:2".into(),
                model: Some("model-b".into()),
                ..Default::default()
            },
        ],
        default_backend: Some("b".into()),
        ..Default::default()
    };
    // default_backend picks b.
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let mut cfg = base();
    let over = BackendOverride {
        model: Some("new-model".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over.clone())).unwrap();
    assert_eq!(
        cfg.backends[0].model.as_deref(),
        Some("model-a"),
        "a untouched"
    );
    assert_eq!(
        cfg.backends[1].model.as_deref(),
        Some("new-model"),
        "b edited"
    );
    assert!(receipts[0].request.is_none());
    assert!(
        receipts[1].request.is_some(),
        "receipt on the SELECTED slot"
    );
    // $NEWT_PROVIDER=a outranks the default and retargets the edit.
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::set_var("NEWT_PROVIDER", "a") };
    let mut cfg = base();
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    assert_eq!(
        cfg.backends[0].model.as_deref(),
        Some("new-model"),
        "a edited"
    );
    assert_eq!(
        cfg.backends[1].model.as_deref(),
        Some("model-b"),
        "b untouched"
    );
    assert!(receipts[0].request.is_some());
}

/// `--backend-name b` is BOTH the edit target and this invocation's
/// selection: the named slot takes the edit with an aligned receipt,
/// and (with the CLI's `$NEWT_PROVIDER` install) selection picks b over
/// the configured default.
#[test]
#[serial_test::serial(real_fs)] // mutates NEWT_PROVIDER (guard-restored)
fn a_named_request_edits_and_selects_the_named_backend() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::set_var("NEWT_PROVIDER", "b") };
    let mut cfg = Config {
        backends: vec![
            BackendConfig {
                name: "a".into(),
                endpoint: "http://a:1".into(),
                ..Default::default()
            },
            BackendConfig {
                name: "b".into(),
                endpoint: "http://b:2".into(),
                model: Some("model-b".into()),
                ..Default::default()
            },
        ],
        default_backend: Some("a".into()),
        ..Default::default()
    };
    let over = BackendOverride {
        name: Some("b".into()),
        model: Some("new-model".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    assert_eq!(cfg.backends[1].model.as_deref(), Some("new-model"));
    assert!(
        receipts[1].request.is_some(),
        "receipt aligned with the edit"
    );
    let resolved = ResolvedConfig {
        config: cfg,
        receipts,
    };
    let picked = resolved.selected_backend().expect("named selection");
    assert_eq!(picked.slot, 1, "name-only selection beats the default");
    assert_eq!(picked.backend.model.as_deref(), Some("new-model"));
    assert!(picked.receipt.request.is_some());
}

/// A field-only `--backend-*` cannot edit the explicitly selected but
/// destination-less slot (editing it routes nothing; editing another
/// deserts the selection) — while a DESTINATION request targeting the
/// same backend by name is fine: the request itself supplies the route.
#[test]
#[serial_test::serial(real_fs)] // mutates NEWT_PROVIDER (guard-restored)
fn an_unnamed_field_only_request_cannot_edit_an_unroutable_selected_slot() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    let base = || Config {
        backends: vec![
            BackendConfig {
                name: "routable".into(),
                endpoint: "http://a:1".into(),
                ..Default::default()
            },
            BackendConfig {
                name: "hollow".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let model_only = BackendOverride {
        model: Some("m".into()),
        ..Default::default()
    };
    // $NEWT_PROVIDER selects the hollow slot.
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::set_var("NEWT_PROVIDER", "hollow") };
    let mut cfg = base();
    let err = resolve_for_test(&mut cfg, &[], Some(model_only.clone()))
        .expect_err("no silent edit of an unroutable selection");
    assert!(
        err.contains("hollow") && err.contains("--backend-url"),
        "names the slot and the remediation: {err}"
    );
    // default_backend selecting it errors identically.
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let mut cfg = Config {
        default_backend: Some("hollow".into()),
        ..base()
    };
    let err = resolve_for_test(&mut cfg, &[], Some(model_only))
        .expect_err("default-selected unroutable slot refuses the edit");
    assert!(err.contains("hollow"), "{err}");
    // A destination request naming it supplies the route — allowed.
    let mut cfg = Config {
        default_backend: Some("hollow".into()),
        ..base()
    };
    let over = BackendOverride {
        name: Some("hollow".into()),
        endpoint: Some("http://now-routable:9".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    assert_eq!(cfg.backends.len(), 1);
    assert_eq!(cfg.backends[0].endpoint, "http://now-routable:9");
    assert_eq!(
        receipts[0].request.as_ref().map(|r| r.mode),
        Some(RequestMode::ExclusiveDestination)
    );
}

/// Destination/kind coherence is one invariant everywhere: a
/// model_path route composes to `BackendKind::Embedded`; an endpoint
/// route never retains Embedded (cleared to probe-at-connect). Asserted
/// on the EFFECTIVE backend and the receipt destination, not just
/// selection.
#[test]
fn destination_kind_coherence_is_enforced_in_composition() {
    // HTTP/OpenAI backend retargeted to a model_path.
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "a".into(),
            endpoint: "http://a:1".into(),
            kind: Some(BackendKind::Openai),
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        name: Some("a".into()),
        model_path: Some("/models/x.gguf".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    let b = &cfg.backends[0];
    assert_eq!(
        b.kind,
        Some(BackendKind::Embedded),
        "model_path route IS embedded"
    );
    assert_eq!(b.endpoint, "");
    let requested = receipts[0]
        .request
        .as_ref()
        .unwrap()
        .destination_over(&receipts[0].declaration.destination);
    assert_eq!(
        requested,
        BackendDestination::new(None, Some("/models/x.gguf".into()))
    );
    assert_eq!(BackendDestination::of(b), requested);

    // A brand-new path-only CLI backend.
    let mut cfg = Config {
        backends: vec![],
        ..Default::default()
    };
    let over = BackendOverride {
        model_path: Some("/models/y.gguf".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    assert_eq!(cfg.backends[0].kind, Some(BackendKind::Embedded));
    assert_eq!(
        receipts[0]
            .request
            .as_ref()
            .unwrap()
            .destination_over(&receipts[0].declaration.destination),
        BackendDestination::new(None, Some("/models/y.gguf".into()))
    );

    // Embedded backend retargeted to an endpoint: Embedded must not ride.
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "emb".into(),
            model_path: Some("/models/x.gguf".into()),
            kind: Some(BackendKind::Embedded),
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        name: Some("emb".into()),
        endpoint: Some("http://h:1".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    let b = &cfg.backends[0];
    assert_eq!(b.endpoint, "http://h:1");
    assert_eq!(b.model_path, None);
    assert_eq!(
        b.kind, None,
        "an endpoint route never retains Embedded — cleared to probe-at-connect"
    );
    assert_eq!(
        BackendDestination::of(b),
        receipts[0]
            .request
            .as_ref()
            .unwrap()
            .destination_over(&receipts[0].declaration.destination)
    );
}

/// Explicitly contradictory destination/kind pairs are refused, and an
/// incoherent model_path-on-HTTP-kind DECLARATION is not routable.
#[test]
fn contradictory_destination_kind_pairs_are_refused() {
    let base = || Config {
        backends: vec![BackendConfig {
            name: "a".into(),
            endpoint: "http://a:1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut cfg = base();
    let err = resolve_for_test(
        &mut cfg,
        &[],
        Some(BackendOverride {
            endpoint: Some("http://h:1".into()),
            kind: Some(BackendKind::Embedded),
            ..Default::default()
        }),
    )
    .expect_err("url + embedded kind");
    assert!(err.contains("contradictory"), "{err}");
    let mut cfg = base();
    let err = resolve_for_test(
        &mut cfg,
        &[],
        Some(BackendOverride {
            model_path: Some("/m.gguf".into()),
            kind: Some(BackendKind::Openai),
            ..Default::default()
        }),
    )
    .expect_err("model_path + HTTP kind");
    assert!(err.contains("contradictory"), "{err}");
    // The declaration-level incoherence: model_path on an HTTP kind is
    // not a route.
    assert!(!backend_is_routable(&BackendConfig {
        name: "weird".into(),
        model_path: Some("/m.gguf".into()),
        kind: Some(BackendKind::Openai),
        ..Default::default()
    }));
    assert!(backend_is_routable(&BackendConfig {
        name: "emb".into(),
        model_path: Some("/m.gguf".into()),
        kind: Some(BackendKind::Embedded),
        ..Default::default()
    }));
}

/// The public `BackendOverride::apply` delegates to the invariant-owning
/// assembly path: refusals leave the config byte-for-byte untouched
/// (warned, for the infallible surface; typed, via `try_apply`), the
/// unnamed field-only edit lands on the SELECTED slot, a named miss is
/// an error rather than a silent no-op, and cross-destination kind
/// coherence holds.
#[test]
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER (guard-restored)
fn backend_override_apply_delegates_to_the_invariant_owning_path() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let base = || Config {
        backends: vec![
            BackendConfig {
                name: "a".into(),
                endpoint: "http://a:1".into(),
                model: Some("model-a".into()),
                ..Default::default()
            },
            BackendConfig {
                name: "b".into(),
                endpoint: "http://b:2".into(),
                model: Some("model-b".into()),
                ..Default::default()
            },
        ],
        default_backend: Some("b".into()),
        ..Default::default()
    };
    // BackendConfig carries no PartialEq — compare serialized bytes.
    let snap = |cfg: &Config| -> Vec<String> {
        cfg.backends
            .iter()
            .map(|b| toml::to_string(b).unwrap())
            .collect()
    };
    let untouched = snap(&base());
    // Both destinations / empty destination: refused, untouched.
    for over in [
        BackendOverride {
            endpoint: Some("http://h:1".into()),
            model_path: Some("/m.gguf".into()),
            ..Default::default()
        },
        BackendOverride {
            endpoint: Some(String::new()),
            ..Default::default()
        },
    ] {
        let mut cfg = base();
        assert!(over.try_apply(&mut cfg).is_err());
        assert_eq!(snap(&cfg), untouched, "refusal leaves it untouched");
        let mut cfg = base();
        over.apply(&mut cfg); // infallible surface: warns, same untouched state
        assert_eq!(snap(&cfg), untouched);
    }
    // Unnamed field-only: the SELECTED slot (default_backend = b), not [0].
    let mut cfg = base();
    BackendOverride {
        model: Some("new".into()),
        ..Default::default()
    }
    .apply(&mut cfg);
    assert_eq!(cfg.backends[0].model.as_deref(), Some("model-a"));
    assert_eq!(cfg.backends[1].model.as_deref(), Some("new"));
    // Named miss: an error via try_apply; untouched via apply.
    let mut cfg = base();
    let over = BackendOverride {
        name: Some("ghost".into()),
        model: Some("m".into()),
        ..Default::default()
    };
    let err = over.try_apply(&mut cfg).expect_err("named miss");
    assert!(err.contains("ghost"), "{err}");
    assert_eq!(snap(&cfg), untouched);
    // Cross-destination kind coherence through the public surface.
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "emb".into(),
            model_path: Some("/m.gguf".into()),
            kind: Some(BackendKind::Embedded),
            ..Default::default()
        }],
        ..Default::default()
    };
    BackendOverride {
        name: Some("emb".into()),
        endpoint: Some("http://h:1".into()),
        ..Default::default()
    }
    .apply(&mut cfg);
    assert_eq!(
        cfg.backends[0].kind, None,
        "Embedded cleared on an HTTP route"
    );
    assert_eq!(cfg.backends[0].model_path, None);
}

/// A NAMED field-only request must target a routable backend — editing
/// a destination-less one routes nothing.
#[test]
fn a_named_field_only_request_must_target_a_routable_backend() {
    let base = || Config {
        backends: vec![
            BackendConfig {
                name: "real".into(),
                endpoint: "http://r:1".into(),
                ..Default::default()
            },
            BackendConfig {
                name: "hollow".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    for over in [
        BackendOverride {
            name: Some("hollow".into()),
            model: Some("m".into()),
            ..Default::default()
        },
        BackendOverride {
            name: Some("hollow".into()),
            ..Default::default()
        },
    ] {
        let mut cfg = base();
        let err = resolve_for_test(&mut cfg, &[], Some(over.clone()))
            .expect_err("a named unroutable target refuses the edit");
        assert!(
            err.contains("hollow") && err.contains("--backend-url"),
            "{err}"
        );
        let mut cfg = base();
        let err = over
            .try_apply(&mut cfg)
            .expect_err("same through try_apply");
        assert!(err.contains("hollow"), "{err}");
    }
}

/// Destination XOR holds for DECLARATIONS too: an inline backend with
/// both endpoint and model_path is a hard error on normal AND profile
/// paths; a both-destination drop-in warn-skips, leaving the prior
/// declaration standing.
#[test]
fn a_both_destination_declaration_is_rejected_everywhere() {
    let both = BackendConfig {
        name: "twoplace".into(),
        endpoint: "http://h:1".into(),
        model_path: Some("/m.gguf".into()),
        ..Default::default()
    };
    let mut cfg = Config {
        backends: vec![both.clone()],
        ..Default::default()
    };
    let err = resolve_for_test(&mut cfg, &[], None).expect_err("inline both");
    assert!(err.contains("ONE destination"), "{err}");
    let err = Config {
        backends: vec![both],
        ..Default::default()
    }
    .prepare_runtime()
    .expect_err("profile path validates too");
    assert!(err.to_string().contains("ONE destination"), "{err}");
    // Drop-in variant: warn-skip; the prior declaration survives.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("keep.toml"),
        "record = \"operator_v1\"\nendpoint = \"http://new:9\"\nmodel_path = \"/m.gguf\"\n",
    )
    .unwrap();
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "keep".into(),
            endpoint: "http://old:1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    // #1984: warnings as a returned value, not a scraped log — see
    // `BackendAssembly::warnings`'s doc in config/backend.rs.
    let (_receipts, warnings) = merge_for_test_with_warnings(&mut cfg, &[dir.path()]).unwrap();
    let warnings = warnings.join("\n");
    assert!(warnings.contains("ONE destination"), "{warnings}");
    assert_eq!(cfg.backends[0].endpoint, "http://old:1", "prior survives");
}

/// A kind-only field request must match the target's EXISTING
/// destination — refused atomically, never silently normalized away.
#[test]
fn kind_only_field_requests_must_match_the_targets_destination() {
    let http = || Config {
        backends: vec![BackendConfig {
            name: "http".into(),
            endpoint: "http://h:1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let emb = || Config {
        backends: vec![BackendConfig {
            name: "emb".into(),
            model_path: Some("/m.gguf".into()),
            kind: Some(BackendKind::Embedded),
            ..Default::default()
        }],
        ..Default::default()
    };
    // embedded kind onto an HTTP destination: refused (both surfaces).
    let over = BackendOverride {
        name: Some("http".into()),
        kind: Some(BackendKind::Embedded),
        ..Default::default()
    };
    let mut cfg = http();
    let err = resolve_for_test(&mut cfg, &[], Some(over.clone())).expect_err("refuse");
    assert!(err.contains("contradictory"), "{err}");
    let mut cfg = http();
    assert!(over.try_apply(&mut cfg).is_err());
    assert_eq!(cfg.backends[0].kind, None, "untouched");
    // HTTP kind onto a model_path destination: refused.
    let over = BackendOverride {
        name: Some("emb".into()),
        kind: Some(BackendKind::Openai),
        ..Default::default()
    };
    let mut cfg = emb();
    let err = resolve_for_test(&mut cfg, &[], Some(over)).expect_err("refuse");
    assert!(err.contains("contradictory"), "{err}");
    // embedded kind onto a model_path destination: fine.
    let over = BackendOverride {
        name: Some("emb".into()),
        kind: Some(BackendKind::Embedded),
        ..Default::default()
    };
    let mut cfg = emb();
    resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    assert_eq!(cfg.backends[0].kind, Some(BackendKind::Embedded));
}

/// A field-only edit never invents tiers: an intentionally empty
/// `tiers = []` declaration stays empty. Tier defaulting belongs to the
/// exclusive destination request alone.
#[test]
fn a_field_only_edit_never_invents_tiers() {
    let base = || Config {
        backends: vec![BackendConfig {
            name: "a".into(),
            endpoint: "http://a:1".into(),
            tiers: vec![],
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        name: Some("a".into()),
        model: Some("m".into()),
        ..Default::default()
    };
    let mut cfg = base();
    resolve_for_test(&mut cfg, &[], Some(over.clone())).unwrap();
    assert!(cfg.backends[0].tiers.is_empty(), "assembly path");
    let mut cfg = base();
    over.try_apply(&mut cfg).unwrap();
    assert!(cfg.backends[0].tiers.is_empty(), "public composer path");
    // Exclusive destination still defaults tiers so it serves.
    let mut cfg = base();
    BackendOverride {
        name: Some("a".into()),
        endpoint: Some("http://new:9".into()),
        ..Default::default()
    }
    .try_apply(&mut cfg)
    .unwrap();
    assert_eq!(cfg.backends[0].tiers.len(), 4, "exclusive defaults tiers");
}

/// Public composition aligns config-level selection with the request
/// target: an exclusive request re-points `default_backend` at its
/// (kept or new) backend, a NAMED edit selects its target, an unnamed
/// edit leaves the selection alone.
#[test]
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER (guard-restored)
fn try_apply_aligns_config_selection_with_the_request_target() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let base = || Config {
        backends: vec![
            BackendConfig {
                name: "a".into(),
                endpoint: "http://a:1".into(),
                ..Default::default()
            },
            BackendConfig {
                name: "b".into(),
                endpoint: "http://b:2".into(),
                ..Default::default()
            },
        ],
        default_backend: Some("a".into()),
        ..Default::default()
    };
    // Exclusive unnamed: the new `cli` backend IS the selection — no
    // stale default naming a discarded backend.
    let mut cfg = base();
    BackendOverride {
        endpoint: Some("http://new:9".into()),
        ..Default::default()
    }
    .try_apply(&mut cfg)
    .unwrap();
    assert_eq!(cfg.default_backend.as_deref(), Some("cli"));
    assert_eq!(
        cfg.select_configured_backend().map(|b| b.name.as_str()),
        Some("cli"),
        "no stale selection after the exclusive request"
    );
    // Named field-only: the named target becomes the selection.
    let mut cfg = base();
    BackendOverride {
        name: Some("b".into()),
        model: Some("m".into()),
        ..Default::default()
    }
    .try_apply(&mut cfg)
    .unwrap();
    assert_eq!(cfg.default_backend.as_deref(), Some("b"));
    assert_eq!(
        cfg.select_configured_backend().map(|b| b.name.as_str()),
        Some("b")
    );
    // Unnamed field-only: edits the selected backend; selection stays.
    let mut cfg = base();
    BackendOverride {
        model: Some("m".into()),
        ..Default::default()
    }
    .try_apply(&mut cfg)
    .unwrap();
    assert_eq!(cfg.default_backend.as_deref(), Some("a"), "unchanged");
    assert_eq!(cfg.backends[0].model.as_deref(), Some("m"));
}

/// An unnamed kind edit that would REORDER the shared precedence is
/// refused with a demand for --backend-name — edit target and final
/// selection must be the same slot.
#[test]
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER (guard-restored)
fn a_destabilizing_unnamed_kind_edit_requires_a_name() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let base = || Config {
        backends: vec![
            BackendConfig {
                name: "b".into(),
                endpoint: "http://b:1".into(),
                kind: Some(BackendKind::Ollama),
                ..Default::default()
            },
            BackendConfig {
                name: "a".into(),
                endpoint: "http://a:2".into(),
                kind: Some(BackendKind::Openai),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    // Preference selects `a` (OpenAI). Retagging it ollama would make
    // `b` the selection — divergence, refused.
    let over = BackendOverride {
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    };
    let mut cfg = base();
    let err = resolve_for_test(&mut cfg, &[], Some(over)).expect_err("diverges");
    assert!(err.contains("--backend-name"), "{err}");
    // Named, the same edit is explicit and fine.
    let over = BackendOverride {
        name: Some("a".into()),
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    };
    let mut cfg = base();
    resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    assert_eq!(cfg.backends[1].kind, Some(BackendKind::Ollama));
}

/// O: an empty/whitespace `--backend-model` is refused ATOMICALLY —
/// there is no implicit clear. Otherwise the flattened route would
/// serve server-decides while the receipt/binding fell back to the
/// STALE declared model, and Phase B's principal derivation would
/// activate against a model the session is not running. With and
/// without a card rebind; config untouched on refusal.
#[test]
fn an_empty_model_request_is_refused_never_a_stale_fallback() {
    let base = || Config {
        backends: vec![BackendConfig {
            name: "a".into(),
            endpoint: "http://a:1".into(),
            model: Some("declared-a".into()),
            card: Some("card-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let snap = |cfg: &Config| -> Vec<String> {
        cfg.backends
            .iter()
            .map(|b| toml::to_string(b).unwrap())
            .collect()
    };
    let untouched = snap(&base());
    for model in ["", "   "] {
        for card in [None, Some("card-c")] {
            let over = BackendOverride {
                name: Some("a".into()),
                model: Some(model.to_string()),
                card: card.map(str::to_string),
                ..Default::default()
            };
            let mut cfg = base();
            let err = resolve_for_test(&mut cfg, &[], Some(over.clone()))
                .expect_err("an empty model request must refuse");
            assert!(err.contains("--backend-model"), "{err}");
            let mut cfg = base();
            assert!(over.try_apply(&mut cfg).is_err(), "try_apply refuses too");
            assert_eq!(snap(&cfg), untouched, "config untouched on refusal");
            assert_eq!(
                cfg.backends[0].model.as_deref(),
                Some("declared-a"),
                "the declared model is neither cleared nor re-bound"
            );
        }
    }
}

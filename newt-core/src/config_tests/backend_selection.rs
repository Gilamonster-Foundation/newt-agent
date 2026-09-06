use super::*;

// Backend routing, selector precedence, and runtime composition agreement.

/// A valid embedded `model_path` is routable everywhere selection used
/// to require an endpoint: sole, default, preference, and the exclusive
/// model_path request.
#[test]
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER (guard-restored)
fn embedded_backends_are_routable_for_selection() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let embedded = BackendConfig {
        name: "emb".into(),
        model_path: Some("/models/x.gguf".into()),
        kind: Some(BackendKind::Embedded),
        ..Default::default()
    };
    // Sole.
    let cfg = Config {
        backends: vec![embedded.clone()],
        ..Default::default()
    };
    assert_eq!(
        cfg.select_configured_backend().map(|b| b.name.as_str()),
        Some("emb")
    );
    // Default names it.
    let cfg = Config {
        backends: vec![
            BackendConfig {
                name: "http".into(),
                endpoint: "http://a:1".into(),
                ..Default::default()
            },
            embedded.clone(),
        ],
        default_backend: Some("emb".into()),
        ..Default::default()
    };
    assert_eq!(
        cfg.select_configured_backend().map(|b| b.name.as_str()),
        Some("emb")
    );
    // Preference: first ROUTABLE wins when nothing is more specific.
    let cfg = Config {
        backends: vec![
            BackendConfig {
                name: "hollow".into(),
                ..Default::default()
            },
            embedded.clone(),
        ],
        ..Default::default()
    };
    assert_eq!(
        cfg.select_configured_backend().map(|b| b.name.as_str()),
        Some("emb")
    );
    // Exclusive model_path request: the one slot, selected.
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "http".into(),
            endpoint: "http://a:1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let over = BackendOverride {
        model_path: Some("/models/x.gguf".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], Some(over)).unwrap();
    let resolved = ResolvedConfig {
        config: cfg,
        receipts,
    };
    let picked = resolved
        .selected_backend()
        .expect("embedded exclusive selects");
    assert_eq!(picked.slot, 0);
    assert_eq!(picked.backend.model_path.as_deref(), Some("/models/x.gguf"));
    assert_eq!(
        picked.backend.endpoint, "",
        "no endpoint on the embedded route"
    );
}

/// `$NEWT_PROVIDER` naming a configured but DESTINATION-LESS backend is
/// a typed hard error on the selection contract — never the pre-#1819
/// silent pick of the unroutable backend, and never a silent
/// fall-through to some other backend. The `Option` surfaces select
/// NOTHING (documented), the receipts stay slot-aligned, and a provider
/// still wins the name tie against an unroutable backend.
#[test]
#[serial_test::serial(real_fs)] // mutates NEWT_PROVIDER (guard-restored)
fn an_env_named_unroutable_backend_is_a_typed_error_never_a_silent_pick() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::set_var("NEWT_PROVIDER", "hollow") };
    let mut cfg = Config {
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
    assert!(
        matches!(cfg.select_backend(), SelectionOutcome::UnroutableNamed(ref n) if n == "hollow"),
        "the high-level contract surfaces the error: {:?}",
        cfg.select_backend()
    );
    assert!(
        cfg.select_configured_backend().is_none(),
        "the Option surface selects NOTHING — not `hollow`, not `routable`"
    );
    // Receipts stay slot-aligned; the receipt-bearing pick agrees (None).
    let receipts = resolve_for_test(&mut cfg, &[], None).unwrap();
    assert_eq!(receipts.len(), 2);
    let resolved = ResolvedConfig {
        config: cfg,
        receipts,
    };
    assert!(
        resolved.selected_backend().is_none(),
        "same shared selector"
    );
    // A provider claiming the name still wins the tie.
    let cfg = Config {
        backends: vec![BackendConfig {
            name: "hollow".into(),
            ..Default::default()
        }],
        providers: vec![ProviderConfig {
            name: "hollow".into(),
            command: "newt-provider-openai".into(),
            model: None,
            env_pass: vec![],
            tiers: vec![],
        }],
        ..Default::default()
    };
    assert!(
        matches!(
            cfg.select_backend(),
            SelectionOutcome::Selected(SelectedBackend::Provider(p)) if p.name == "hollow"
        ),
        "provider wins the name tie: {:?}",
        cfg.select_backend()
    );
}

/// `default_backend` naming a destination-less backend errors the same
/// way — previously it silently fell through to the preference rules
/// and ran a different backend than the one the operator configured.
#[test]
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER (guard-restored)
fn a_default_named_unroutable_backend_errors_instead_of_silent_preference() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let cfg = Config {
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
        default_backend: Some("hollow".into()),
        ..Default::default()
    };
    assert!(
        matches!(cfg.select_backend(), SelectionOutcome::UnroutableNamed(ref n) if n == "hollow"),
        "{:?}",
        cfg.select_backend()
    );
    assert!(cfg.select_configured_backend().is_none());
}

/// A valid cached probe for a DISK-declared backend must not emit the
/// destructive "unconfigured — delete the file" warning merely because
/// this invocation exclusively selected another backend: attachment
/// resolves against final declarations BEFORE the CLI prunes. A genuine
/// disk-level endpoint mismatch still warns.
#[test]
fn exclusive_selection_of_another_backend_emits_no_orphan_probe_warning() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("cached.toml"),
        "record = \"probe_v1\"\nendpoint = \"http://cached:8000\"\nkind = \"openai\"\n",
    )
    .unwrap();
    let base = || Config {
        backends: vec![
            BackendConfig {
                name: "cached".into(),
                endpoint: "http://cached:8000".into(),
                ..Default::default()
            },
            BackendConfig {
                name: "other".into(),
                endpoint: "http://other:9".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    // Exclusive selection of `other`: the cache quietly attaches (then
    // its slot is pruned) — no orphan/delete warning.
    let mut cfg = base();
    let over = BackendOverride {
        name: Some("other".into()),
        endpoint: Some("http://other:9".into()),
        ..Default::default()
    };
    // #1984: warnings are asserted as RETURNED VALUES now, not scraped off
    // a global tracing subscriber (see `BackendAssembly::warnings`'s doc in
    // config/backend.rs). `.join("\n")` keeps every `.contains()`/`!.contains()`
    // assertion below byte-for-byte unchanged from the pre-#1984 shape.
    let (_receipts, warnings) =
        resolve_for_test_with_warnings(&mut cfg, &[dir.path()], Some(over)).unwrap();
    let warnings = warnings.join("\n");
    assert!(
        !warnings.contains("unconfigured") && !warnings.contains("delete the file"),
        "a valid cache for a disk-declared backend is not an orphan: {warnings}"
    );
    assert_eq!(cfg.backends.len(), 1);
    assert_eq!(cfg.backends[0].name, "other");
    // Control: a genuine disk-level mismatch still warns.
    let mismatch = tempfile::tempdir().unwrap();
    std::fs::write(
        mismatch.path().join("cached.toml"),
        "record = \"probe_v1\"\nendpoint = \"http://elsewhere:1\"\nkind = \"openai\"\n",
    )
    .unwrap();
    let mut cfg = base();
    let (_receipts, warnings) =
        resolve_for_test_with_warnings(&mut cfg, &[mismatch.path()], None).unwrap();
    let warnings = warnings.join("\n");
    assert!(
        warnings.contains("does not match"),
        "the real mismatch keeps its warning: {warnings}"
    );
    // And a truly unconfigured probe still warns destructively-visibly.
    let mut cfg = Config {
        backends: vec![],
        ..Default::default()
    };
    let (_receipts, warnings) =
        resolve_for_test_with_warnings(&mut cfg, &[dir.path()], None).unwrap();
    let warnings = warnings.join("\n");
    assert!(warnings.contains("unconfigured"), "{warnings}");
}

/// An explicit env selector that matches NOTHING (a typo, or a
/// provider's name) stops the Option surface and the unnamed field-only
/// override — never a silent edit/selection of some other backend.
#[test]
#[serial_test::serial(real_fs)] // mutates NEWT_PROVIDER (guard-restored)
fn an_unmatched_env_selector_stops_option_surfaces_and_unnamed_overrides() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    let base = || Config {
        backends: vec![BackendConfig {
            name: "real".into(),
            endpoint: "http://r:1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::set_var("NEWT_PROVIDER", "ghost") };
    let cfg = base();
    assert!(
        cfg.select_configured_backend().is_none(),
        "unknown env name selects NOTHING — not `real`"
    );
    let mut cfg = base();
    let err = resolve_for_test(
        &mut cfg,
        &[],
        Some(BackendOverride {
            model: Some("m".into()),
            ..Default::default()
        }),
    )
    .expect_err("no silent edit of `real`");
    assert!(err.contains("ghost"), "{err}");
    // A provider's name behaves identically at this layer (the slot
    // selector only knows [[backends]]); the error says so.
    assert!(
        err.contains("provider"),
        "mentions the provider case: {err}"
    );
}

/// Field-only targeting runs over the PROBE-INFORMED view: a cached
/// probe that makes B OpenAI moves both the preference selection AND
/// the unnamed edit to B — edit target and final selection agree.
#[test]
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER (guard-restored)
fn probe_informed_targeting_agrees_with_final_selection() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("b.toml"),
        "record = \"probe_v1\"\nendpoint = \"http://b:2\"\nkind = \"openai\"\n",
    )
    .unwrap();
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
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let over = BackendOverride {
        model: Some("m".into()),
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[dir.path()], Some(over)).unwrap();
    assert!(
        receipts[0].request.is_none(),
        "raw-first `a` is NOT the target"
    );
    assert!(
        receipts[1].request.is_some(),
        "the probe-informed OpenAI preference targets `b`"
    );
    let resolved = ResolvedConfig {
        config: cfg,
        receipts,
    };
    let picked = resolved.selected_backend().expect("something selects");
    assert_eq!(
        picked.slot, 1,
        "final selection agrees with the edit target"
    );
    assert!(picked.receipt.request.is_some());
}

/// An EMPTY `default_backend` is absent on every surface — Option,
/// typed, and override targeting agree (previously the slot selector
/// treated `Some("")` as an authoritative selector for a backend named
/// `""`).
#[test]
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER (guard-restored)
fn an_empty_default_backend_is_absent_on_every_surface() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let base = || Config {
        backends: vec![BackendConfig {
            name: "real".into(),
            endpoint: "http://r:1".into(),
            ..Default::default()
        }],
        default_backend: Some(String::new()),
        ..Default::default()
    };
    let cfg = base();
    assert_eq!(
        cfg.select_configured_backend().map(|b| b.name.as_str()),
        Some("real"),
        "Option surface"
    );
    assert!(
        matches!(
            cfg.select_backend(),
            SelectionOutcome::Selected(SelectedBackend::Configured(b)) if b.name == "real"
        ),
        "typed surface"
    );
    let mut cfg = base();
    let receipts = resolve_for_test(
        &mut cfg,
        &[],
        Some(BackendOverride {
            model: Some("m".into()),
            ..Default::default()
        }),
    )
    .unwrap();
    assert!(receipts[0].request.is_some(), "override targeting agrees");
}

/// Provider identity is validated on normal and profile paths, and the
/// deliberate cross-namespace tie precedence is pinned: a ROUTABLE
/// backend wins the name tie; a destination-less one loses it to the
/// provider.
#[test]
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER (guard-restored)
fn provider_identity_is_validated_and_name_ties_are_pinned() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let provider = |name: &str| ProviderConfig {
        name: name.into(),
        command: "newt-provider-openai".into(),
        model: None,
        env_pass: vec![],
        tiers: vec![],
    };
    // Duplicate providers: hard error (profile path shown; the normal
    // path shares the same validation call).
    let err = Config {
        providers: vec![provider("twin"), provider("twin")],
        ..Default::default()
    }
    .prepare_runtime()
    .expect_err("duplicate providers");
    assert!(err.to_string().contains("twin"), "{err}");
    // Empty provider name: hard error.
    let err = Config {
        providers: vec![provider(" ")],
        ..Default::default()
    }
    .prepare_runtime()
    .expect_err("empty provider name");
    assert!(err.to_string().contains("no name"), "{err}");
    // Tie precedence: a ROUTABLE backend beats the same-name provider…
    let cfg = Config {
        backends: vec![BackendConfig {
            name: "tie".into(),
            endpoint: "http://t:1".into(),
            ..Default::default()
        }],
        providers: vec![provider("tie")],
        default_backend: Some("tie".into()),
        ..Default::default()
    };
    assert!(matches!(
        cfg.select_backend(),
        SelectionOutcome::Selected(SelectedBackend::Configured(b)) if b.name == "tie"
    ));
    // …and a destination-less backend loses the tie to the provider.
    let cfg = Config {
        backends: vec![BackendConfig {
            name: "tie".into(),
            ..Default::default()
        }],
        providers: vec![provider("tie")],
        default_backend: Some("tie".into()),
        ..Default::default()
    };
    assert!(matches!(
        cfg.select_backend(),
        SelectionOutcome::Selected(SelectedBackend::Provider(p)) if p.name == "tie"
    ));
}

/// Provider-only parity: the NORMAL path must not synthesize a
/// localhost backend when `[[providers]]` exist — the synthetic backend
/// would outrank the provider that the profile path selects. A
/// provider-only config is configured (`is_unconfigured` = false); the
/// fully bare config still gets the localhost fallback.
#[test]
#[serial_test::serial(real_fs)] // pins NEWT_CONFIG/HOME/cwd + NEWT_PROVIDER
fn a_provider_only_config_selects_the_provider_on_normal_and_profile_paths() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let dir = tempfile::tempdir().unwrap();
    let _sandbox = HomeSandbox::enter(dir.path());
    std::fs::write(
            dir.path().join("config.toml"),
            "[[providers]]\nname = \"acme\"\ncommand = \"newt-provider-openai\"\nenv_pass = []\ntiers = []\n",
        )
        .unwrap();
    // Normal path: no synthesized backend, the provider is selected.
    let resolved = Config::resolve_runtime_unpublished().unwrap();
    assert!(
        resolved.backends.is_empty(),
        "no synthetic localhost backend beside a provider"
    );
    assert!(!resolved.is_unconfigured(), "a provider IS configuration");
    assert!(matches!(
        resolved.select_backend(),
        SelectionOutcome::Selected(SelectedBackend::Provider(p)) if p.name == "acme"
    ));
    // Profile path: the same selection from the same config.
    let profile = Config {
        providers: vec![ProviderConfig {
            name: "acme".into(),
            command: "newt-provider-openai".into(),
            model: None,
            env_pass: vec![],
            tiers: vec![],
        }],
        backends: vec![],
        ..Default::default()
    };
    let resolved = profile.prepare_runtime().unwrap();
    assert!(resolved.backends.is_empty());
    assert!(matches!(
        resolved.select_backend(),
        SelectionOutcome::Selected(SelectedBackend::Provider(p)) if p.name == "acme"
    ));
    // Fully bare (no providers either): the localhost fallback remains.
    std::fs::write(dir.path().join("config.toml"), "# empty\n").unwrap();
    let resolved = Config::resolve_runtime_unpublished().unwrap();
    assert_eq!(resolved.backends.len(), 1);
    assert_eq!(resolved.backends[0].name, "ollama");
    assert!(resolved.is_unconfigured());
}

/// K: requested-slot pinning applies to the RUNTIME composers too —
/// with a stale `default_backend = a`, an exclusive or NAMED request
/// for `b` and NO CLI-installed env, the composed config must select
/// `b` (default re-pointed), never resolve Unknown/None against a
/// config that plainly contains it.
#[test]
#[serial_test::serial(real_fs)] // mutates the CLI-override global + env
fn runtime_composers_pin_selection_to_the_requested_slot() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    // The process-global CLI override is not guard-covered — clear it
    // on every exit path.
    struct OverrideGuard;
    impl Drop for OverrideGuard {
        fn drop(&mut self) {
            set_cli_backend_override(BackendOverride::default());
        }
    }
    let _o = OverrideGuard;
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
    // NAMED field-only request for b (profile composer).
    set_cli_backend_override(BackendOverride {
        name: Some("b".into()),
        model: Some("m".into()),
        ..Default::default()
    });
    let resolved = base().prepare_runtime().unwrap();
    assert_eq!(resolved.default_backend.as_deref(), Some("b"));
    let picked = resolved.selected_backend().expect("b selects");
    assert_eq!(picked.backend.name, "b");
    assert!(
        picked.receipt.request.is_some(),
        "receipt on the pinned slot"
    );
    // Exclusive destination request (profile composer): the surviving
    // slot is the selection — no stale default naming a discarded a.
    set_cli_backend_override(BackendOverride {
        endpoint: Some("http://new:9".into()),
        ..Default::default()
    });
    let resolved = base().prepare_runtime().unwrap();
    assert_eq!(resolved.default_backend.as_deref(), Some("cli"));
    let picked = resolved
        .selected_backend()
        .expect("the exclusive slot selects");
    assert_eq!(picked.backend.name, "cli");
    assert_eq!(picked.slot, 0);
}

/// Embedded destinations are intrinsically Instance (`derive_serving`):
/// a model_path route never composes with `serving = multiplexer` — a
/// declared/inherited multiplexer normalizes to Instance, and the
/// EXPLICIT contradictions (destination request + serving, field-only
/// serving on an embedded target) refuse atomically.
#[test]
fn an_embedded_route_never_composes_as_a_multiplexer() {
    // Declaration: model_path + declared multiplexer → Instance.
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "emb".into(),
            model_path: Some("/m.gguf".into()),
            serving: Some(Serving::Multiplexer),
            ..Default::default()
        }],
        ..Default::default()
    };
    resolve_for_test(&mut cfg, &[], None).unwrap();
    assert_eq!(cfg.backends[0].kind, Some(BackendKind::Embedded));
    assert_eq!(
        cfg.backends[0].serving,
        Some(Serving::Instance),
        "an embedded route serves exactly one artifact"
    );
    // Exclusive retarget: HTTP multiplexer → model_path inherits the
    // declared serving, normalized to Instance.
    let http_mux = || Config {
        backends: vec![BackendConfig {
            name: "mux".into(),
            endpoint: "http://m:1".into(),
            serving: Some(Serving::Multiplexer),
            ..Default::default()
        }],
        ..Default::default()
    };
    let retarget = BackendOverride {
        name: Some("mux".into()),
        model_path: Some("/m.gguf".into()),
        ..Default::default()
    };
    let mut cfg = http_mux();
    resolve_for_test(&mut cfg, &[], Some(retarget.clone())).unwrap();
    assert_eq!(cfg.backends[0].serving, Some(Serving::Instance));
    assert_eq!(cfg.backends[0].kind, Some(BackendKind::Embedded));
    let mut cfg = http_mux();
    retarget.try_apply(&mut cfg).unwrap();
    assert_eq!(
        cfg.backends[0].serving,
        Some(Serving::Instance),
        "try_apply too"
    );
    // EXPLICIT model_path + serving=multiplexer: refused atomically.
    let contradictory = BackendOverride {
        name: Some("mux".into()),
        model_path: Some("/m.gguf".into()),
        serving: Some(Serving::Multiplexer),
        ..Default::default()
    };
    let mut cfg = http_mux();
    let err = resolve_for_test(&mut cfg, &[], Some(contradictory.clone()))
        .expect_err("explicit contradiction refuses");
    assert!(err.contains("contradictory"), "{err}");
    let mut cfg = http_mux();
    assert!(contradictory.try_apply(&mut cfg).is_err());
    assert_eq!(
        cfg.backends[0].endpoint, "http://m:1",
        "untouched on refusal"
    );
    // Field-only serving=multiplexer on an embedded target: refused
    // atomically, target untouched.
    let emb = || Config {
        backends: vec![BackendConfig {
            name: "emb".into(),
            model_path: Some("/m.gguf".into()),
            kind: Some(BackendKind::Embedded),
            ..Default::default()
        }],
        ..Default::default()
    };
    let field_only = BackendOverride {
        name: Some("emb".into()),
        serving: Some(Serving::Multiplexer),
        ..Default::default()
    };
    let mut cfg = emb();
    let err = resolve_for_test(&mut cfg, &[], Some(field_only.clone()))
        .expect_err("field-only serving refuses on embedded");
    assert!(
        err.contains("emb") && err.contains("contradictory"),
        "{err}"
    );
    let mut cfg = emb();
    assert!(field_only.try_apply(&mut cfg).is_err());
    assert_eq!(cfg.backends[0].serving, None, "untouched on refusal");
    // Control: serving=multiplexer on an HTTP target stays legitimate.
    let mut cfg = http_mux();
    resolve_for_test(
        &mut cfg,
        &[],
        Some(BackendOverride {
            name: Some("mux".into()),
            serving: Some(Serving::Multiplexer),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(cfg.backends[0].serving, Some(Serving::Multiplexer));
}

use super::*;

// Backend assembly: declaration/probe layering and binding receipts.

// ── input-footer mode ──────────────────────────────────────────────

/// #1786/#1819: a REAL multiplexer probe writeback followed by the REAL
/// disk merge retains the operator\'s declared model/card/capability
/// across a restart — the probe_v1 overlay has no fields to clear them
/// with, and its observed kind/serving still apply.
#[test]
#[serial_test::serial(real_fs)] // pins NEWT_CONFIG_DIR, like its writeback sibling
fn inline_declarations_survive_a_mux_probe_writeback_and_restart_merge() {
    let declared = BackendConfig {
        name: "dgx1".into(),
        endpoint: "http://dgx:8000".into(),
        model: Some("bound-model".into()),
        card: Some("team-reasoner".into()),
        capability: Some(crate::model_card::Capability {
            emits_leading_reasoning: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let home = tempfile::tempdir().unwrap();
    let _env = ConfigDirGuard::set(home.path());
    std::fs::write(home.path().join("config.toml"), "# cfg\n").unwrap();
    let observation = ProbeObservation {
        name: "dgx1".into(),
        endpoint: "http://dgx:8000".into(),
        kind: Some(BackendKind::Openai),
        api: None,
        serving: ProbedServing::Multiplexer,
    };
    assert!(matches!(
        persist_probe_observation(&observation).expect("writeback runs"),
        ProbeWriteback::Written(_)
    ));

    // "Restart": a fresh config resolves the declared backend plus the
    // probe drop-in.
    let mut cfg = Config {
        backends: vec![declared],
        ..Default::default()
    };
    merge_for_test(&mut cfg, &[&home.path().join("backends")]).unwrap();
    let merged = &cfg.backends[0];
    assert_eq!(merged.card.as_deref(), Some("team-reasoner"));
    assert!(merged.capability.is_some());
    assert_eq!(
        merged.effective_model(),
        Some("bound-model"),
        "a mux writeback persists no model — the declaration stands"
    );
    assert_eq!(
        merged.kind,
        Some(BackendKind::Openai),
        "observed kind applies"
    );
    assert_eq!(merged.serving, Some(Serving::Multiplexer));
}

/// The legacy ambiguity refuses to load: a file carrying the EXACT old
/// newt-adopt probe marker plus a model (old writebacks merged INTO
/// operator files) cannot be attributed — the error names the file and
/// both remediations. (A probe timestamp WITHOUT that marker proves
/// nothing and stays operator — see the classification matrix test.)
#[test]
fn legacy_untagged_probe_stamped_model_fails_visibly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("dgx1.toml"),
            "endpoint = \"http://dgx:8000\"\nmodel = \"warm-pick\"\n\n[provenance]\nsource = \"newt adopt v0.7.9 (probed; delete this file to reset)\"\nprobed = \"2026-08-01\"\n",
        )
        .unwrap();
    let mut cfg = Config {
        backends: vec![],
        ..Default::default()
    };
    let err =
        merge_for_test(&mut cfg, &[dir.path()]).expect_err("the ambiguity must refuse to load");
    assert!(err.contains("dgx1.toml"), "names the file: {err}");
    assert!(
        err.contains("operator_v1"),
        "names the claim remediation: {err}"
    );
    assert!(
        err.contains("delete"),
        "offers the reset remediation: {err}"
    );
}

#[test]
fn disk_backends_load_per_file_by_stem_and_override_inline() {
    let dir = tempfile::tempdir().unwrap();
    // A minimal drop-in: name omitted (filename is authoritative), tiers
    // omitted (defaults empty), kind omitted (defaults ollama).
    std::fs::write(
        dir.path().join("dgx1.toml"),
        "endpoint = \"http://REDACTED-HOST:11434\"\nmodel = \"qwen3:30b\"\n",
    )
    .unwrap();
    // Malformed (missing required `endpoint`) is skipped, not fatal.
    std::fs::write(dir.path().join("broken.toml"), "model = \"x\"\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "not a backend").unwrap();

    let mut cfg = Config {
        // An inline backend of the same name that the drop-in should replace,
        // plus an unrelated one that must survive untouched.
        backends: vec![
            BackendConfig {
                name: "dgx1".into(),
                endpoint: "http://stale:11434".into(),
                model: Some("old-model".into()),
                model_path: None,
                tiers: vec![],
                kind: Some(BackendKind::Ollama),
                api: Default::default(),
                api_key_file: None,
                api_key_env: None,
                ..Default::default()
            },
            BackendConfig {
                name: "gpu-runner".into(),
                endpoint: "http://gpu-runner:11434".into(),
                model: Some("qwen2.5-coder:14b".into()),
                model_path: None,
                tiers: vec![],
                kind: Some(BackendKind::Ollama),
                api: Default::default(),
                api_key_file: None,
                api_key_env: None,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    merge_for_test(&mut cfg, &[dir.path()]).unwrap();

    // The drop-in replaced the inline dgx1 in place (no duplicate), gpu-runner kept.
    assert_eq!(cfg.backends.len(), 2, "only the valid .toml loads, no dup");
    let dgx1 = cfg.backends.iter().find(|b| b.name == "dgx1").unwrap();
    assert_eq!(dgx1.endpoint, "http://REDACTED-HOST:11434", "disk wins");
    assert_eq!(dgx1.effective_model(), Some("qwen3:30b"));
    assert_eq!(dgx1.kind, None, "absent kind means probe-at-connect");
    assert!(
        cfg.backends.iter().any(|b| b.name == "gpu-runner"),
        "gpu-runner kept"
    );
}

#[test]
fn probe_records_overlay_only_observed_fields_never_auth_or_tiers() {
    // A probe_v1 record structurally carries no auth/tiers, and the
    // loader's whitelist overlay never touches them — the config's
    // bearer token and tier assignment survive BY CONSTRUCTION, not by
    // inheritance heuristics.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("gpt41.toml"),
            "record = \"probe_v1\"\nendpoint = \"https://api.openai.com\"\nkind = \"openai\"\nserving = \"multiplexer\"\n",
        )
        .unwrap();
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "gpt41".into(),
            endpoint: "https://api.openai.com".into(),
            model: Some("gpt-4.1".into()),
            api_key_env: Some("OPENAI_API_KEY".into()),
            api_key_file: Some("/vault/openai".into()),
            tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
            ..Default::default()
        }],
        ..Default::default()
    };
    merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    let b = cfg.backends.iter().find(|b| b.name == "gpt41").unwrap();
    assert_eq!(b.kind, Some(BackendKind::Openai), "observed kind overlaid");
    assert_eq!(
        b.serving,
        Some(Serving::Multiplexer),
        "observed serving overlaid"
    );
    assert_eq!(b.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    assert_eq!(b.api_key_file.as_deref(), Some("/vault/openai"));
    assert_eq!(
        b.tiers,
        vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
        "tiers untouched"
    );
    assert_eq!(
        b.effective_model(),
        Some("gpt-4.1"),
        "a mux record leaves the declared model standing"
    );
}

#[test]
fn operator_record_omissions_clear_even_with_a_probe_timestamp() {
    // The TAG owns the merge semantics; BackendProvenance stays
    // informational. An operator_v1 file that happens to carry a probed
    // timestamp still replaces wholesale — its omissions deliberately
    // clear/rebind.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("eval.toml"),
            "record = \"operator_v1\"\nendpoint = \"http://router:8080\"\n\n[provenance]\nprobed = \"2026-08-01\"\n",
        )
        .unwrap();
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "eval".into(),
            endpoint: "http://router:8080".into(),
            model: Some("big-30b".into()),
            card: Some("team-reasoner".into()),
            api_key_env: Some("TOKEN".into()),
            tiers: vec![Tier::Fast, Tier::Standard],
            ..Default::default()
        }],
        ..Default::default()
    };
    merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    let b = cfg.backends.iter().find(|b| b.name == "eval").unwrap();
    assert_eq!(b.model, None, "omitted model clears");
    assert_eq!(
        b.card, None,
        "omitted card clears — rebinding stays possible"
    );
    assert_eq!(b.api_key_env, None, "omitted auth clears");
    assert!(b.tiers.is_empty(), "omitted tiers clear");
}

#[test]
fn probe_record_with_a_different_endpoint_does_not_overlay() {
    // Association is exact name PLUS endpoint: a probe of some other
    // endpoint may not rewrite this backend, whatever the filename says.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("gpt41.toml"),
        "record = \"probe_v1\"\nendpoint = \"http://other:9\"\nkind = \"ollama\"\n",
    )
    .unwrap();
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "gpt41".into(),
            endpoint: "https://api.openai.com".into(),
            kind: Some(BackendKind::Openai),
            ..Default::default()
        }],
        ..Default::default()
    };
    merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    assert_eq!(
        cfg.backends[0].kind,
        Some(BackendKind::Openai),
        "not overlaid"
    );
}

#[test]
fn probe_record_for_an_unconfigured_backend_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("ghost.toml"),
        "record = \"probe_v1\"\nendpoint = \"http://h:1\"\nkind = \"ollama\"\n",
    )
    .unwrap();
    let mut cfg = Config {
        backends: vec![],
        ..Default::default()
    };
    merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    assert!(
        cfg.backends.is_empty(),
        "a probe OVERLAY cannot define a backend"
    );
}

/// P0 (#1819 review): the probe overlay may rewrite the backend's live
/// `model`, but the card-binding SEED is captured first — declared
/// A/cardA + probed Instance B seeds cardA bound to A, so the session's
/// principal (B) is an exact mux/selected MISMATCH (typed inactive),
/// never a silent rebind of cardA onto B.
#[test]
fn probe_overlay_preserves_the_declared_binding_seed() {
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
            card: Some("team-reasoner".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let receipts = merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    let b = cfg.backends.iter().find(|b| b.name == "dgx1").unwrap();
    assert_eq!(
        b.effective_model(),
        Some("probed-b"),
        "the live route adopts the probed instance model"
    );
    let receipt = &receipts[0];
    assert_eq!(
        receipt.declaration.model.as_deref(),
        Some("declared-a"),
        "the declaration layer never absorbs a probe result"
    );
    assert_eq!(
        receipt.observation.as_ref().map(|o| &o.serving),
        Some(&ProbedServing::Instance {
            model: Some("probed-b".into())
        }),
        "the probed model is recorded as an OBSERVATION"
    );
    assert_eq!(receipt.binding.card.as_deref(), Some("team-reasoner"));
    assert_eq!(
        receipt.binding.bound_model.as_deref(),
        Some("declared-a"),
        "the binding evidence is the DECLARATION, not the probe result — \
             deciding for principal `probed-b` is an exact mismatch (inactive)"
    );
    assert_eq!(
        receipt.binding.bound_destination,
        BackendDestination::new(Some("http://dgx:8000".into()), None)
    );
}

/// Old `newt setup` / `newt init` files are untagged AND probe-stamped
/// AND carry a model — but their source marker identifies an operator
/// writer, so they classify as operator records, not as the ambiguity.
#[test]
fn setup_written_untagged_files_stay_operator() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("lab.toml"),
            "endpoint = \"http://lab:8000\"\nmodel = \"qwen3:30b\"\nkind = \"openai\"\n\n\
             [provenance]\nsource = \"newt setup v0.7.9 (auto-detected Openai)\"\nprobed = \"2026-07-01\"\nderived_serving = true\n",
        )
        .unwrap();
    let mut cfg = Config {
        backends: vec![],
        ..Default::default()
    };
    merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    assert_eq!(cfg.backends.len(), 1, "loads as an operator definition");
    assert_eq!(cfg.backends[0].effective_model(), Some("qwen3:30b"));
}

/// A legacy model-less adopt cache (the exact old runtime-writer marker,
/// probe-shaped) overlays like a probe record — it must NOT wholesale-
/// replace and clear the config's declarations.
#[test]
fn legacy_adopt_probe_cache_overlays_without_clearing_declarations() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("dgx1.toml"),
            "endpoint = \"http://dgx:8000\"\nkind = \"openai\"\nserving = \"multiplexer\"\ntiers = []\n\n\
             [provenance]\nsource = \"newt adopt v0.8.0 abcdef123456 (probed; delete this file to reset)\"\nprobed = \"2026-08-01\"\nderived_serving = true\n",
        )
        .unwrap();
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "dgx1".into(),
            endpoint: "http://dgx:8000".into(),
            model: Some("declared-a".into()),
            card: Some("team-reasoner".into()),
            api_key_env: Some("TOKEN".into()),
            tiers: vec![Tier::Fast],
            ..Default::default()
        }],
        ..Default::default()
    };
    merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    let b = &cfg.backends[0];
    assert_eq!(b.card.as_deref(), Some("team-reasoner"), "card survives");
    assert_eq!(b.api_key_env.as_deref(), Some("TOKEN"), "auth survives");
    assert_eq!(b.tiers, vec![Tier::Fast], "tiers survive");
    assert_eq!(b.effective_model(), Some("declared-a"), "model survives");
    assert_eq!(b.kind, Some(BackendKind::Openai), "observed kind applies");
}

/// A legacy adopt-marked file that ALSO carries operator fields (the old
/// writeback merged into operator files) is the genuinely ambiguous
/// hybrid — hard error, both remediations named.
#[test]
fn legacy_adopt_hybrid_with_operator_fields_is_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("dgx1.toml"),
            "endpoint = \"http://dgx:8000\"\nmodel = \"warm-pick\"\napi_key_env = \"TOKEN\"\n\n\
             [provenance]\nsource = \"newt adopt v0.7.9 (probed; delete this file to reset)\"\nprobed = \"2026-08-01\"\n",
        )
        .unwrap();
    let mut cfg = Config {
        backends: vec![],
        ..Default::default()
    };
    let err = merge_for_test(&mut cfg, &[dir.path()]).expect_err("hybrids refuse to load");
    assert!(
        err.contains("operator_v1") && err.contains("delete"),
        "{err}"
    );
}

/// A `probe_v1` record smuggling operator-owned fields (or a model with
/// no instance serving) is rejected whole — nothing overlays, the
/// declarations stand.
#[test]
fn probe_record_smuggling_operator_fields_is_rejected() {
    for body in [
            // card through the machine channel
            "record = \"probe_v1\"\nendpoint = \"http://h:1\"\nkind = \"ollama\"\ncard = \"evil\"\n",
            // model without instance serving
            "record = \"probe_v1\"\nendpoint = \"http://h:1\"\nserving = \"multiplexer\"\nmodel = \"b\"\n",
            // no endpoint (no association key)
            "record = \"probe_v1\"\nkind = \"ollama\"\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("gpt41.toml"), body).unwrap();
            let mut cfg = Config {
                backends: vec![BackendConfig {
                    name: "gpt41".into(),
                    endpoint: "http://h:1".into(),
                    kind: Some(BackendKind::Openai),
                    ..Default::default()
                }],
                ..Default::default()
            };
            merge_for_test(&mut cfg, &[dir.path()]).unwrap();
            assert_eq!(
                cfg.backends[0].kind,
                Some(BackendKind::Openai),
                "nothing overlays from an invalid probe record: {body}"
            );
            assert_eq!(cfg.backends[0].card, None);
        }
}

// ── the backend assembly: identity, slots, layers (#1819) ─────────

/// Backend identity is validated on EVERY assembly path — normal
/// resolve and profiles alike: duplicate names (which could hand A the
/// card declared for B) and empty names are hard, actionable errors.
#[test]
fn backend_identity_is_validated_on_normal_and_profile_paths() {
    let dup = || {
        vec![
            BackendConfig {
                name: "twin".into(),
                endpoint: "http://a:1".into(),
                model: Some("model-a".into()),
                card: Some("card-a".into()),
                ..Default::default()
            },
            BackendConfig {
                name: "twin".into(),
                endpoint: "http://b:2".into(),
                model: Some("model-b".into()),
                card: Some("card-b".into()),
                ..Default::default()
            },
        ]
    };
    // Normal path: the assembly constructor refuses.
    let err = BackendAssembly::new(dup()).expect_err("duplicates refuse");
    assert!(err.contains("twin") && err.contains("rename one"), "{err}");
    // Profile path: the same shared validation, through prepare_runtime.
    let cfg = Config {
        backends: dup(),
        ..Default::default()
    };
    let err = cfg.prepare_runtime().expect_err("profiles validate too");
    assert!(err.to_string().contains("twin"), "{err}");
    // Empty name.
    let err = BackendAssembly::new(vec![BackendConfig {
        name: "  ".into(),
        endpoint: "http://a:1".into(),
        ..Default::default()
    }])
    .expect_err("empty names refuse");
    assert!(err.contains("has no name"), "{err}");
}

/// Receipts align 1:1 BY SLOT with `backends`; indexed and zipped
/// access agree, and resolved selection uses the same index selector as
/// `select_configured_backend`, so the two can never disagree.
#[test]
fn receipts_align_by_slot_and_selection_agrees() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("second.toml"),
        "record = \"probe_v1\"\nendpoint = \"http://b:2\"\nkind = \"openai\"\n",
    )
    .unwrap();
    let mut cfg = Config {
        backends: vec![
            BackendConfig {
                name: "first".into(),
                endpoint: "http://a:1".into(),
                ..Default::default()
            },
            BackendConfig {
                name: "second".into(),
                endpoint: "http://b:2".into(),
                ..Default::default()
            },
        ],
        default_backend: Some("second".into()),
        ..Default::default()
    };
    let receipts = merge_for_test(&mut cfg, &[dir.path()]).unwrap();
    assert_eq!(receipts.len(), cfg.backends.len(), "1:1 by construction");
    assert!(receipts[0].observation.is_none());
    assert!(
        receipts[1].observation.is_some(),
        "the probe attached to ITS slot"
    );
    let resolved = ResolvedConfig {
        config: cfg,
        receipts,
    };
    let rb = resolved.backend(1).expect("slot 1 exists");
    assert_eq!(rb.slot, 1);
    assert_eq!(rb.backend.name, "second");
    assert!(rb.receipt.observation.is_some());
    assert!(resolved.backend(2).is_none(), "out of range is None");
    let zipped: Vec<(usize, &str)> = resolved
        .backends()
        .map(|rb| (rb.slot, rb.backend.name.as_str()))
        .collect();
    assert_eq!(zipped, vec![(0, "first"), (1, "second")]);
    // Selection: default_backend names slot 1 — the receipt-bearing pick
    // and the borrowed pick agree by shared index selector.
    let picked = resolved.selected_backend().expect("default selects");
    assert_eq!(picked.slot, 1);
    assert_eq!(
        resolved
            .select_configured_backend()
            .map(|b| b.name.as_str()),
        Some("second"),
        "same slot through the Config surface"
    );
}

/// Three layers, in order: inline A/cardA declaration → a probe
/// observation attaches → a SKIPPED operator record (no destination)
/// touches NOTHING — declaration AND observation survive. A VALID
/// operator record then resets both.
#[test]
fn a_skipped_operator_record_touches_nothing_and_a_valid_one_resets() {
    let probe_dir = tempfile::tempdir().unwrap();
    std::fs::write(
            probe_dir.path().join("dgx1.toml"),
            "record = \"probe_v1\"\nendpoint = \"http://dgx:8000\"\nserving = \"instance\"\nmodel = \"probed-b\"\n",
        )
        .unwrap();
    let hollow_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        hollow_dir.path().join("dgx1.toml"),
        "record = \"operator_v1\"\nmodel = \"only-a-model\"\n",
    )
    .unwrap();
    let declared = BackendConfig {
        name: "dgx1".into(),
        endpoint: "http://dgx:8000".into(),
        model: Some("declared-a".into()),
        card: Some("card-a".into()),
        ..Default::default()
    };
    let mut cfg = Config {
        backends: vec![declared.clone()],
        ..Default::default()
    };
    let receipts = merge_for_test(&mut cfg, &[probe_dir.path(), hollow_dir.path()]).unwrap();
    let receipt = &receipts[0];
    assert_eq!(
        receipt.declaration.model.as_deref(),
        Some("declared-a"),
        "the skipped operator record must not strip the declaration"
    );
    assert!(
        receipt.observation.is_some(),
        "…nor the earlier probe observation"
    );
    assert_eq!(receipt.binding.card.as_deref(), Some("card-a"));

    // A VALID operator record replaces wholesale and resets the slot.
    let replace_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        replace_dir.path().join("dgx1.toml"),
        "record = \"operator_v1\"\nendpoint = \"http://new:9000\"\nmodel = \"fresh\"\n",
    )
    .unwrap();
    let mut cfg = Config {
        backends: vec![declared],
        ..Default::default()
    };
    let receipts = merge_for_test(&mut cfg, &[probe_dir.path(), replace_dir.path()]).unwrap();
    let receipt = &receipts[0];
    assert_eq!(receipt.declaration.model.as_deref(), Some("fresh"));
    assert_eq!(receipt.declaration.card, None, "reset — omissions clear");
    assert!(
        receipt.observation.is_none(),
        "the observation was about the OLD declaration — reset with it"
    );
}

/// The external validate → publish → keep-using-the-receipts flow:
/// publication reads (`&self`), so the receipt-bearing view survives it.
#[test]
fn validate_then_publish_then_keep_the_receipt_view() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    let cfg = Config {
        backends: vec![BackendConfig {
            name: "a".into(),
            endpoint: "http://a:1".into(),
            card: Some("card-a".into()),
            model: Some("model-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    cfg.validate_backend_identities().expect("valid first");
    let mut cfg = cfg;
    let receipts = resolve_for_test(&mut cfg, &[], None).unwrap();
    let resolved = ResolvedConfig {
        config: cfg,
        receipts,
    };
    resolved.publish_runtime_settings();
    // The same immutable view keeps answering AFTER publication.
    let picked = resolved.backend(0).expect("slot 0");
    assert_eq!(picked.receipt.binding.card.as_deref(), Some("card-a"));
}

/// Two-phase directory loading: a HOME-dir probe survives to be judged
/// against a PROJECT-dir operator declaration — attached on an exact
/// destination match, skipped on a mismatch — and a later probe record
/// deterministically supersedes an earlier one.
#[test]
fn a_home_probe_attaches_against_the_final_project_declaration() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("roamer.toml"),
        "record = \"probe_v1\"\nendpoint = \"http://e:8000\"\nkind = \"ollama\"\n",
    )
    .unwrap();
    // Exact match: the project dir DECLARES roamer at the probed
    // destination — the earlier probe attaches against it.
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("roamer.toml"),
        "record = \"operator_v1\"\nendpoint = \"http://e:8000\"\nmodel = \"declared\"\n",
    )
    .unwrap();
    let mut cfg = Config {
        backends: vec![],
        ..Default::default()
    };
    let receipts = merge_for_test(&mut cfg, &[home.path(), project.path()]).unwrap();
    assert!(
        receipts[0].observation.is_some(),
        "the home probe reached the project declaration"
    );
    assert_eq!(cfg.backends[0].kind, Some(BackendKind::Ollama));
    assert_eq!(cfg.backends[0].effective_model(), Some("declared"));
    // Mismatch: the project declaration moved — the probe is skipped.
    let moved = tempfile::tempdir().unwrap();
    std::fs::write(
        moved.path().join("roamer.toml"),
        "record = \"operator_v1\"\nendpoint = \"http://elsewhere:9\"\n",
    )
    .unwrap();
    let mut cfg = Config {
        backends: vec![],
        ..Default::default()
    };
    let receipts = merge_for_test(&mut cfg, &[home.path(), moved.path()]).unwrap();
    assert!(
        receipts[0].observation.is_none(),
        "a probe of E never attaches to a declaration at E2"
    );
    assert_eq!(cfg.backends[0].kind, None);
    // Probe precedence: a project-dir probe supersedes the home one.
    let project_probe = tempfile::tempdir().unwrap();
    std::fs::write(
        project_probe.path().join("roamer.toml"),
        "record = \"probe_v1\"\nendpoint = \"http://e:8000\"\nkind = \"openai\"\n",
    )
    .unwrap();
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "roamer".into(),
            endpoint: "http://e:8000".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let receipts = merge_for_test(&mut cfg, &[home.path(), project_probe.path()]).unwrap();
    assert_eq!(
        receipts[0].observation.as_ref().and_then(|o| o.kind),
        Some(BackendKind::Openai),
        "deterministic last-wins probe precedence"
    );
}

/// `model_path = ""` is not a destination: an empty-path drop-in cannot
/// pass the destination check and strip a valid earlier declaration.
///
/// #1984: asserts on the RETURNED warning value, not a scraped log — this
/// exact test flaked on PR #1982 (which touched zero config files) because
/// the pre-#1984 `captured_warnings` helper's per-test
/// `tracing::subscriber::with_default` capture raced tracing's
/// process-wide callsite interest cache against sibling tests in this file
/// doing the same thing concurrently; the returned-value shape has no
/// global dispatcher in the loop to race. See `BackendAssembly::warnings`'s
/// doc in config/backend.rs for the full mechanism.
#[test]
fn an_empty_model_path_dropin_cannot_replace_a_declaration() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("keep.toml"),
        "record = \"operator_v1\"\nmodel_path = \"\"\nmodel = \"stripper\"\n",
    )
    .unwrap();
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "keep".into(),
            endpoint: "http://old:1".into(),
            model: Some("declared".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (_receipts, warnings) = merge_for_test_with_warnings(&mut cfg, &[dir.path()]).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("neither endpoint nor model_path")),
        "{warnings:?}"
    );
    assert_eq!(cfg.backends[0].endpoint, "http://old:1");
    assert_eq!(cfg.backends[0].model.as_deref(), Some("declared"));
}

/// Preview/composition NORMALIZATION parity: a declaration with a
/// model_path and a stale HTTP kind composes to Embedded with no CLI
/// request — so the identical config must also accept a harmless
/// model-only edit (the preview normalizes the same way), never refuse
/// it as "unroutable".
#[test]
#[serial_test::serial(real_fs)] // reads NEWT_PROVIDER (guard-restored)
fn an_incoherent_model_path_declaration_normalizes_with_and_without_an_edit() {
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    // SAFETY: guard held; restored on drop.
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let base = || Config {
        backends: vec![BackendConfig {
            name: "weird".into(),
            model_path: Some("/m.gguf".into()),
            kind: Some(BackendKind::Openai),
            ..Default::default()
        }],
        ..Default::default()
    };
    // Without any request: composes, normalized to Embedded.
    let mut cfg = base();
    resolve_for_test(&mut cfg, &[], None).unwrap();
    assert_eq!(cfg.backends[0].kind, Some(BackendKind::Embedded));
    // With a harmless model-only edit: SAME acceptance, same shape.
    let mut cfg = base();
    let receipts = resolve_for_test(
        &mut cfg,
        &[],
        Some(BackendOverride {
            model: Some("m".into()),
            ..Default::default()
        }),
    )
    .expect("the preview normalizes exactly as composition does");
    assert!(receipts[0].request.is_some());
    assert_eq!(cfg.backends[0].kind, Some(BackendKind::Embedded));
    assert_eq!(cfg.backends[0].model.as_deref(), Some("m"));
}

/// L: empty/whitespace model strings never become receipt identity —
/// the declaration and request layers both normalize through the
/// effective-model rule before bindings are minted.
#[test]
fn empty_model_strings_never_become_receipt_identity() {
    // Declaration: model = "" + a card → binding bound to NO model.
    let mut cfg = Config {
        backends: vec![BackendConfig {
            name: "a".into(),
            endpoint: "http://a:1".into(),
            model: Some("".into()),
            card: Some("card-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let receipts = resolve_for_test(&mut cfg, &[], None).unwrap();
    assert_eq!(receipts[0].declaration.model, None, "effective-model rule");
    assert_eq!(receipts[0].binding.bound_model, None);
}

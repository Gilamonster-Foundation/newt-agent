use super::*;

// Drop-in ownership, probe record validation, claiming, and writeback.

#[serial_test::serial(real_fs)]
#[test]
fn writeback_does_not_carry_prior_fields_across_an_endpoint_change() {
    // E1's kind/api/serving/model must not be re-stamped under E2: an
    // endpoint change makes every prior observation someone else's.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "# cfg\n").unwrap();
    let _env = ConfigDirGuard::set(dir.path());

    let e1 = ProbeObservation {
        name: "roamer".into(),
        endpoint: "http://e1:8000".into(),
        kind: Some(BackendKind::Openai),
        api: Some(OpenAiApi::Responses),
        serving: ProbedServing::Instance {
            model: Some("b".into()),
        },
    };
    assert!(matches!(
        persist_probe_observation(&e1).unwrap(),
        ProbeWriteback::Written(_)
    ));
    let e2 = ProbeObservation {
        name: "roamer".into(),
        endpoint: "http://e2:9000".into(),
        kind: None,
        api: None,
        serving: ProbedServing::Unknown,
    };
    let ProbeWriteback::Written(path) = persist_probe_observation(&e2).unwrap() else {
        panic!("probe_v1 file updates");
    };
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("http://e2:9000"));
    for stale in ["kind =", "api =", "serving =", "model ="] {
        assert!(
            !body.contains(stale),
            "`{stale}` carried across the endpoint change: {body}"
        );
    }
}

#[serial_test::serial(real_fs)]
#[test]
fn writeback_creates_the_backends_dir_when_missing() {
    // Regression pin: the writer must work into a fresh config dir with
    // no backends/ subdir (today ResolvedPath::atomic_write creates it;
    // this keeps that load-bearing behavior observed).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "# cfg\n").unwrap();
    let _env = ConfigDirGuard::set(dir.path());

    let observation = ProbeObservation {
        name: "fresh".into(),
        endpoint: "http://h:1".into(),
        kind: Some(BackendKind::Ollama),
        api: None,
        serving: ProbedServing::Multiplexer,
    };
    let ProbeWriteback::Written(path) = persist_probe_observation(&observation).unwrap() else {
        panic!("must write into a freshly created backends dir");
    };
    assert!(path.is_file());
}

#[serial_test::serial(real_fs)]
#[test]
fn writeback_probed_backend_lands_in_dedicated_dropin_not_config_toml() {
    // Probe write-back must never touch config.toml — only
    // backends/<name>.toml, tagged `record = "probe_v1"`, so reset =
    // delete that one file. Serial: pins NEWT_CONFIG_DIR.
    let dir = tempfile::tempdir().unwrap();
    let config_toml = dir.path().join("config.toml");
    std::fs::write(&config_toml, "# keep me\n").unwrap();
    let _env = ConfigDirGuard::set(dir.path());

    let observation = ProbeObservation {
        name: "dgx1-llama".into(),
        endpoint: "http://host:8000".into(),
        kind: Some(BackendKind::Openai),
        api: Some(OpenAiApi::Responses),
        serving: ProbedServing::Instance {
            model: Some("nemotron".into()),
        },
    };
    let ProbeWriteback::Written(written) = persist_probe_observation(&observation).unwrap() else {
        panic!("user config dir is set — the record must write");
    };
    assert_eq!(written, dir.path().join("backends").join("dgx1-llama.toml"));
    let body = std::fs::read_to_string(&written).unwrap();
    assert!(body.contains("record = \"probe_v1\""), "tagged: {body}");
    assert!(body.contains("kind = \"openai\""));
    assert!(body.contains("api = \"responses\""));
    assert!(
        body.contains("model = \"nemotron\""),
        "an INSTANCE model is backend truth and persists: {body}"
    );
    assert!(body.contains("serving = \"instance\""));
    // Main config untouched.
    assert_eq!(
        std::fs::read_to_string(&config_toml).unwrap(),
        "# keep me\n"
    );

    // A later MULTIPLEXER observation on the same probe_v1 file REMOVES
    // the previously observed instance model — a mux pick is per-session
    // and has no field to persist through.
    let observation2 = ProbeObservation {
        name: "dgx1-llama".into(),
        endpoint: "http://host:8000".into(),
        kind: Some(BackendKind::Openai),
        api: Some(OpenAiApi::ChatCompletions),
        serving: ProbedServing::Multiplexer,
    };
    assert!(matches!(
        persist_probe_observation(&observation2).unwrap(),
        ProbeWriteback::Written(_)
    ));
    let body2 = std::fs::read_to_string(&written).unwrap();
    assert!(
        !body2.contains("model ="),
        "the instance model is removed by the mux rewrite: {body2}"
    );
    assert!(body2.contains("serving = \"multiplexer\""));
    assert!(body2.contains("api = \"chat_completions\""));
}

#[serial_test::serial(real_fs)]
#[test]
fn writeback_skips_an_operator_owned_file_byte_for_byte() {
    // Untagged and operator_v1 files are operator property: the runtime
    // returns a typed SkippedOperatorOwned outcome and leaves every byte
    // — comments included — untouched.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "# cfg\n").unwrap();
    let backends = dir.path().join("backends");
    std::fs::create_dir_all(&backends).unwrap();
    let _env = ConfigDirGuard::set(dir.path());

    let observation = ProbeObservation {
        name: "ops".into(),
        endpoint: "http://host:8000".into(),
        kind: Some(BackendKind::Openai),
        api: None,
        serving: ProbedServing::Multiplexer,
    };
    for body in [
        "# hand-authored\nendpoint = \"http://host:8000\"\n",
        "record = \"operator_v1\"\nendpoint = \"http://host:8000\"\n",
    ] {
        let path = backends.join("ops.toml");
        std::fs::write(&path, body).unwrap();
        let outcome = persist_probe_observation(&observation).unwrap();
        assert_eq!(outcome, ProbeWriteback::SkippedOperatorOwned(path.clone()));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            body,
            "byte-for-byte untouched"
        );
    }
}

/// The full legacy-ownership matrix: untagged is Operator by default —
/// a probe timestamp, a custom source, setup/init/preset markers, and
/// every near-collision of the adopt marker included. Only the fully
/// anchored exact newt-adopt marker classifies further: strict
/// model-less probe shape → Probe; ANY model/card/operator field
/// beside it → the hard ambiguity.
#[test]
fn legacy_ownership_classification_matrix() {
    const MARKER: &str = "newt adopt v0.7.9 (probed; delete this file to reset)";
    let with = |source: Option<&str>, probed: bool, f: fn(&mut BackendConfig)| {
        let mut b = BackendConfig {
            name: "x".into(),
            endpoint: "http://e:1".into(),
            provenance: Some(BackendProvenance {
                source: source.map(str::to_string),
                probed: probed.then(|| "2026-08-01".to_string()),
                derived_serving: None,
            }),
            ..Default::default()
        };
        f(&mut b);
        b
    };
    let operator_cases: &[(&str, BackendConfig)] = &[
        (
            "no provenance at all",
            BackendConfig {
                name: "x".into(),
                endpoint: "http://e:1".into(),
                model: Some("m".into()),
                ..Default::default()
            },
        ),
        (
            "probed timestamp, no source, model",
            with(None, true, |b| {
                b.model = Some("m".into());
            }),
        ),
        (
            "custom probed source, model",
            with(Some("my-tool 1.0"), true, |b| {
                b.model = Some("m".into());
            }),
        ),
        (
            "setup marker",
            with(
                Some("newt setup v0.7.9 (auto-detected Openai)"),
                true,
                |b| {
                    b.model = Some("m".into());
                },
            ),
        ),
        (
            "preset marker",
            with(Some("newt setup v0.7.3 (preset acme)"), true, |b| {
                b.model = Some("m".into());
            }),
        ),
        ("init marker", with(Some("newt init v0.8.0"), true, |_| {})),
        (
            "adopt near-suffix",
            with(
                Some("newt adopt v0.7.9 (probed; delete this file to reset)."),
                true,
                |_| {},
            ),
        ),
        (
            "adopt near-prefix",
            with(
                Some("my newt adopt v0.7.9 (probed; delete this file to reset)"),
                true,
                |_| {},
            ),
        ),
        (
            "adopt empty version",
            with(
                Some("newt adopt v (probed; delete this file to reset)"),
                true,
                |_| {},
            ),
        ),
    ];
    // The raw text for a constructed case is its own serialization (the
    // canonical shape — the raw-key cases below use literal fixtures).
    let classify = |b: &BackendConfig| classify_untagged_dropin(b, &toml::to_string(b).unwrap());
    for (what, b) in operator_cases {
        assert!(
            matches!(classify(b), Ok(DropinOwner::Operator)),
            "{what} must classify Operator"
        );
    }
    // The exact marker, strict model-less probe shape → Probe.
    assert!(matches!(
        classify(&with(Some(MARKER), true, |b| {
            b.kind = Some(BackendKind::Openai);
            b.serving = Some(Serving::Multiplexer);
        })),
        Ok(DropinOwner::Probe)
    ));
    // …even without the probe timestamp (the marker is the evidence).
    assert!(matches!(
        classify(&with(Some(MARKER), false, |_| {})),
        Ok(DropinOwner::Probe)
    ));
    // The exact marker + UNKNOWN evidence — judged on RAW keys (the
    // permissive parse would silently drop these): both remediations.
    for (what, raw) in [
            (
                "unknown top-level key",
                "endpoint = \"http://e:1\"\nwarm_pool = 3\n\n[provenance]\nsource = \"newt adopt v0.7.9 (probed; delete this file to reset)\"\nprobed = \"2026-08-01\"\n",
            ),
            (
                "unknown [provenance] key",
                "endpoint = \"http://e:1\"\n\n[provenance]\nsource = \"newt adopt v0.7.9 (probed; delete this file to reset)\"\nprobed = \"2026-08-01\"\nsmuggled = \"x\"\n",
            ),
        ] {
            let b: BackendConfig = toml::from_str(raw).unwrap();
            let err = classify_untagged_dropin(&b, raw).expect_err(what);
            assert!(
                err.contains("operator_v1") && err.contains("delete"),
                "{what}: both remediations named: {err}"
            );
        }
    // The exact marker + ANY binding/operator evidence → hard ambiguity.
    type Mutation = fn(&mut BackendConfig);
    let ambiguous_cases: &[(&str, Mutation)] = &[
        ("instance + model", |b| {
            b.serving = Some(Serving::Instance);
            b.model = Some("m".into());
        }),
        ("multiplexer + model", |b| {
            b.serving = Some(Serving::Multiplexer);
            b.model = Some("m".into());
        }),
        ("card", |b| b.card = Some("c".into())),
        ("auth", |b| b.api_key_env = Some("K".into())),
        ("tiers", |b| b.tiers = vec![Tier::Fast]),
        ("managed", |b| b.managed = Some(ManagedMode::Shared)),
    ];
    for (what, f) in ambiguous_cases {
        let err = classify(&with(Some(MARKER), true, *f)).expect_err(what);
        assert!(
            err.contains("operator_v1") && err.contains("delete"),
            "{what}: both remediations named: {err}"
        );
    }
}

/// The public ownership boundary: classification, the canonical
/// operator render (shared with the writer), and the comment-preserving
/// claim/retag — without the raw tag vocabulary in the API.
#[test]
fn the_dropin_ownership_boundary_classifies_stamps_and_claims() {
    // Classification.
    assert_eq!(
        classify_backend_dropin("record = \"operator_v1\"\nendpoint = \"http://e:1\"\n"),
        Ok(DropinOwnership::Operator)
    );
    assert_eq!(
        classify_backend_dropin("record = \"probe_v1\"\nendpoint = \"http://e:1\"\n"),
        Ok(DropinOwnership::Probe)
    );
    assert_eq!(
        classify_backend_dropin("# hand-authored\nendpoint = \"http://e:1\"\n"),
        Ok(DropinOwnership::Operator)
    );
    assert_eq!(
            classify_backend_dropin(
                "endpoint = \"http://e:1\"\nkind = \"openai\"\n\n[provenance]\nsource = \"newt adopt v0.7.9 (probed; delete this file to reset)\"\nprobed = \"2026-08-01\"\n"
            ),
            Ok(DropinOwnership::Probe),
            "the unambiguous legacy probe cache"
        );
    assert!(
        classify_backend_dropin("endpoint = 42\n").is_err(),
        "malformed"
    );
    let err = classify_backend_dropin(
            "endpoint = \"http://e:1\"\nmodel = \"m\"\n\n[provenance]\nsource = \"newt adopt v0.7.9 (probed; delete this file to reset)\"\n"
        )
        .expect_err("the ambiguity is an error here too");
    assert!(
        err.contains("operator_v1") && err.contains("delete"),
        "{err}"
    );

    // The canonical render IS what the writer writes.
    let backend = BackendConfig {
        name: "ops".into(),
        endpoint: "http://e:1".into(),
        model: Some("m".into()),
        ..Default::default()
    };
    let rendered = render_operator_backend_dropin(&backend).unwrap();
    assert_eq!(
        classify_backend_dropin(&rendered),
        Ok(DropinOwnership::Operator)
    );
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "# cfg\n").unwrap();
    let written = write_backend_dropin(&config_path, &backend).unwrap();
    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        rendered,
        "one renderer, shared by the core writer"
    );

    // Claim/retag: comments, key order, and unknown keys preserved; the
    // stamp lands TOP-LEVEL even when a [provenance] table follows.
    let probe_text = "# probed by newt\nendpoint = \"http://e:1\" # the server\nrecord = \"probe_v1\"\nfuture_key = 1\n\n[provenance]\nprobed = \"2026-08-01\"\n";
    let claimed = claim_backend_dropin_as_operator(probe_text).unwrap();
    assert_eq!(
        classify_backend_dropin(&claimed),
        Ok(DropinOwnership::Operator)
    );
    for preserved in [
        "# probed by newt",
        "# the server",
        "future_key = 1",
        "[provenance]",
    ] {
        assert!(claimed.contains(preserved), "`{preserved}` lost: {claimed}");
    }
    assert!(!claimed.contains("probe_v1"), "retagged: {claimed}");
    // Untagged file with a trailing table: the new stamp must not land
    // inside [provenance].
    let untagged = "endpoint = \"http://e:1\"\n\n[provenance]\nprobed = \"2026-08-01\"\n";
    let claimed = claim_backend_dropin_as_operator(untagged).unwrap();
    assert_eq!(
        classify_backend_dropin(&claimed),
        Ok(DropinOwnership::Operator)
    );
    // Idempotent.
    assert_eq!(
        claim_backend_dropin_as_operator(&claimed).unwrap(),
        claimed,
        "claiming an operator file changes nothing"
    );
    assert!(
        claim_backend_dropin_as_operator("endpoint = \n").is_err(),
        "claiming non-TOML errors"
    );
}

/// Claiming preserves the `record` line's OWN decor — the trailing
/// ownership note survives the retag byte-for-byte, with exact output
/// order/comments/unknown keys and idempotence.
#[test]
fn claiming_preserves_the_record_lines_own_comment() {
    let probe_text = "\
# machine-written cache
record = \"probe_v1\"  # ownership note: delete to re-probe
endpoint = \"http://e:1\" # the server
future_key = 1

[provenance]
probed = \"2026-08-01\"
";
    let claimed = claim_backend_dropin_as_operator(probe_text).unwrap();
    let expected = probe_text.replace("probe_v1", "operator_v1");
    assert_eq!(claimed, expected, "ONLY the tag value changes");
    assert_eq!(
        claim_backend_dropin_as_operator(&claimed).unwrap(),
        claimed,
        "idempotent"
    );
    assert_eq!(
        classify_backend_dropin(&claimed),
        Ok(DropinOwnership::Operator)
    );
}

/// Claiming refuses to overwrite a `[record]` table or `[[record]]`
/// array — those are someone's data, not an ownership tag.
#[test]
fn claiming_refuses_a_record_table() {
    for body in [
        "endpoint = \"http://e:1\"\n\n[record]\nx = 1\n",
        "endpoint = \"http://e:1\"\n\n[[record]]\nx = 1\n",
    ] {
        let err = claim_backend_dropin_as_operator(body).expect_err("a record table is not a tag");
        assert!(err.contains("refusing"), "{err}");
    }
}

/// The operator writer stamps `operator_v1` at the FILE boundary —
/// `BackendConfig` has no tag field to launder through it — and the
/// private header reader sees exactly that tag.
#[test]
fn the_operator_writer_stamps_the_tag_at_the_file_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "# cfg\n").unwrap();
    let backend = BackendConfig {
        name: "ops".into(),
        endpoint: "http://host:8000".into(),
        model: Some("m".into()),
        ..Default::default()
    };
    let path = write_backend_dropin(&config_path, &backend).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.starts_with("record = \"operator_v1\"\n"),
        "stamped first: {body}"
    );
    assert_eq!(
        disk_record_tag(&body).unwrap(),
        Some(RecordTag::OperatorV1),
        "the header reader agrees"
    );
    // And the loader treats it as an operator definition.
    let mut cfg = Config {
        backends: vec![],
        ..Default::default()
    };
    merge_for_test(&mut cfg, &[path.parent().unwrap()]).unwrap();
    assert_eq!(cfg.backends.len(), 1);
    assert_eq!(cfg.backends[0].effective_model(), Some("m"));
}

/// An unambiguous LEGACY probe cache (untagged, exact old adopt marker,
/// probe-shaped) migrates to tagged `probe_v1` through the typed
/// writeback — and an endpoint change afterwards clears every piece of
/// the old serving/model evidence.
#[serial_test::serial(real_fs)]
#[test]
fn a_legacy_probe_cache_migrates_to_probe_v1_through_typed_writeback() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "# cfg\n").unwrap();
    let backends = dir.path().join("backends");
    std::fs::create_dir_all(&backends).unwrap();
    let _env = ConfigDirGuard::set(dir.path());
    let path = backends.join("roamer.toml");
    std::fs::write(
            &path,
            "endpoint = \"http://e1:8000\"\nkind = \"openai\"\nserving = \"instance\"\ntiers = []\n\n\
             [provenance]\nsource = \"newt adopt v0.7.9 (probed; delete this file to reset)\"\nprobed = \"2026-08-01\"\n",
        )
        .unwrap();
    // Same endpoint: the legacy cache is the prior probe record —
    // refresh migrates it to a tagged probe_v1 (kind carried forward).
    let observation = ProbeObservation {
        name: "roamer".into(),
        endpoint: "http://e1:8000".into(),
        kind: None,
        api: None,
        serving: ProbedServing::Multiplexer,
    };
    assert!(matches!(
        persist_probe_observation(&observation).unwrap(),
        ProbeWriteback::Written(_)
    ));
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("record = \"probe_v1\""), "migrated: {body}");
    assert!(body.contains("kind = \"openai\""), "same-endpoint carry");
    assert!(body.contains("serving = \"multiplexer\""));
    // Endpoint change: nothing of E1 survives under E2.
    let moved = ProbeObservation {
        name: "roamer".into(),
        endpoint: "http://e2:9000".into(),
        kind: None,
        api: None,
        serving: ProbedServing::Unknown,
    };
    assert!(matches!(
        persist_probe_observation(&moved).unwrap(),
        ProbeWriteback::Written(_)
    ));
    let body = std::fs::read_to_string(&path).unwrap();
    for stale in ["kind =", "serving =", "model =", "e1:8000"] {
        assert!(!body.contains(stale), "`{stale}` survived the move: {body}");
    }
}

/// The deprecated `writeback_probed_backend` wrapper keeps its source
/// signature but NEVER reports a lossy conversion as success: a valid
/// instance patch writes a probe_v1 record (`Ok(Some(path))`);
/// unrepresentable patches (model off-instance, operator-owned fields)
/// error BEFORE any write; an operator-owned same-name file is a
/// path-bearing error with the bytes untouched.
#[serial_test::serial(real_fs)]
#[test]
#[allow(deprecated)]
fn the_deprecated_writeback_wrapper_never_reports_lossy_success() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "# cfg\n").unwrap();
    let _env = ConfigDirGuard::set(dir.path());
    let patch = BackendConfig {
        name: "compat".into(),
        endpoint: "http://h:1".into(),
        serving: Some(Serving::Instance),
        model: Some("m".into()),
        ..Default::default()
    };
    // Valid Instance+model: persists through the typed channel.
    let path = writeback_probed_backend(&patch)
        .unwrap()
        .expect("writes through the typed channel");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("record = \"probe_v1\""));
    assert!(body.contains("model = \"m\""));
    // Model without Instance serving: error BEFORE any write — the
    // existing probe file's bytes stay put.
    let before = std::fs::read_to_string(&path).unwrap();
    let mux = BackendConfig {
        serving: Some(Serving::Multiplexer),
        ..patch.clone()
    };
    let err = writeback_probed_backend(&mux).expect_err("lossy model is refused");
    assert!(err.contains("instance"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "no write");
    // An operator-owned field: refused before any write, too.
    let smuggle = BackendConfig {
        api_key_env: Some("TOKEN".into()),
        ..patch.clone()
    };
    let err = writeback_probed_backend(&smuggle).expect_err("operator fields refused");
    assert!(err.contains("api_key_env"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "no write");
    // Operator-owned same-name file: a path-bearing error, bytes intact.
    let operator_body = "# mine\nrecord = \"operator_v1\"\nendpoint = \"http://h:1\"\n";
    std::fs::write(&path, operator_body).unwrap();
    let err = writeback_probed_backend(&patch).expect_err("skips are not silent");
    assert!(
        err.contains("compat.toml") && err.contains("operator-owned"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), operator_body);
}

/// The strict machine schema rejects EVERY operator-owned or unknown
/// key — top-level and nested — and a nonempty legacy `tiers`.
#[test]
fn probe_records_reject_every_operator_owned_and_unknown_key() {
    for (key, line) in [
        ("card", "card = \"x\""),
        ("capability", "capability = {}"),
        ("api_key_env", "api_key_env = \"K\""),
        ("api_key_file", "api_key_file = \"/f\""),
        ("managed", "managed = \"shared\""),
        ("host", "host = \"h\""),
        ("coexist", "coexist = true"),
        ("ram_gib", "ram_gib = 1.5"),
        ("engine", "engine = \"x\""),
        ("model_path", "model_path = \"/m\""),
        ("wholly unknown", "future_key = 1"),
    ] {
        let body = format!("record = \"probe_v1\"\nendpoint = \"http://h:1\"\n{line}\n");
        assert!(
            parse_probe_record(&body).is_err(),
            "`{key}` must not ride the machine channel"
        );
    }
    // Nested provenance smuggling is denied one level down, too.
    assert!(parse_probe_record(
            "record = \"probe_v1\"\nendpoint = \"http://h:1\"\n\n[provenance]\nprobed = \"2026-08-01\"\nsmuggled = \"x\"\n"
        )
        .is_err());
    // A nonempty legacy tiers is operator configuration.
    assert!(parse_probe_record(
        "record = \"probe_v1\"\nendpoint = \"http://h:1\"\ntiers = [\"FAST\"]\n"
    )
    .is_err());
    // …while the empty legacy `tiers = []` is tolerated on read.
    assert!(
        parse_probe_record("record = \"probe_v1\"\nendpoint = \"http://h:1\"\ntiers = []\n")
            .is_ok()
    );
}

#[test]
fn probe_observation_record_is_typed_only_instance_carries_model() {
    // The record derives from a TYPED observation: a multiplexer or
    // unknown observation has no model field to persist AT ALL, so a
    // per-session pick can never freeze into tomorrow's declared model —
    // and no probe record ever carries operator-owned fields.
    let base = ProbeObservation {
        name: "b".into(),
        endpoint: "http://h:1".into(),
        kind: Some(BackendKind::Openai),
        api: None,
        serving: ProbedServing::Multiplexer,
    };
    let mux = probe_machine_record(&base);
    assert_eq!(mux.model, None);
    assert_eq!(mux.serving, Some(Serving::Multiplexer));
    assert_eq!(mux.record, Some(RecordTag::ProbeV1));
    assert!(!toml::to_string(&mux).unwrap().contains("model ="));

    let unknown = probe_machine_record(&ProbeObservation {
        serving: ProbedServing::Unknown,
        ..base.clone()
    });
    assert_eq!(unknown.model, None);
    assert_eq!(unknown.serving, None, "nothing observed, nothing recorded");

    let instance = probe_machine_record(&ProbeObservation {
        serving: ProbedServing::Instance {
            model: Some("m".into()),
        },
        ..base
    });
    assert_eq!(instance.model.as_deref(), Some("m"));
    let body = toml::to_string(&instance).unwrap();
    for banned in ["card", "capability", "api_key", "managed", "host ="] {
        assert!(!body.contains(banned), "`{banned}` leaked into: {body}");
    }
}

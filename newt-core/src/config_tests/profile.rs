use super::*;

// Technique profiles, verification knobs, bundles, and profile selection.

// ── profile composition (technique library) ────────────────────────

#[test]
fn profile_parses_techniques_and_knobs() {
    let cfg: Config = toml::from_str(
        r#"
            [profiles.nemotron]
            techniques = ["knowledge_base", "verify_gate", "retry"]

            [profiles.nemotron.verify_gate]
            surface_match = "exact"

            [profiles.nemotron.retry]
            max_retries = 3
            "#,
    )
    .unwrap();
    let p = &cfg.profiles["nemotron"];
    assert!(p.validate().is_ok());
    assert!(p.enables("verify_gate") && p.enables("retry"));
    assert_eq!(
        p.verify_gate_knobs().surface_match,
        crate::verify_gate::SurfaceMatch::Exact
    );
    assert_eq!(p.retry_knobs().max_retries, 3);
}

#[test]
fn profile_knobs_default_when_unset() {
    // techniques named but no knob tables → defaults apply
    let p: ProfileConfig = toml::from_str("techniques = [\"verify_gate\", \"retry\"]").unwrap();
    assert_eq!(
        p.verify_gate_knobs().surface_match,
        crate::verify_gate::SurfaceMatch::Exact // the complete-gate default
    );
    assert_eq!(p.retry_knobs().max_retries, 2);
}

#[test]
fn profile_rejects_unknown_technique() {
    let p: ProfileConfig =
        toml::from_str("techniques = [\"knowledge_base\", \"teleport\"]").unwrap();
    let err = p.validate().unwrap_err();
    assert!(err.contains("teleport"), "err: {err}");
}

#[test]
fn profile_rejects_unmet_presupposition() {
    // retry presupposes verify_gate — listing retry alone is now a load-time error.
    let p: ProfileConfig = toml::from_str("techniques = [\"retry\"]").unwrap();
    let err = p.validate().unwrap_err();
    assert!(
        err.contains("retry") && err.contains("verify_gate") && err.contains("presupposes"),
        "err: {err}"
    );
    // …and adding verify_gate satisfies it.
    let ok: ProfileConfig = toml::from_str("techniques = [\"verify_gate\", \"retry\"]").unwrap();
    assert!(ok.validate().is_ok());
}

#[test]
fn registry_does_not_alter_the_resolved_technique_set() {
    // Golden: validate() accepts the nemotron set and the resolved order/membership
    // is byte-identical to the input — the registry adds checks, not behavior.
    let p: ProfileConfig =
        toml::from_str("techniques = [\"knowledge_base\", \"verify_gate\", \"retry\"]").unwrap();
    assert!(p.validate().is_ok());
    assert_eq!(p.techniques, vec!["knowledge_base", "verify_gate", "retry"]);
    for t in ["knowledge_base", "verify_gate", "retry"] {
        assert!(p.enables(t));
    }
}

#[test]
fn empty_profiles_is_the_default() {
    // no [profiles] table → empty map, behavior unchanged
    let cfg: Config = toml::from_str("").unwrap();
    assert!(cfg.profiles.is_empty());
    assert!(cfg.bundles.is_empty());
}

// ── bundles (the loadable kit unit) ────────────────────────────────

fn bundle_cfg() -> Config {
    toml::from_str(
        r#"
            [profiles.nemotron]
            techniques = ["knowledge_base", "verify_gate", "retry"]
            [profiles.qwen-coder]
            techniques = []

            [bundles.nemotron]
            about = "nemotron family support"
            applies_to = ["nemotron"]
            default_profile = "nemotron"
            families = { "nemotron" = "nemotron", "qwen" = "qwen-coder" }

            [bundles.review-heavy]              # use-case bundle: no applies_to
            default_profile = "nemotron"
            "#,
    )
    .unwrap()
}

#[test]
fn resolve_bundle_errors_on_unknown() {
    let cfg = bundle_cfg();
    assert!(cfg.resolve_bundle("nemotron").is_ok());
    let err = cfg.resolve_bundle("ghost").unwrap_err();
    assert!(err.contains("no such bundle"), "{err}");
}

#[test]
fn bundle_profile_for_family_exact_then_default() {
    let cfg = bundle_cfg();
    let b = cfg.resolve_bundle("nemotron").unwrap();
    // EXACT typed-family match — never a model-name prefix.
    assert_eq!(
        cfg.bundle_profile_for_family(b, Some("nemotron")),
        Some("nemotron")
    );
    assert_eq!(
        cfg.bundle_profile_for_family(b, Some("qwen")),
        Some("qwen-coder")
    );
    // An unmapped family — or no family at all — falls to the bundle's
    // default profile (the bundle was chosen; its default applies).
    assert_eq!(
        cfg.bundle_profile_for_family(b, Some("llama")),
        Some("nemotron")
    );
    assert_eq!(cfg.bundle_profile_for_family(b, None), Some("nemotron"));
}

#[test]
fn infer_bundle_only_from_exact_family() {
    let cfg = bundle_cfg();
    // The exact typed family → the nemotron bundle.
    assert_eq!(
        cfg.infer_bundle_for_family(Some("nemotron"))
            .map(|(n, _)| n),
        Some("nemotron")
    );
    // A family nothing names — and NO family (the qwen-LOOKING alias
    // with no exact card: labels are never evidence) → no inference.
    assert!(cfg.infer_bundle_for_family(Some("gpt")).is_none());
    assert!(cfg.infer_bundle_for_family(None).is_none());
    // A model-name-shaped string is NOT a family key: exact equality
    // only, no prefix matching.
    assert!(cfg.infer_bundle_for_family(Some("nemotron3:33b")).is_none());
}

#[test]
fn pick_active_profile_precedence() {
    let cfg = bundle_cfg();
    // 1. explicit --profile wins over everything.
    let p = cfg
        .pick_active_profile(Some("qwen-coder"), Some("nemotron"), Some("nemotron"))
        .unwrap()
        .unwrap();
    assert_eq!(p.name, "qwen-coder");
    assert_eq!(p.via, PickVia::Profile);
    // 2. --bundle resolves to its profile for the TYPED family.
    let p = cfg
        .pick_active_profile(None, Some("nemotron"), Some("nemotron"))
        .unwrap()
        .unwrap();
    assert_eq!(
        (p.name.as_str(), p.via),
        ("nemotron", PickVia::Bundle("nemotron".into()))
    );
    // 3. inferred from the exact family when neither flag is set —
    //    and family A → profile A, family gone → None (the refresh
    //    funnel re-derives per route transition).
    let p = cfg
        .pick_active_profile(None, None, Some("nemotron"))
        .unwrap()
        .unwrap();
    assert_eq!(p.via, PickVia::InferredBundle("nemotron".into()));
    assert!(cfg.pick_active_profile(None, None, None).unwrap().is_none());
    // 4. a card-less qwen-looking ALIAS has no family → no profile.
    assert!(cfg.pick_active_profile(None, None, None).unwrap().is_none());
    // an unknown explicit bundle is a hard error.
    assert!(cfg
        .pick_active_profile(None, Some("ghost"), Some("x"))
        .is_err());
}

#[test]
fn disk_bundles_load_per_file_by_stem() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("nemotron.toml"),
        "applies_to = [\"nemotron\"]\ndefault_profile = \"nemotron\"\n",
    )
    .unwrap();
    // a malformed drop-in must be skipped, not break loading
    std::fs::write(
        dir.path().join("broken.toml"),
        "applies_to = \"not-a-list\"\n",
    )
    .unwrap();
    // a non-toml file is ignored
    std::fs::write(dir.path().join("README.md"), "not a bundle").unwrap();

    let mut cfg = Config::default();
    cfg.merge_bundles_from_dir(dir.path());
    assert_eq!(cfg.bundles.len(), 1, "only the valid .toml loads");
    let b = cfg
        .bundles
        .get("nemotron")
        .expect("loaded by filename stem");
    assert_eq!(b.applies_to, vec!["nemotron"]);
    assert_eq!(b.default_profile.as_deref(), Some("nemotron"));
    // a disk file overrides an inline bundle of the same name (last-wins)
    cfg.bundles.insert("x".into(), BundleConfig::default());
    std::fs::write(dir.path().join("x.toml"), "about = \"from disk\"\n").unwrap();
    cfg.merge_bundles_from_dir(dir.path());
    assert_eq!(cfg.bundles["x"].about.as_deref(), Some("from disk"));
}

#[test]
fn surface_match_round_trips_lowercase() {
    let k: VerifyGateKnobs = toml::from_str("surface_match = \"prefix\"").unwrap();
    assert_eq!(k.surface_match, crate::verify_gate::SurfaceMatch::Prefix);
}

#[test]
fn resolve_profile_looks_up_validates_and_errors() {
    let cfg: Config = toml::from_str(
        r#"
            [profiles.nemotron]
            techniques = ["verify_gate"]
            [profiles.bad]
            techniques = ["teleport"]
            "#,
    )
    .unwrap();
    // known + valid → the profile
    assert!(cfg
        .resolve_profile("nemotron")
        .unwrap()
        .enables("verify_gate"));
    // known name but invalid technique → validation error
    assert!(cfg.resolve_profile("bad").unwrap_err().contains("teleport"));
    // unknown name → no-such-profile error, listing the known ones
    let err = cfg.resolve_profile("ghost").unwrap_err();
    assert!(
        err.contains("no such profile") && err.contains("nemotron"),
        "err: {err}"
    );
}

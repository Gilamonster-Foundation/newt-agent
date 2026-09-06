use super::*;

// Loadout composition, reference validation, and disk loading.

// ── loadouts (the top-level composition; inert until Slice 1) ───────

#[test]
fn loadout_parses_inline_and_validates_references() {
    let cfg: Config = toml::from_str(
        r#"
            [[backends]]
            name = "dgx"
            endpoint = "http://dgx.local:11434"
            model = "nemotron-3:33b"
            tiers = []

            [profiles.nemotron]
            techniques = ["knowledge_base", "verify_gate", "retry"]
            [bundles.nemotron]
            default_profile = "nemotron"

            [loadouts.dev-nemotron]
            provider = "dgx"
            model    = "nemotron@deep"
            kit      = "nemotron"
            profile  = "nemotron"
            role     = "python-developer"
            [loadouts.dev-nemotron.settings]
            num_ctx = 24576
            framing = "Ship small, verify."
            "#,
    )
    .unwrap();
    let l = &cfg.loadouts["dev-nemotron"];
    assert_eq!(l.provider.as_deref(), Some("dgx"));
    assert_eq!(l.model.as_deref(), Some("nemotron@deep"));
    assert_eq!(l.role.as_deref(), Some("python-developer"));
    assert_eq!(l.settings.as_ref().unwrap().num_ctx, Some(24576));
    // references resolve
    assert!(l.validate(&cfg).is_ok());
}

#[test]
fn loadout_rejects_dangling_references() {
    let cfg: Config = toml::from_str(
        r#"
            [[backends]]
            name = "real-box"
            endpoint = "http://h:11434"
            model = "m"

            [profiles.nemotron]
            techniques = ["verify_gate"]
            "#,
    )
    .unwrap();
    // dangling kit
    let bad_kit = Loadout {
        kit: Some("ghost-bundle".into()),
        ..Default::default()
    };
    let e = bad_kit.validate(&cfg).unwrap_err();
    assert!(
        e.contains("kit 'ghost-bundle'") && e.contains("no such bundle"),
        "{e}"
    );
    // dangling profile
    let bad_profile = Loadout {
        profile: Some("ghost-profile".into()),
        ..Default::default()
    };
    let e = bad_profile.validate(&cfg).unwrap_err();
    assert!(
        e.contains("profile 'ghost-profile'") && e.contains("no such profile"),
        "{e}"
    );
    // dangling provider — must name a [backends] entry (Slice 2). The error
    // lists the known backends, here the explicit `real-box`.
    let bad_provider = Loadout {
        provider: Some("ghost-provider".into()),
        ..Default::default()
    };
    let e = bad_provider.validate(&cfg).unwrap_err();
    assert!(
        e.contains("provider 'ghost-provider'")
            && e.contains("no [backends] entry")
            && e.contains("real-box"),
        "{e}"
    );
    // an empty loadout is valid (no references)
    assert!(Loadout::default().validate(&cfg).is_ok());
}

#[test]
fn disk_loadouts_load_per_file_by_stem() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("dev-nemotron.toml"),
        "provider = \"dgx\"\nmodel = \"nemotron@deep\"\nkit = \"nemotron\"\n",
    )
    .unwrap();
    // a malformed drop-in must be skipped, not break loading
    std::fs::write(
        dir.path().join("broken.toml"),
        "provider = [\"not-a-string\"]\n",
    )
    .unwrap();
    // a non-toml file is ignored
    std::fs::write(dir.path().join("README.md"), "not a loadout").unwrap();

    let mut cfg = Config::default();
    cfg.merge_loadouts_from_dir(dir.path());
    assert_eq!(cfg.loadouts.len(), 1, "only the valid .toml loads");
    let l = cfg
        .loadouts
        .get("dev-nemotron")
        .expect("loaded by filename stem");
    assert_eq!(l.provider.as_deref(), Some("dgx"));
    assert_eq!(l.model.as_deref(), Some("nemotron@deep"));
    assert_eq!(l.kit.as_deref(), Some("nemotron"));
    // a disk file overrides an inline loadout of the same name (last-wins)
    cfg.loadouts.insert("x".into(), Loadout::default());
    std::fs::write(dir.path().join("x.toml"), "role = \"from-disk\"\n").unwrap();
    cfg.merge_loadouts_from_dir(dir.path());
    assert_eq!(cfg.loadouts["x"].role.as_deref(), Some("from-disk"));
}

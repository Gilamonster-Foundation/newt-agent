use super::*;

// Crew roles, dispatch constraints, and disk loading.

#[test]
fn crew_parses_inline_and_validates_role_references() {
    let cfg: Config = toml::from_str(
        r#"
            [[backends]]
            name = "dgx"
            endpoint = "http://dgx.local:11434"
            model = "qwen3-coder:30b"
            tiers = []
            [[backends]]
            name = "gpu-runner"
            endpoint = "http://localhost:11434"
            model = "qwen2.5-coder:3b"
            tiers = []

            [loadouts.planner]
            provider = "dgx"
            [loadouts.navigator]
            provider = "dgx"
            [loadouts.triage]
            provider = "gpu-runner"

            [crews.coder]
            planner = "planner"
            navigator = "navigator"
            triage = "triage"
            loop = "patch-revise"
            [crews.coder.budgets]
            max_attempts = 4
            require_human_review_on = ["auth", "crypto"]
            "#,
    )
    .unwrap();
    let c = &cfg.crews["coder"];
    assert_eq!(c.planner, "planner");
    assert_eq!(c.navigator.as_deref(), Some("navigator"));
    assert_eq!(c.loop_program.as_deref(), Some("patch-revise"));
    assert_eq!(c.budgets.as_ref().unwrap().max_attempts, Some(4));
    // each role names a known loadout, and each loadout validates
    assert!(c.validate(&cfg).is_ok());
}

#[test]
fn crew_rejects_dangling_and_invalid_roles() {
    let cfg: Config = toml::from_str(
        r#"
            [[backends]]
            name = "dgx"
            endpoint = "http://dgx.local:11434"
            model = "m"
            tiers = []
            [loadouts.planner]
            provider = "dgx"
            "#,
    )
    .unwrap();
    // dangling role: triage names no loadout
    let dangling = Crew {
        planner: "planner".into(),
        triage: Some("ghost".into()),
        ..Default::default()
    };
    let e = dangling.validate(&cfg).unwrap_err();
    assert!(e.contains("triage 'ghost'"), "{e}");
    assert!(e.contains("no [loadouts]"), "{e}");
    // transitive: a role's loadout has a dangling provider
    let mut cfg2 = cfg.clone();
    cfg2.loadouts.insert(
        "bad".into(),
        Loadout {
            provider: Some("nope".into()),
            ..Default::default()
        },
    );
    let transitive = Crew {
        planner: "bad".into(),
        ..Default::default()
    };
    let e = transitive.validate(&cfg2).unwrap_err();
    assert!(
        e.contains("planner 'bad'") && e.contains("provider 'nope'"),
        "{e}"
    );
}

#[test]
fn disk_crews_load_per_file_by_stem() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("coder.toml"),
        "planner = \"planner\"\nnavigator = \"navigator\"\n",
    )
    .unwrap();
    // malformed (missing required `planner`) is skipped, not fatal
    std::fs::write(dir.path().join("broken.toml"), "navigator = \"x\"\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "not a crew").unwrap();

    let mut cfg = Config::default();
    cfg.merge_crews_from_dir(dir.path());
    assert_eq!(cfg.crews.len(), 1, "only the valid .toml loads");
    let c = cfg.crews.get("coder").expect("loaded by filename stem");
    assert_eq!(c.planner, "planner");
    // disk overrides inline of the same name (last-wins)
    cfg.crews.insert(
        "coder".into(),
        Crew {
            planner: "inline".into(),
            ..Default::default()
        },
    );
    cfg.merge_crews_from_dir(dir.path());
    assert_eq!(cfg.crews["coder"].planner, "planner", "disk wins");
}

use super::*;

// Tool schema exposure profiles.

#[test]
fn tool_exposure_defaults_to_full_identity_when_absent() {
    // No `[tool_exposure]` section ⇒ the identity controller (unchanged
    // advertised catalog).
    let cfg: Config = toml::from_str("").unwrap();
    assert!(cfg.tool_exposure.is_none());
    let resolved = cfg.tool_exposure();
    assert_eq!(resolved.profile, ExposureProfile::Full);
    assert_eq!(resolved.schema_budget_pct, 15);
    assert_eq!(resolved.max_initial_tools, 0);
    assert!(resolved.supports_dynamic_catalog);
    assert_eq!(
        Config::default().tool_exposure().profile,
        ExposureProfile::Full
    );
}

#[test]
fn tool_exposure_parses_an_auto_profile_override() {
    let cfg: Config = toml::from_str(
        r#"
            [tool_exposure]
            profile = "auto"
            schema_budget_pct = 12
            max_initial_tools = 8
            supports_dynamic_catalog = false
        "#,
    )
    .unwrap();
    let resolved = cfg.tool_exposure();
    assert_eq!(resolved.profile, ExposureProfile::Auto);
    assert_eq!(resolved.schema_budget_pct, 12);
    assert_eq!(resolved.max_initial_tools, 8);
    assert!(!resolved.supports_dynamic_catalog);
}

#[test]
fn tool_exposure_minimal_profile_parses() {
    let cfg: Config = toml::from_str("[tool_exposure]\nprofile = \"minimal\"\n").unwrap();
    let resolved = cfg.tool_exposure();
    assert_eq!(resolved.profile, ExposureProfile::Minimal);
    // Omitted keys fall back to the shared defaults.
    assert_eq!(resolved.schema_budget_pct, 15);
}

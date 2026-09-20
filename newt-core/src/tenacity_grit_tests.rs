//! #2449: Grit is selectable and its explicit zero budget must survive config.
use super::*;

#[test]
fn grit_2449_selector_is_a_real_cumulative_level() {
    let grit = "grit"
        .parse::<Tenacity>()
        .expect("grit is a supported tenacity selector");
    assert_eq!(grit.label(), "grit");
    assert!(Tenacity::Normal < grit && grit < Tenacity::Resolute);
    assert!(
        !grit.requires_verification(),
        "Grit alone may explain expected nonzero results"
    );
    assert_eq!(
        grit.project_tool_round_limit(4),
        4,
        "Grit grants no extra rounds"
    );
}

#[test]
fn grit_2449_explicit_zero_budget_survives_versioned_config_roundtrip() {
    let config: TenacityConfig =
        toml::from_str("version = 2\ndefault = \"resolute\"\n[budgets]\ngrit_retries = 0\n")
            .unwrap();
    let encoded = toml::Value::try_from(config).unwrap();
    assert_eq!(
        encoded
            .get("budgets")
            .and_then(|v| v.get("grit_retries"))
            .and_then(toml::Value::as_integer),
        Some(0),
        "zero is an explicit no-retry policy, not an ignored unknown field"
    );
}

/// #2449: an unversioned current Grit declaration must be rejected unchanged,
/// not partially rewritten because a neighboring field is genuinely legacy.
#[test]
fn grit_2449_unversioned_persona_is_not_partially_migrated() {
    let text = "+++\ntenacity = \"grit\"\ncognition = \"pondering\"\n+++\nBody\n";
    assert!(
        crate::psyche_import::migrate_persona_text(text).is_none(),
        "missing psyche_version belongs to strict decoding; preserve original bytes"
    );
}

/// #2449: a current default does not authorize migrating neighboring old
/// family labels before the missing pursuit version is diagnosed.
#[test]
fn grit_2449_unversioned_config_is_not_partially_migrated() {
    let text = "[tenacity]\ndefault = \"grit\"\n[tenacity.families]\nexample = \"relentless\"\n";
    assert!(
        crate::psyche_import::migrate_config_text(text).is_none(),
        "missing version belongs to strict decoding; preserve original bytes"
    );
}

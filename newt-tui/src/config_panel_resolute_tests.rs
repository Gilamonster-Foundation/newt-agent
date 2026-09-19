//! #2451: restored tenacity must survive real panel and persona boundaries.
use super::*;

#[test]
fn resolute_2451_panel_projects_and_saves_persona_tenacity() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    newt_core::tenacity::clear_cli_tenacity();
    let persona = PersonaChoice {
        name: "reviewer".into(),
        profile: Some(newt_core::RoleProfile {
            tenacity: Some(Tenacity::Resolute),
            ..Default::default()
        }),
    };
    let panel = super::tests::panel(Some("reviewer"), vec![persona], Initiative::Measured);
    assert_eq!(panel.projected_tenacity(), Tenacity::Resolute);
    assert_eq!(panel.tenacity_cell().1, "persona: reviewer");
    let saved = panel.persona_content("reviewer").unwrap();
    let loaded = newt_core::RoleProfile::parse(&saved).unwrap();
    assert_eq!(loaded.tenacity, Some(Tenacity::Resolute));
    assert!(saved.contains("psyche_version = 2"));
}

#[test]
fn resolute_2451_persona_activation_and_clear_restore_tenacity() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    newt_core::tenacity::clear_cli_tenacity();
    newt_core::tenacity::set_tenacity_config(Default::default());
    let mut persona = crate::test_persona("reviewer", "Review.", "reviewer.md".into());
    persona.profile.tenacity = Some(Tenacity::Resolute);
    crate::seat_persona_dials(Some(&persona));
    assert_eq!(
        newt_core::tenacity::effective_tenacity(),
        Tenacity::Resolute
    );
    crate::seat_persona_dials(None);
    assert_eq!(newt_core::tenacity::effective_tenacity(), Tenacity::Normal);
}

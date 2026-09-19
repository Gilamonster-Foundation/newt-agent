use super::*;

/// #2451: the new pursuit level is accepted without lifting the round cap.
#[test]
fn resolute_2451_level_parses_and_keeps_configured_cap() {
    let level = "resolute"
        .parse::<Tenacity>()
        .expect("resolute is a supported level");
    assert_eq!(level.label(), "resolute");
    let limit = resolve_tool_round_limit(40, Some(level), None);
    assert_eq!(limit.rounds, 40);
    assert_eq!(limit.source, ToolRoundLimitSource::Config);
}

/// #2451: a typed family default may select cumulative relentless behavior,
/// but cap authority remains the explicit operator input, never the default.
#[test]
fn resolute_2451_automatic_relentless_keeps_cap_provenance() {
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    clear_cli_tenacity();
    set_session_tool_rounds(None);
    set_configured_tool_rounds(Some(40));
    let config: crate::Config = toml::from_str(
        "[tenacity]\nversion = 2\ndefault = \"normal\"\n[tenacity.families]\nexample = \"relentless\"\n",
    ).unwrap();
    config.publish_runtime_settings();
    crate::initiative::set_active_model_family(Some("example".into()));
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    let inherited = session_tool_round_limit().unwrap();
    assert_eq!(inherited.rounds, 40);
    assert_eq!(inherited.source, ToolRoundLimitSource::Config);
    assert_eq!(inherited.tenacity, None);
    crate::initiative::set_active_model_family(Some("prefix-example".into()));
    assert_eq!(
        effective_tenacity(),
        Tenacity::Normal,
        "family matching is exact"
    );
    set_cli_tenacity(Tenacity::Relentless);
    assert_eq!(
        session_tool_round_limit().unwrap().rounds,
        RELENTLESS_TOOL_ROUND_TARGET
    );
    set_session_tool_rounds(Some(7));
    assert_eq!(session_tool_round_limit().unwrap().rounds, 7);
}

/// #2451: every automatic layer participates without acquiring cap authority;
/// a running turn stays fixed while another thread republishes settings.
#[test]
fn resolute_2451_precedence_and_turn_capture_survive_republication() {
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    clear_cli_tenacity();
    set_persona_tenacity(None);
    let config: crate::Config = toml::from_str("[tenacity]\nversion = 2\ndefault = \"resolute\"\n[tenacity.families]\nexample = \"relentless\"\n").unwrap();
    config.publish_runtime_settings();
    crate::initiative::set_active_model_family(None);
    assert_eq!(effective_tenacity(), Tenacity::Resolute);
    crate::initiative::set_active_model_family(Some("EXAMPLE".into()));
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    set_persona_tenacity(Some(Tenacity::Normal));
    assert_eq!(effective_tenacity(), Tenacity::Normal);
    set_cli_tenacity(Tenacity::Resolute);
    let captured = crate::psyche::capture_turn_psyche();
    std::thread::spawn(|| {
        clear_cli_tenacity();
        set_persona_tenacity(None);
        crate::Config::default().publish_runtime_settings();
        assert_eq!(
            effective_tenacity(),
            Tenacity::Normal,
            "siblings resolve their own live policy"
        );
    })
    .join()
    .unwrap();
    assert_eq!(
        effective_tenacity(),
        Tenacity::Resolute,
        "active turn is immutable"
    );
    {
        let _nested = crate::psyche::capture_turn_psyche();
        assert_eq!(effective_tenacity(), Tenacity::Resolute);
    }
    drop(captured);
    assert_eq!(
        effective_tenacity(),
        Tenacity::Normal,
        "next turn sees new settings"
    );
}

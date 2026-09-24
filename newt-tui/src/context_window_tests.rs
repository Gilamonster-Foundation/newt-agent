use super::*;
use newt_core::BackendKind;

fn facts(kind: BackendKind) -> WindowFacts {
    WindowFacts {
        kind,
        served: None,
        cached_window: None,
        cached_hard_window: None,
        cached_safe_context: None,
        cached_max_ok_input: None,
        configured_window: None,
        community_window: None,
        recovered_window: None,
        input_ceiling_pct: 80,
        configured_num_ctx: None,
        context_size_override: None,
    }
}

#[test]
fn the_served_window_wins_and_says_so() {
    let got = resolve(WindowFacts {
        served: Some(131_072),
        cached_window: Some(65_536),
        configured_window: Some(32_768),
        community_window: Some(1_000_000),
        ..facts(BackendKind::Openai)
    });
    assert_eq!(got.full_window, Some(131_072));
    assert_eq!(got.window_source, Some(WindowSource::Served));
    // OpenAI gets the full window; core applies the ceiling and output reserve.
    assert_eq!(got.num_ctx, Some(131_072));
    assert_eq!(got.safe_context, Some(104_857), "80% of 131,072");
}

#[test]
fn each_weaker_declaration_is_named_when_it_is_the_one_used() {
    let cached = resolve(WindowFacts {
        cached_window: Some(65_536),
        configured_window: Some(32_768),
        ..facts(BackendKind::Openai)
    });
    assert_eq!(
        (cached.full_window, cached.window_source),
        (Some(65_536), Some(WindowSource::Cached))
    );
    let configured = resolve(WindowFacts {
        configured_window: Some(32_768),
        community_window: Some(1_000_000),
        ..facts(BackendKind::Openai)
    });
    assert_eq!(configured.window_source, Some(WindowSource::Configured));
    let community = resolve(WindowFacts {
        community_window: Some(1_000_000),
        ..facts(BackendKind::Openai)
    });
    assert_eq!(community.window_source, Some(WindowSource::Community));
    assert_eq!(resolve(facts(BackendKind::Openai)).window_source, None);
}

#[test]
fn a_server_rejection_caps_the_window_and_num_ctx() {
    let got = resolve(WindowFacts {
        served: Some(131_072),
        recovered_window: Some(65_536),
        ..facts(BackendKind::Openai)
    });
    assert_eq!(got.recovered_hard_window, Some(65_536));
    assert_eq!(got.full_window, Some(65_536));
    assert_eq!(got.num_ctx, Some(65_536));
}

/// Three-Cs (#2565): the Ollama path used a literal `* 80 / 100` for the
/// served window, ignoring `input_ceiling_pct`. The configured percentage now
/// applies on every path.
#[test]
fn a_non_openai_served_window_uses_the_configured_percentage() {
    let got = resolve(WindowFacts {
        served: Some(100_000),
        input_ceiling_pct: 90,
        ..facts(BackendKind::Ollama)
    });
    assert_eq!(got.safe_context, Some(90_000));
    assert_eq!(
        got.num_ctx,
        Some(90_000),
        "Ollama sizes KV from the safe context"
    );
}

#[test]
fn the_session_size_override_caps_both_guards_and_feeds_ollama_num_ctx() {
    let got = resolve(WindowFacts {
        served: Some(100_000),
        cached_max_ok_input: Some(70_000),
        context_size_override: Some(50_000),
        ..facts(BackendKind::Ollama)
    });
    assert_eq!(
        (got.safe_context, got.max_ok_input),
        (Some(50_000), Some(50_000))
    );
    assert_eq!(got.num_ctx, Some(50_000));
}

#[test]
fn an_explicit_num_ctx_wins_but_a_rejection_still_caps_it() {
    let got = resolve(WindowFacts {
        served: Some(131_072),
        configured_num_ctx: Some(98_304),
        recovered_window: Some(65_536),
        ..facts(BackendKind::Openai)
    });
    assert_eq!(got.num_ctx, Some(65_536));
    let uncapped = resolve(WindowFacts {
        served: Some(131_072),
        configured_num_ctx: Some(98_304),
        ..facts(BackendKind::Openai)
    });
    assert_eq!(uncapped.num_ctx, Some(98_304));
}

#[test]
fn the_ceiling_percentage_default_comes_from_config_not_a_literal() {
    let cfg = newt_core::Config::default();
    assert_eq!(
        input_ceiling_pct(&cfg),
        newt_core::config::ContextConfig::default().input_ceiling_pct
    );
}

/// #2572 review: an override is not a percentage and not a measurement. The
/// sources come from the one resolution, so the view cannot re-derive them.
#[test]
fn each_limit_carries_the_source_that_set_it() {
    let served = WindowFacts {
        served: Some(131_072),
        cached_max_ok_input: Some(100_703),
        ..facts(BackendKind::Openai)
    };
    let got = resolve(served);
    assert_eq!(got.safe_context_source, Some(LimitSource::PercentOfWindow));
    assert_eq!(got.max_ok_input_source, Some(LimitSource::Cached));
    let overridden = resolve(WindowFacts {
        context_size_override: Some(50_000),
        ..served
    });
    assert_eq!(
        (overridden.safe_context, overridden.safe_context_source),
        (Some(50_000), Some(LimitSource::SessionOverride))
    );
    assert_eq!(
        (overridden.max_ok_input, overridden.max_ok_input_source),
        (Some(50_000), Some(LimitSource::SessionOverride))
    );
    let cached_only = resolve(WindowFacts {
        cached_safe_context: Some(30_000),
        ..facts(BackendKind::Openai)
    });
    assert_eq!(cached_only.safe_context_source, Some(LimitSource::Cached));
    let configured = resolve(WindowFacts {
        configured_window: Some(32_768),
        ..facts(BackendKind::Ollama)
    });
    assert_eq!(
        (configured.safe_context, configured.safe_context_source),
        (Some(32_768), Some(LimitSource::ConfiguredWindow))
    );
    assert_eq!(
        resolve(facts(BackendKind::Openai)).safe_context_source,
        None
    );
}

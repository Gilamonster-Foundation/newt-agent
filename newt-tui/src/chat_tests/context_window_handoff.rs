use super::*;

#[test]
fn hard_recovery_caps_future_declared_windows_without_raising_tighter_ones() {
    assert_eq!(
        cap_context_window_by_recovery(Some(65_536), Some(32_768)),
        Some(32_768),
    );
    assert_eq!(
        cap_context_window_by_recovery(Some(16_384), Some(32_768)),
        Some(16_384),
    );
    assert_eq!(
        cap_context_window_by_recovery(Some(65_536), None),
        Some(65_536),
        "an ordinary probe is not a hard cap on an explicit override",
    );
    assert_eq!(
        cap_context_window_by_recovery(None, Some(32_768)),
        Some(32_768),
    );

    let capable = newt_core::model_card::ChatCompletionsCapability {
        cognition: Some(true),
        ..Default::default()
    };
    let next_chat_window = cap_context_window_by_recovery(Some(65_536), Some(32_768));
    assert_eq!(
        newt_core::agentic::initial_context_input_budget(
            newt_core::BackendKind::Openai,
            newt_core::OpenAiApi::ChatCompletions,
            next_chat_window,
            90,
            Some(newt_core::role_profile::Cognition::Contemplating),
            capable,
            newt_core::model_card::ReasoningReplayScope::CurrentUserTurn,
            Some(29_491),
            Some(29_491),
        ),
        Some(16_768),
        "the next 90%-configured Chat turn must retain the 32K hard window and 16K output reserve",
    );

    let next_ollama_window = cap_context_window_by_recovery(Some(50_000), Some(32_768));
    assert_eq!(
        newt_core::agentic::initial_context_input_budget(
            newt_core::BackendKind::Ollama,
            newt_core::OpenAiApi::ChatCompletions,
            next_ollama_window,
            80,
            None,
            Default::default(),
            newt_core::model_card::ReasoningReplayScope::Never,
            Some(50_000),
            Some(50_000),
        ),
        Some(26_214),
        "the next Ollama turn must cap a raised /context size at the recovered full window",
    );
}

#[test]
fn openai_handoff_keeps_the_full_window_separate_from_the_input_cap() {
    assert_eq!(
        context_window_for_core(newt_core::BackendKind::Openai, Some(32_768), Some(26_214),),
        Some(32_768),
    );
    assert_eq!(
        newt_core::config::input_percentage_ceiling(32_768, 90),
        29_491,
        "the configured percentage, not a hardcoded 80%, seeds the OpenAI input cap",
    );
    assert_eq!(
        context_window_for_core(newt_core::BackendKind::Openai, None, Some(26_214)),
        None,
        "a cached input cap must not be reinterpreted as a full OpenAI window",
    );
    assert_eq!(
        context_window_for_core(newt_core::BackendKind::Ollama, Some(32_768), Some(26_214),),
        Some(26_214),
        "Ollama retains the conservative KV-allocation fallback",
    );
}

#[test]
fn selected_model_switch_replaces_the_previous_context_window() {
    assert_eq!(
        selected_model_context_window(None, None, Some(1_000_000)),
        Some(1_000_000)
    );
    assert_eq!(
        selected_model_context_window(None, Some(131_072), None),
        Some(131_072)
    );
    assert_eq!(
        selected_model_context_window(Some(262_144), Some(131_072), Some(1_000_000)),
        Some(262_144),
        "fresh endpoint metadata wins for the newly selected model"
    );
}

#[test]
fn openai_gauge_reports_the_output_reserved_send_budget() {
    let capability = newt_core::model_card::ChatCompletionsCapability {
        cognition: Some(true),
        ..Default::default()
    };
    assert_eq!(
        context_gauge_budget(
            newt_core::BackendKind::Openai,
            newt_core::OpenAiApi::ChatCompletions,
            Some(32_768),
            Some(32_768),
            80,
            Some(newt_core::role_profile::Cognition::Contemplating),
            capability,
            newt_core::model_card::ReasoningReplayScope::CurrentUserTurn,
            Some(26_214),
            Some(26_214),
        ),
        Some(16_768),
        "the visible gauge must match the contemplating request's actual input ceiling",
    );
    assert_eq!(
        context_gauge_budget(
            newt_core::BackendKind::Openai,
            newt_core::OpenAiApi::ChatCompletions,
            Some(65_536),
            None,
            80,
            Some(newt_core::role_profile::Cognition::Contemplating),
            capability,
            newt_core::model_card::ReasoningReplayScope::CurrentUserTurn,
            Some(26_214),
            Some(26_214),
        ),
        Some(26_214),
        "an ordinary 32K probe must not defeat an explicit 65K turn window",
    );
    assert_eq!(
        context_gauge_budget(
            newt_core::BackendKind::Openai,
            newt_core::OpenAiApi::ChatCompletions,
            Some(65_536),
            Some(32_768),
            80,
            Some(newt_core::role_profile::Cognition::Contemplating),
            capability,
            newt_core::model_card::ReasoningReplayScope::CurrentUserTurn,
            Some(26_214),
            Some(26_214),
        ),
        Some(16_768),
        "the same 32K value must tighten only after a numbered 400 observed it",
    );
    assert_eq!(
        context_gauge_budget(
            newt_core::BackendKind::Ollama,
            newt_core::OpenAiApi::ChatCompletions,
            Some(50_000),
            None,
            80,
            None,
            Default::default(),
            newt_core::model_card::ReasoningReplayScope::Never,
            Some(50_000),
            Some(50_000),
        ),
        Some(40_000),
        "an ordinary Ollama probe must not defeat /context size",
    );
}

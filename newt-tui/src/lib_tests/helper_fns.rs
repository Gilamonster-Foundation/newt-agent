use super::*;

/// Re-homed `trim_to_token_budget_zero_is_noop` at the passthrough (F3):
/// a configured zero — per-model or global — disables the token trigger
/// instead of reaching the loop as "budget 0, fire every round".
#[test]
fn zero_mid_loop_trim_tokens_is_disabled() {
    // Global zero → disabled.
    assert_eq!(effective_mid_loop_trim_tokens(None, Some(0)), None);
    // Per-model zero overrides a real global → disabled for this model.
    assert_eq!(effective_mid_loop_trim_tokens(Some(0), Some(5_000)), None);
    // Real values pass through, override winning.
    assert_eq!(
        effective_mid_loop_trim_tokens(None, Some(5_000)),
        Some(5_000)
    );
    assert_eq!(
        effective_mid_loop_trim_tokens(Some(3_000), Some(5_000)),
        Some(3_000)
    );
    // Nothing configured → disabled.
    assert_eq!(effective_mid_loop_trim_tokens(None, None), None);
}

#[test]
fn today_date_matches_utc_calendar() {
    // today_date derives YYYY-MM-DD from epoch seconds (UTC). Compare with
    // chrono, sampling before and after to be immune to a midnight rollover
    // between the two calls.
    let before = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let got = today_date();
    let after = chrono::Utc::now().format("%Y-%m-%d").to_string();
    assert!(
        got == before || got == after,
        "today_date()={got} not in [{before}, {after}]"
    );
}

#[test]
fn keep_alive_str_default_and_configured() {
    assert_eq!(keep_alive_str(&newt_core::Config::default()), "5m");
    let cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            keep_alive: "30m".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(keep_alive_str(&cfg), "30m");
}

#[test]
fn markdown_enabled_resolves_config_session_and_color() {
    use newt_core::MarkdownMode;
    let cfg_with = |m: MarkdownMode| newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            markdown: m,
            ..Default::default()
        }),
        ..Default::default()
    };
    // Default (auto): follows color.
    assert!(markdown_enabled(&newt_core::Config::default(), true, None));
    assert!(!markdown_enabled(
        &newt_core::Config::default(),
        false,
        None
    ));
    // Config off: never renders, even with color.
    assert!(!markdown_enabled(&cfg_with(MarkdownMode::Off), true, None));
    // Config on: still gated by color (ANSI needs color).
    assert!(markdown_enabled(&cfg_with(MarkdownMode::On), true, None));
    assert!(!markdown_enabled(&cfg_with(MarkdownMode::On), false, None));
    // Session override wins over config, still color-gated.
    assert!(!markdown_enabled(
        &cfg_with(MarkdownMode::On),
        true,
        Some(false)
    ));
    assert!(markdown_enabled(
        &cfg_with(MarkdownMode::Off),
        true,
        Some(true)
    ));
    assert!(!markdown_enabled(
        &cfg_with(MarkdownMode::Off),
        false,
        Some(true)
    ));
}

#[test]
fn context_manager_resolves_session_config_default() {
    use newt_core::{ContextConfig, ContextManager};
    let cfg_with = |m: ContextManager| newt_core::Config {
        context: Some(ContextConfig {
            manager: m,
            ..Default::default()
        }),
        ..Default::default()
    };
    // No [context] → standard.
    assert_eq!(
        context_manager(&newt_core::Config::default(), None),
        ContextManager::Standard
    );
    // Config value when no session override.
    assert_eq!(
        context_manager(&cfg_with(ContextManager::Progressive), None),
        ContextManager::Progressive
    );
    // Session override wins over config.
    assert_eq!(
        context_manager(
            &cfg_with(ContextManager::Progressive),
            Some(ContextManager::Standard)
        ),
        ContextManager::Standard
    );
}

#[test]
fn compaction_trigger_policy_resolves_session_config_default() {
    use newt_core::{CompactionTriggerPolicy, ContextConfig};
    let cfg_with = |policy: CompactionTriggerPolicy| newt_core::Config {
        context: Some(ContextConfig {
            compaction_trigger_policy: policy,
            ..Default::default()
        }),
        ..Default::default()
    };

    // No [context] → the conservative headroom-aware default.
    assert_eq!(
        compaction_trigger_policy(&newt_core::Config::default(), None),
        CompactionTriggerPolicy::HeadroomAware
    );
    assert_eq!(
        compaction_trigger_policy_source(&newt_core::Config::default(), None),
        "default"
    );

    // Config wins when the session has not selected a temporary policy.
    assert_eq!(
        compaction_trigger_policy(&cfg_with(CompactionTriggerPolicy::MessageCount), None),
        CompactionTriggerPolicy::MessageCount
    );
    assert_eq!(
        compaction_trigger_policy_source(&cfg_with(CompactionTriggerPolicy::MessageCount), None),
        "config"
    );

    // A session override remains highest precedence.
    assert_eq!(
        compaction_trigger_policy(
            &cfg_with(CompactionTriggerPolicy::MessageCount),
            Some(CompactionTriggerPolicy::HeadroomAware)
        ),
        CompactionTriggerPolicy::HeadroomAware
    );
    assert_eq!(
        compaction_trigger_policy_source(
            &cfg_with(CompactionTriggerPolicy::MessageCount),
            Some(CompactionTriggerPolicy::HeadroomAware)
        ),
        "session"
    );
}

#[test]
fn context_features_resolves_preset_config_session() {
    use newt_core::{
        BackendKind, ContextConfig, ContextFeature as F, ContextFeatures, ContextManager,
    };
    // Cloud (Openai) base: per the context policy, every available
    // feature defaults on except Provenance, regardless of backend.
    let cloud = context_features(
        &newt_core::Config::default(),
        ContextManager::Standard,
        &ContextFeatures::default(),
        BackendKind::Openai,
    );
    assert!(cloud.get(F::ToolOffload));
    assert!(cloud.get(F::Semantic));
    assert!(cloud.get(F::Scratchpad));
    assert!(cloud.get(F::Scheduled));
    // [context.features] override layers over the preset base.
    let mut cfg_feats = ContextFeatures::default();
    cfg_feats.set(F::Semantic, Some(true));
    let cfg = newt_core::Config {
        context: Some(ContextConfig {
            manager: ContextManager::Standard,
            features: cfg_feats,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(context_features(
        &cfg,
        ContextManager::Standard,
        &ContextFeatures::default(),
        BackendKind::Openai,
    )
    .get(F::Semantic));
    // Session override wins over config (forces it back off).
    let mut sess = ContextFeatures::default();
    sess.set(F::Semantic, Some(false));
    assert!(
        !context_features(&cfg, ContextManager::Standard, &sess, BackendKind::Openai)
            .get(F::Semantic)
    );
}

#[test]
fn context_features_local_backend_defaults_plan_semantic_ledger_on() {
    use newt_core::{
        BackendKind, ContextConfig, ContextFeature as F, ContextFeatures, ContextManager,
    };
    // #945 + Step 27.4: a local (Ollama) session defaults tool_offload,
    // scratchpad, semantic, and scheduled ON with no config at all.
    let local = context_features(
        &newt_core::Config::default(),
        ContextManager::Standard,
        &ContextFeatures::default(),
        BackendKind::Ollama,
    );
    assert!(local.get(F::ToolOffload));
    assert!(local.get(F::Scratchpad));
    assert!(local.get(F::Semantic));
    assert!(local.get(F::Scheduled));
    // Explicit [context.features] off values still win.
    let mut off = ContextFeatures::default();
    off.set(F::Scheduled, Some(false));
    off.set(F::ToolOffload, Some(false));
    let cfg = newt_core::Config {
        context: Some(ContextConfig {
            manager: ContextManager::Standard,
            features: off,
            ..Default::default()
        }),
        ..Default::default()
    };
    let resolved = context_features(
        &cfg,
        ContextManager::Standard,
        &ContextFeatures::default(),
        BackendKind::Ollama,
    );
    assert!(
        !resolved.get(F::Scheduled),
        "explicit off overrides the local default"
    );
    assert!(
        !resolved.get(F::ToolOffload),
        "explicit off overrides default-on offload"
    );
    assert!(resolved.get(F::Scratchpad), "untouched feature stays on");
}

#[test]
fn handle_context_command_dispatch() {
    use newt_core::{BackendKind, CompactionTriggerPolicy, ContextFeatures, ContextManager};
    let cfg = newt_core::Config::default();
    let none = ContextFeatures::default();
    // Cloud kind keeps the all-off base so these assertions isolate the
    // dispatch logic from the Step 27.4 local default.
    let run =
        |rest: &str| handle_context_command(rest, &cfg, None, None, &none, BackendKind::Openai);

    // bare status: manager + features summary, no mutation. `tool_offload`
    // now defaults on for EVERY backend kind (`base_for` sets it
    // unconditionally, unlike scratchpad/scheduled/semantic which are
    // Ollama-only local defaults) — the one feature that's on even on
    // this deliberately-Openai (all-else-off) baseline.
    let r = run("");
    assert!(r.lines[0].contains("context manager: standard"));
    assert!(r.lines[0].contains("features on: tool_offload"));
    assert!(r.lines[1].contains("headroom_aware (default)"));
    assert!(r.set_manager.is_none() && r.set_feature.is_none());

    // The policy has its own inspect/set/reset surface. A reset is explicit in
    // the pure result so the chat loop can clear rather than leave a stale
    // session override in place.
    assert!(run("compaction").lines[0].contains("headroom_aware (default)"));
    assert_eq!(
        run("compaction message_count").set_compaction_trigger_policy,
        Some(CompactionTriggerPolicyOverride::Set(
            CompactionTriggerPolicy::MessageCount
        ))
    );
    assert_eq!(
        run("compaction HEADROOM_AWARE").set_compaction_trigger_policy,
        Some(CompactionTriggerPolicyOverride::Set(
            CompactionTriggerPolicy::HeadroomAware
        )),
        "the command accepts the canonical policy parser's case-insensitive form"
    );
    let session_status = handle_context_command(
        "",
        &cfg,
        None,
        Some(CompactionTriggerPolicy::MessageCount),
        &none,
        BackendKind::Openai,
    );
    assert!(session_status.lines[1].contains("message_count (session)"));
    let r = handle_context_command(
        "compaction reset",
        &cfg,
        None,
        Some(CompactionTriggerPolicy::MessageCount),
        &none,
        BackendKind::Openai,
    );
    assert_eq!(
        r.set_compaction_trigger_policy,
        Some(CompactionTriggerPolicyOverride::Reset)
    );
    assert!(r.lines[0].contains("headroom_aware (default)"));
    assert!(run("compaction nope").lines[0].contains("unknown compaction trigger policy"));

    // manager set (standard is available)
    assert_eq!(
        run("manager standard").set_manager,
        Some(ContextManager::Standard)
    );

    // unavailable manager → reported, NOT applied
    let r = run("manager progressive");
    assert!(r.set_manager.is_none());
    assert!(r.lines[0].contains("not yet available"));

    // unknown manager
    assert!(run("manager bogus").lines[0].contains("unknown context manager"));

    // feature list: all six listed; only provenance not-yet-available (the
    // other five shipped in 26.3/26.4/26.5/26.6a/26.6b).
    let r = run("feature");
    assert!(r.lines.iter().any(|l| l.contains("scratchpad")));
    assert!(r.lines.iter().any(|l| l.contains("tool_offload")));
    assert_eq!(
        r.lines
            .iter()
            .filter(|l| l.contains("not yet available"))
            .count(),
        1
    );

    // toggling the one still-unavailable feature → reported with its issue,
    // NOT applied (provenance = #584, the remaining pending feature).
    let r = run("feature provenance on");
    assert!(r.set_feature.is_none());
    assert!(r.lines[0].contains("not yet available") && r.lines[0].contains("#584"));

    // alias still resolves ("handles" = provenance, still pending)
    assert!(run("feature handles on").lines[0].contains("not yet available"));

    // unknown feature / bad toggle / unknown subcommand
    assert!(run("feature bogus on").lines[0].contains("unknown context feature"));
    assert!(run("feature scratchpad maybe").lines[0].contains("unknown toggle"));
    assert!(run("wat").lines[0].contains("unknown /context subcommand"));

    // feature query (no toggle) shows state + availability. Semantic now
    // defaults ON for every backend under the all-on-except-provenance
    // policy (`base_for`), so the resolved query reports "on".
    assert!(run("feature semantic").lines[0].contains("context feature semantic: on"));
    assert!(run("semantic").lines[0].contains("context feature semantic: on"));

    // A feature FORCED on via [context.features] (allowed even before it's
    // implemented): toggling it off is still refused, and the message +
    // bare status report the REAL state (config-forced on), not a hardcoded
    // "off" — the review-flagged honesty edge.
    let mut feats = ContextFeatures::default();
    feats.set(newt_core::ContextFeature::Provenance, Some(true));
    let cfg_on = newt_core::Config {
        context: Some(newt_core::ContextConfig {
            manager: ContextManager::Standard,
            features: feats,
            ..Default::default()
        }),
        ..Default::default()
    };
    let r = handle_context_command(
        "feature provenance off",
        &cfg_on,
        None,
        None,
        &none,
        BackendKind::Openai,
    );
    assert!(
        r.set_feature.is_none(),
        "an unavailable feature is never applied"
    );
    assert!(
        r.lines[0].contains("staying on"),
        "message reflects the config-forced ON state: {:?}",
        r.lines[0]
    );
    assert!(
        handle_context_command("", &cfg_on, None, None, &none, BackendKind::Openai).lines[0]
            .contains("provenance (pending #584)"),
        "bare status annotates a config-on-but-unavailable feature as pending"
    );

    // tool_offload shipped in 26.3 → toggling it ON is now APPLIED (no
    // "not yet available"); proves the availability gate flips correctly.
    let r = run("feature tool_offload on");
    assert_eq!(
        r.set_feature,
        Some((newt_core::ContextFeature::ToolOffload, true))
    );
    assert!(!r.lines[0].contains("not yet available"), "{:?}", r.lines);

    // scratchpad shipped in 26.4 → its alias "state" toggles ON too.
    let r = run("feature state on");
    assert_eq!(
        r.set_feature,
        Some((newt_core::ContextFeature::Scratchpad, true))
    );

    // semantic shipped in 26.5 → toggles ON (alias "retrieval" too).
    assert_eq!(
        run("feature retrieval on").set_feature,
        Some((newt_core::ContextFeature::Semantic, true))
    );
    assert_eq!(
        run("semantic on").set_feature,
        Some((newt_core::ContextFeature::Semantic, true))
    );

    // experiential shipped in 26.6a → toggles ON (alias "experience" too).
    assert_eq!(
        run("feature experience on").set_feature,
        Some((newt_core::ContextFeature::Experiential, true))
    );

    // scheduled shipped in 26.6b → toggles ON (alias "compiled" too).
    assert_eq!(
        run("feature compiled on").set_feature,
        Some((newt_core::ContextFeature::Scheduled, true))
    );
}

#[test]
fn context_stats_text_composes_budget_compression_and_features() {
    use newt_core::{CompactionTriggerPolicy, CompressCounters, ContextFeatureSet};
    let counters = CompressCounters {
        compressions: 3,
        strikes: 1,
        disabled: false,
        last_reclaim: Some(0.42),
    };
    let features = ContextFeatureSet::default();

    // No gauge yet → "not yet measured".
    let none = context_stats_text(
        None,
        &counters,
        CompactionTriggerPolicy::HeadroomAware,
        "default",
        features,
        None,
        None,
        None,
        None,
        None,
    );
    assert_eq!(none[0], "context stats");
    assert!(none.iter().any(|l| l.contains("budget: not yet measured")));
    assert!(none
        .iter()
        .any(|l| l.contains("automatic compaction: headroom_aware (default)")));

    // With a gauge → budget line shows the fraction + percent.
    let s = context_stats_text(
        Some((899_000, 1_024_000)),
        &counters,
        CompactionTriggerPolicy::MessageCount,
        "session",
        features,
        None,
        None,
        None,
        None,
        None,
    );
    let joined = s.join("\n");
    assert!(joined.contains("899k/1024k"), "{joined}");
    assert!(joined.contains("% of the send window"), "{joined}");
    assert!(joined.contains("automatic compaction: message_count (session)"));
    // Compression telemetry is reused from the /memory section.
    assert!(joined.contains("compressions this session: 3"), "{joined}");
    assert!(joined.contains("reclaimed 42%"), "{joined}");
    // Every feature is listed; all but provenance are available — only one
    // feature is still pending.
    for f in newt_core::ContextFeature::ALL {
        assert!(joined.contains(f.keyword()), "missing {}", f.keyword());
    }
    assert_eq!(
        s.iter().filter(|l| l.contains("(pending #")).count(),
        1,
        "only provenance still pending (the other five shipped)"
    );

    // each available feature renders its impact on its row when on.
    let mut on = ContextFeatureSet::default();
    on.set(newt_core::ContextFeature::ToolOffload, true);
    on.set(newt_core::ContextFeature::Scratchpad, true);
    on.set(newt_core::ContextFeature::Semantic, true);
    on.set(newt_core::ContextFeature::Experiential, true);
    on.set(newt_core::ContextFeature::Scheduled, true);
    let imp = context_stats_text(
        None,
        &counters,
        CompactionTriggerPolicy::HeadroomAware,
        "config",
        on,
        Some((3, 48_000)),
        Some((5, 12_000)),
        Some((42, 60_000)),
        Some((7, 9_000)),
        Some((4, 2)),
    )
    .join("\n");
    assert!(
        imp.contains("[on ] tool_offload  — 3 offloaded (~48k chars elided)"),
        "{imp}"
    );
    assert!(
        imp.contains("[on ] scratchpad  — 5 keys (~12k chars)"),
        "{imp}"
    );
    assert!(
        imp.contains("[on ] semantic  — 42 chunks indexed (~60k chars)"),
        "{imp}"
    );
    assert!(
        imp.contains("[on ] experiential  — 7 experiences (~9k chars)"),
        "{imp}"
    );
    assert!(
        imp.contains("[on ] scheduled  — 2/4 plan steps done"),
        "{imp}"
    );

    // A zero budget renders the unmeasured line (no divide-by-zero).
    assert!(context_stats_text(
        Some((10, 0)),
        &counters,
        CompactionTriggerPolicy::HeadroomAware,
        "default",
        features,
        None,
        None,
        None,
        None,
        None
    )
    .iter()
    .any(|l| l.contains("not yet measured")));
}

#[test]
fn mid_loop_trim_threshold_clamps_below_round_cap() {
    // Default config: threshold 40 clamped to max_tool_rounds(25) - 3 = 22,
    // so the trim safety valve always fires before the round ceiling.
    assert_eq!(mid_loop_trim_threshold(&newt_core::Config::default()), 22);

    // Small round cap: threshold clamps to cap - 3.
    let cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            max_tool_rounds: 7,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(mid_loop_trim_threshold(&cfg), 4);

    // Explicit threshold below the clamp passes through untouched.
    let cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            max_tool_rounds: 25,
            mid_loop_trim_threshold: 5,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(mid_loop_trim_threshold(&cfg), 5);
}

#[test]
fn timeout_helpers_default_and_configured() {
    let empty = newt_core::Config::default();
    assert_eq!(connect_timeout_secs(&empty), 5);
    assert_eq!(inference_timeout_secs(&empty), 120);
    let cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            connect_timeout_secs: 9,
            inference_timeout_secs: 300,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(connect_timeout_secs(&cfg), 9);
    assert_eq!(inference_timeout_secs(&cfg), 300);
}

#[test]
fn build_check_cmd_reads_config() {
    assert_eq!(build_check_cmd(&newt_core::Config::default()), None);
    let cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            build_check_cmd: Some("cargo check -q".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(build_check_cmd(&cfg).as_deref(), Some("cargo check -q"));
}

#[test]
fn resolve_workspace_none_uses_current_dir() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(resolve_workspace(None), cwd.to_string_lossy());
}

#[test]
fn expand_prompt_tokens_replaces_all_tokens() {
    let out = expand_prompt_tokens("\\w|\\W|\\v|\\m|\\M", "/tmp/proj", "gpt-4.1", true);
    assert_eq!(out, format!("proj|/tmp/proj|{VERSION}|gpt-4.1|vi"));
    // \h expands to *some* hostname — the token itself must be gone.
    let host = expand_prompt_tokens("on \\h!", "/tmp/proj", "m", false);
    assert!(!host.contains("\\h"), "got: {host}");
    assert!(host.starts_with("on ") && host.ends_with('!'));
}

#[test]
fn resolver_default_backend_beats_the_openai_heuristic() {
    // #1139 (C1): with several backends and a default_backend pointer, the
    // pointer wins; without it, the historical prefer-openai heuristic
    // applies; a sole backend is always chosen.
    let ollama = newt_core::BackendConfig {
        name: "gnuc".into(),
        endpoint: "http://gnuc:11434".into(),
        model: Some("m1".into()),
        kind: Some(newt_core::BackendKind::Ollama),
        ..Default::default()
    };
    let vllm = newt_core::BackendConfig {
        name: "dgx1-8000".into(),
        endpoint: "http://dgx1:8000".into(),
        kind: Some(newt_core::BackendKind::Openai),
        serving: Some(newt_core::Serving::Instance),
        ..Default::default()
    };
    let mut cfg = newt_core::Config {
        backends: vec![ollama.clone(), vllm.clone()],
        ..Default::default()
    };
    // No default → prefer-openai heuristic picks the vllm entry; its
    // model is EMPTY (server dictates; adopt() fills it), and name/serving
    // ride along for the adopt wiring.
    let c = resolve_backend_choice(&cfg);
    assert_eq!(c.name, "dgx1-8000");
    assert_eq!(c.model, "", "unset model stays empty until adopted");
    assert_eq!(c.serving, Some(newt_core::Serving::Instance));
    // default_backend pointer beats the heuristic.
    cfg.default_backend = Some("gnuc".into());
    let c = resolve_backend_choice(&cfg);
    assert_eq!(c.name, "gnuc");
    assert_eq!(c.model, "m1");
    // Sole backend is the obvious choice.
    let solo = newt_core::Config {
        backends: vec![ollama],
        ..Default::default()
    };
    assert_eq!(resolve_backend_choice(&solo).name, "gnuc");
}

#[test]
fn ready_line_names_the_backend_protocol() {
    // Ollama endpoint (e.g. :11434) is labeled ollama …
    let l = ready_line(
        "0.6.8",
        "qwen3.6:27b",
        "http://REDACTED-HOST:11434",
        newt_core::BackendKind::Ollama,
    );
    assert!(
        l.contains("qwen3.6:27b @ http://REDACTED-HOST:11434 (ollama)"),
        "{l}"
    );
    // … an OpenAI-compatible (vLLM) endpoint is labeled openai.
    let v = ready_line(
        "0.6.8",
        "m",
        "http://dgx1:8000",
        newt_core::BackendKind::Openai,
    );
    assert!(v.contains("@ http://dgx1:8000 (openai)"), "{v}");
}

#[test]
fn resolve_embeddings_target_decouples_or_uses_explicit_protocol() {
    use newt_core::BackendKind;
    // The HTTP helper still falls back to the active backend URL when the
    // caller explicitly selects an HTTP embeddings protocol without a
    // separate endpoint.
    let cfg = newt_core::SemanticConfig {
        embeddings_api: Some(BackendKind::Ollama),
        ..Default::default()
    };
    let (url, kind, key) =
        resolve_embeddings_target(&cfg, "http://dgx1:8000", BackendKind::Openai, Some("sk-x"));
    assert_eq!(url, "http://dgx1:8000");
    assert_eq!(kind, BackendKind::Ollama);
    assert_eq!(key.as_deref(), Some("sk-x"));
    // Explicit endpoint → used as-is, no inherited key; protocol defaults to
    // Ollama when embeddings_api is unset.
    let cfg = newt_core::SemanticConfig {
        embeddings_endpoint: Some("http://REDACTED-HOST:11434".to_string()),
        ..Default::default()
    };
    let (url, kind, key) =
        resolve_embeddings_target(&cfg, "http://dgx1:8000", BackendKind::Openai, Some("sk-x"));
    assert_eq!(url, "http://REDACTED-HOST:11434");
    assert_eq!(kind, BackendKind::Ollama);
    assert_eq!(key, None);
    // ...and honors an explicit embeddings_api.
    let cfg = newt_core::SemanticConfig {
        embeddings_api: Some(BackendKind::Openai),
        ..cfg
    };
    let (_, kind, _) = resolve_embeddings_target(&cfg, "http://x", BackendKind::Ollama, None);
    assert_eq!(kind, BackendKind::Openai);
}

#[test]
fn embeddings_backend_is_embedded_by_default_or_embedded_api() {
    // #720: the in-process candle embedder is the default. HTTP embeddings
    // require explicit Ollama/OpenAI semantic config.
    let mut cfg = newt_core::SemanticConfig::default();
    assert!(embeddings_backend_is_embedded(&cfg)); // None (default)
    cfg.embeddings_endpoint = Some("http://REDACTED-HOST:11434".to_string());
    assert!(!embeddings_backend_is_embedded(&cfg));
    cfg.embeddings_endpoint = None;
    cfg.embeddings_api = Some(newt_core::BackendKind::Ollama);
    assert!(!embeddings_backend_is_embedded(&cfg));
    cfg.embeddings_api = Some(newt_core::BackendKind::Openai);
    assert!(!embeddings_backend_is_embedded(&cfg));
    cfg.embeddings_api = Some(newt_core::BackendKind::Embedded);
    assert!(embeddings_backend_is_embedded(&cfg));
}

#[test]
fn semantic_zero_index_hint_matches_embedder_path() {
    let embedded = newt_core::SemanticConfig::default();
    let hint = semantic_zero_index_hint(&embedded);
    // #1279: the honest remediation names the explicit fetch command.
    assert!(hint.contains("newt models pull-embed"), "got: {hint}");
    assert!(hint.contains("embedding_model_path"), "got: {hint}");

    let http = newt_core::SemanticConfig {
        embeddings_endpoint: Some("http://REDACTED-HOST:11434".to_string()),
        ..Default::default()
    };
    assert!(semantic_zero_index_hint(&http).contains("Ollama/OpenAI"));
}

#[test]
fn semantic_embedder_preflight_skips_unavailable_embedded_path() {
    let embedded = newt_core::SemanticConfig::default();
    let reason = semantic_embedder_unavailable_reason(&embedded)
        .expect("default semantic embeddings select the embedded path");
    #[cfg(not(feature = "embedded"))]
    assert!(
        reason.contains("lacks the `embedded` feature"),
        "got: {reason}"
    );
    #[cfg(feature = "embedded")]
    assert!(reason.contains("embedding_model_path"), "got: {reason}");
}

#[test]
fn effective_embedding_model_path_precedence() {
    use std::path::PathBuf;
    // #1279: an explicit config path wins over the pulled default.
    assert_eq!(
        effective_embedding_model_path(
            Some("/explicit/path".to_string()),
            Some(PathBuf::from("/pulled/default"))
        ),
        Some("/explicit/path".to_string())
    );
    // No explicit path → adopt the pulled default when present.
    assert_eq!(
        effective_embedding_model_path(None, Some(PathBuf::from("/pulled/default"))),
        Some("/pulled/default".to_string())
    );
    // Neither present → None (the caller coaches `newt models pull-embed`).
    assert_eq!(effective_embedding_model_path(None, None), None);
}

#[test]
fn semantic_zero_index_and_unavailable_reason_name_the_pull_command() {
    // #1279: with the embedded path selected and no model, both the preflight
    // reason and the zero-index hint coach the explicit fetch (never silent-off).
    let embedded = newt_core::SemanticConfig::default();
    #[cfg(feature = "embedded")]
    {
        let reason = semantic_embedder_unavailable_reason(&embedded).expect("no model → a reason");
        assert!(reason.contains("newt models pull-embed"), "got: {reason}");
    }
    assert!(semantic_zero_index_hint(&embedded).contains("newt models pull-embed"));
}

#[test]
fn semantic_embedder_preflight_allows_explicit_http_embeddings() {
    let http = newt_core::SemanticConfig {
        embeddings_api: Some(newt_core::BackendKind::Ollama),
        ..Default::default()
    };
    assert!(semantic_embedder_unavailable_reason(&http).is_none());
}

#[tokio::test]
async fn build_semantic_embedder_selects_embedded_path() {
    // #720: with no explicit HTTP embeddings target, the builder takes the
    // embedded branch. In a build WITHOUT the `embedded` feature that yields
    // a failing embedder whose error names the missing feature — proving the
    // embedded path was selected (an HTTP client would attempt a network
    // call, not return this message). With the feature but no model dir it
    // likewise fails closed.
    let cfg = newt_core::SemanticConfig::default();
    let embedder = build_semantic_embedder(&cfg, "http://unused", inf_kind_ollama(), None);
    let err = embedder.embed("x").await.unwrap_err().to_string();
    assert!(
        err.contains("embedded"),
        "expected the embedded path's error, got: {err}"
    );
}

#[tokio::test]
async fn make_embedded_embedder_without_model_path_fails_closed() {
    // No `embedding_model_path` → a failing embedder (indexing no-op), not a
    // panic. The message is actionable.
    let embedder = make_embedded_embedder("bge-small-en-v1.5".to_string(), None);
    let err = embedder.embed("x").await.unwrap_err().to_string();
    #[cfg(feature = "embedded")]
    assert!(err.contains("embedding_model_path"), "got: {err}");
    #[cfg(not(feature = "embedded"))]
    assert!(err.contains("--features embedded"), "got: {err}");
}

#[tokio::test]
async fn build_semantic_embedder_http_branch_constructs() {
    // The non-embedded branch builds an HTTP EmbeddingsClient (construction is
    // pure — no network). Exercising it keeps the HTTP path covered.
    let cfg = newt_core::SemanticConfig {
        embeddings_api: Some(newt_core::BackendKind::Ollama),
        ..Default::default()
    };
    let _embedder =
        build_semantic_embedder(&cfg, "http://localhost:11434", inf_kind_ollama(), None);
    // Constructed without panic; embed() is intentionally NOT called (network).
}

/// Local helper: the Ollama backend kind, spelled once for the tests above.
fn inf_kind_ollama() -> newt_core::BackendKind {
    newt_core::BackendKind::Ollama
}

#[test]
fn resolve_backend_choice_prefers_openai_backend() {
    let cfg = newt_core::Config {
        backends: vec![newt_core::BackendConfig {
            name: "vllm".into(),
            endpoint: "http://vllm.example:8000".into(),
            model: Some("qwen3:32b".into()),
            model_path: None,
            tiers: vec![],
            kind: Some(newt_core::BackendKind::Openai),
            api: Default::default(),
            api_key_file: None,
            api_key_env: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let choice = crate::env_resolution_tests::with_env_vars(
        &[],
        &["NEWT_DGX_MODEL", "NEWT_BACKEND", "NEWT_PROVIDER"],
        || resolve_backend_choice(&cfg),
    );
    assert_eq!(choice.kind, newt_core::BackendKind::Openai);
    assert_eq!(choice.url, "http://vllm.example:8000");
    assert_eq!(choice.model, "qwen3:32b");
    assert!(choice.api_key.is_none(), "no key configured → None");
}

#[test]
fn resolve_backend_choice_marks_absent_kind_for_probe() {
    let cfg = newt_core::Config {
        backends: vec![newt_core::BackendConfig {
            name: "dgx1-llama".into(),
            endpoint: "http://host:8000".into(),
            // Minimal durable shape: name + endpoint only.
            kind: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let choice = crate::env_resolution_tests::with_env_vars(
        &[],
        &[
            "NEWT_DGX_MODEL",
            "NEWT_BACKEND",
            "NEWT_PROVIDER",
            "NEWT_DGX_OLLAMA_URL",
            "NEWT_DGX_HOST",
        ],
        || resolve_backend_choice(&cfg),
    );
    assert!(choice.kind_needs_probe, "absent kind must probe at adopt");
    assert_eq!(choice.name, "dgx1-llama");
    assert_eq!(choice.url, "http://host:8000");
}

#[test]
fn resolve_backend_choice_explicit_kind_skips_probe_flag() {
    let cfg = newt_core::Config {
        backends: vec![newt_core::BackendConfig {
            name: "local".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            kind: Some(newt_core::BackendKind::Ollama),
            ..Default::default()
        }],
        ..Default::default()
    };
    let choice = crate::env_resolution_tests::with_env_vars(
        &[],
        &[
            "NEWT_DGX_MODEL",
            "NEWT_BACKEND",
            "NEWT_PROVIDER",
            "NEWT_DGX_OLLAMA_URL",
            "NEWT_DGX_HOST",
        ],
        || resolve_backend_choice(&cfg),
    );
    assert!(!choice.kind_needs_probe);
    assert_eq!(choice.kind, newt_core::BackendKind::Ollama);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_detects_openai_when_kind_absent() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // OpenAI-only: /v1/models answers, /api/tags does not.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "nemotron"}]
        })))
        .mount(&server)
        .await;

    let mut choice = BackendChoice {
        name: "dgx1-llama".into(),
        serving: None,
        url: server.uri(),
        model: String::new(),
        kind: newt_core::BackendKind::Ollama, // placeholder
        kind_needs_probe: true,
        api_key: None,
        chat_completions_capability: Default::default(),
        reasoning_replay_scope: newt_core::model_card::ReasoningReplayScope::Never,
        api: newt_core::OpenAiApi::default(),
        api_needs_probe: false,
        context_window: None,
    };
    let lines = adopt_backend_choice(&mut choice, None);
    assert_eq!(choice.kind, newt_core::BackendKind::Openai);
    assert!(!choice.kind_needs_probe);
    assert_eq!(choice.model, "nemotron");
    assert!(
        lines.iter().any(|l| l.contains("detected openai")),
        "status lines={lines:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_detects_ollama_when_kind_absent() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "llama3.1:8b"}]
        })))
        .mount(&server)
        .await;

    let mut choice = BackendChoice {
        name: "local".into(),
        serving: None,
        url: server.uri(),
        model: String::new(),
        kind: newt_core::BackendKind::Openai, // wrong placeholder — probe must win
        kind_needs_probe: true,
        api_key: None,
        chat_completions_capability: Default::default(),
        reasoning_replay_scope: newt_core::model_card::ReasoningReplayScope::Never,
        api: newt_core::OpenAiApi::default(),
        api_needs_probe: false,
        context_window: None,
    };
    let _ = adopt_backend_choice(&mut choice, None);
    assert_eq!(choice.kind, newt_core::BackendKind::Ollama);
    assert_eq!(choice.model, "llama3.1:8b");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_respects_explicit_kind_without_detect() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // Only OpenAI surface lives here — an explicit ollama kind must NOT flip.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "should-not-adopt"}]
        })))
        .mount(&server)
        .await;

    let mut choice = BackendChoice {
        name: "pinned-ollama".into(),
        serving: None,
        url: server.uri(),
        model: "configured".into(),
        kind: newt_core::BackendKind::Ollama,
        kind_needs_probe: false,
        api_key: None,
        chat_completions_capability: Default::default(),
        reasoning_replay_scope: newt_core::model_card::ReasoningReplayScope::Never,
        api: newt_core::OpenAiApi::default(),
        api_needs_probe: false,
        context_window: None,
    };
    let lines = adopt_backend_choice(&mut choice, None);
    assert_eq!(choice.kind, newt_core::BackendKind::Ollama);
    assert_eq!(
        choice.model, "configured",
        "keep file hint when ollama probe fails"
    );
    assert!(
        lines.iter().any(|l| l.contains("unreachable")),
        "explicit ollama against openai-only endpoint should report unreachable, not detect; lines={lines:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_detects_authenticated_openai_with_bearer() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "gated-model"}]
        })))
        .mount(&server)
        .await;

    let mut choice = BackendChoice {
        name: "gated".into(),
        serving: None,
        url: server.uri(),
        model: String::new(),
        kind: newt_core::BackendKind::Ollama,
        kind_needs_probe: true,
        api_key: Some("secret-token".into()),
        chat_completions_capability: Default::default(),
        reasoning_replay_scope: newt_core::model_card::ReasoningReplayScope::Never,
        api: newt_core::OpenAiApi::default(),
        api_needs_probe: false,
        context_window: None,
    };
    let _ = adopt_backend_choice(&mut choice, None);
    assert_eq!(choice.kind, newt_core::BackendKind::Openai);
    assert_eq!(choice.model, "gated-model");
}

#[test]
fn prewarm_applies_is_url_equality_modulo_trailing_slash() {
    // The pre-warm probe is consumed only for the endpoint it ran against —
    // a first-run wizard may have rewritten the config since splash entry.
    assert!(crate::prewarm_applies(
        "http://gpu:11434",
        "http://gpu:11434/"
    ));
    assert!(!crate::prewarm_applies(
        "http://gpu:11434",
        "http://other:11434"
    ));
}

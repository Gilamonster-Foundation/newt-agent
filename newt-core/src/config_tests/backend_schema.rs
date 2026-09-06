use super::*;
use std::io::Write;

// Backend declarations, capability fields, receipt serialization, and authentication.

/// Step 24.10 (#559): summarizer knobs live in `summarizer.toml` now.
/// Defaults (absent file) reuse the session backend; timeout 60 / retries 1.
#[test]
fn backend_kind_embedded_parses_and_labels() {
    // #639: the config accepts `kind = "embedded"` so the summarizer (and a
    // backend) can select the in-process backend.
    #[derive(serde::Deserialize)]
    struct K {
        kind: BackendKind,
    }
    let k: K = toml::from_str("kind = \"embedded\"").unwrap();
    assert_eq!(k.kind, BackendKind::Embedded);
    assert_eq!(k.kind.label(), "embedded");
}

#[test]
fn backend_api_axis_defaults_and_parses() {
    // Absent → unset (probe-at-connect for openai backends).
    let def: BackendConfig =
        toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\nkind=\"openai\"\n").unwrap();
    assert_eq!(def.api, None);
    // Explicit responses opt-in.
    let resp: BackendConfig = toml::from_str(
        "endpoint=\"http://h:1\"\nmodel=\"gpt-5-codex\"\nkind=\"openai\"\napi=\"responses\"\n",
    )
    .unwrap();
    assert_eq!(resp.api, Some(OpenAiApi::Responses));
    // `chat` is an accepted alias for chat_completions.
    let alias: BackendConfig =
        toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\napi=\"chat\"\n").unwrap();
    assert_eq!(alias.api, Some(OpenAiApi::ChatCompletions));
}

#[test]
fn backend_slots_default_to_one_and_require_a_positive_value() {
    let defaulted: BackendConfig =
        toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\n").unwrap();
    assert_eq!(defaulted.slots.get(), 1);
    assert!(
        !toml::to_string(&defaulted).unwrap().contains("slots"),
        "the default should not bloat existing generated drop-ins"
    );

    let parallel: BackendConfig =
        toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\nslots=3\n").unwrap();
    assert_eq!(parallel.slots.get(), 3);
    assert!(toml::to_string(&parallel).unwrap().contains("slots = 3"));

    assert!(
        toml::from_str::<BackendConfig>("endpoint=\"http://h:1\"\nmodel=\"m\"\nslots=0\n").is_err()
    );
}

#[test]
fn serving_axis_fields_round_trip_and_stay_minimal() {
    // #1129 (epic #1126): the serving axis + host/coexist/ram_gib/card/
    // capability/provenance are all OPTIONAL — a legacy file with none of
    // them parses (None everywhere), and a full file round-trips.
    let legacy: BackendConfig = toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\n").unwrap();
    assert_eq!(legacy.serving, None);
    assert_eq!(legacy.host, None);
    assert_eq!(legacy.coexist, None);
    assert_eq!(legacy.managed, None);

    let full: BackendConfig = toml::from_str(
        "endpoint=\"http://dgx:8000\"\nkind=\"openai\"\nserving=\"multiplexer\"\n\
             managed=\"shared\"\n\
             host=\"dgx1\"\ncoexist=true\nram_gib=480.0\ncard=\"ornith-1.0-35b\"\n\
             [capability]\nthinking_default=true\n\
             [provenance]\nsource=\"newt setup v0.7.3\"\nderived_serving=true\n",
    )
    .unwrap();
    assert_eq!(full.serving, Some(Serving::Multiplexer));
    assert_eq!(full.managed, Some(ManagedMode::Shared));
    assert_eq!(full.host.as_deref(), Some("dgx1"));
    assert_eq!(full.coexist, Some(true));
    assert_eq!(full.ram_gib, Some(480.0));
    assert_eq!(full.card.as_deref(), Some("ornith-1.0-35b"));
    assert_eq!(
        full.capability.as_ref().and_then(|c| c.thinking_default),
        Some(true)
    );
    assert_eq!(
        full.provenance.as_ref().and_then(|p| p.derived_serving),
        Some(true)
    );

    // Serialization stays minimal: unset optional fields are skipped, so a
    // generated backends/<name>.toml doesn't bloat with nulls.
    let out = toml::to_string(&legacy).unwrap();
    assert!(!out.contains("serving"), "unset fields are skipped: {out}");
    assert!(!out.contains("managed"), "unset managed is skipped: {out}");
    assert!(!out.contains("provenance"));
}

#[test]
fn backend_reasoning_replay_scope_is_explicit_and_defaults_never() {
    let default_backend: BackendConfig =
        toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\n").unwrap();
    assert_eq!(
        default_backend.reasoning_replay_scope(),
        crate::model_card::ReasoningReplayScope::Never
    );

    let replay_backend: BackendConfig = toml::from_str(
        "endpoint=\"http://h:1\"\nmodel=\"m\"\n\
             [capability]\nreasoning_replay_scope=\"current_user_turn\"\n",
    )
    .unwrap();
    assert_eq!(
        replay_backend.reasoning_replay_scope(),
        crate::model_card::ReasoningReplayScope::CurrentUserTurn
    );
}

#[test]
fn backend_chat_completions_generation_policy_is_explicit_capability_data() {
    let backend: BackendConfig = toml::from_str(
        "endpoint=\"http://h:1\"\nmodel=\"m\"\nkind=\"openai\"\n\
             [capability.chat_completions]\ncognition=true\n\
             chat_template_kwargs=true\nparallel_tool_calls=false\n\
             bounded_reasoning_continuation=true\n",
    )
    .expect("chat-completions policy is valid capability data");

    let capability = serde_json::to_value(backend.capability.expect("capability present"))
        .expect("capability serializes");
    assert_eq!(capability["chat_completions"]["cognition"], true);
    assert_eq!(capability["chat_completions"]["chat_template_kwargs"], true);
    assert_eq!(capability["chat_completions"]["parallel_tool_calls"], false);
    assert_eq!(
        capability["chat_completions"]["bounded_reasoning_continuation"],
        true
    );
}

#[test]
fn derive_serving_rules() {
    // Ollama is ALWAYS a multiplexer, even with one model pulled today.
    assert_eq!(derive_serving(BackendKind::Ollama, 1), Serving::Multiplexer);
    assert_eq!(derive_serving(BackendKind::Ollama, 7), Serving::Multiplexer);
    // A vLLM instance declares exactly one model on /v1/models.
    assert_eq!(derive_serving(BackendKind::Openai, 1), Serving::Instance);
    // An OpenAI-compatible gateway fronting a fleet lists many.
    assert_eq!(derive_serving(BackendKind::Openai, 3), Serving::Multiplexer);
    // The in-process engine runs one GGUF.
    assert_eq!(derive_serving(BackendKind::Embedded, 1), Serving::Instance);
}

#[test]
fn backend_model_is_optional_and_read_via_effective_model() {
    // #1128 (epic #1126): a model-less backend file PARSES — "the server
    // dictates"; Phase B's adopt() fills it at session start. Previously
    // `model` was required, so such a drop-in failed to parse and was
    // silently skipped.
    let serverless: BackendConfig =
        toml::from_str("endpoint=\"http://h:8000\"\nkind=\"openai\"\n").unwrap();
    assert_eq!(serverless.model, None);
    assert_eq!(serverless.effective_model(), None);

    // A declared model reads through effective_model unchanged.
    let pinned: BackendConfig =
        toml::from_str("endpoint=\"http://h:1\"\nmodel=\"qwen3:32b\"\n").unwrap();
    assert_eq!(pinned.effective_model(), Some("qwen3:32b"));

    // An EMPTY model string counts as unset — it must never be sent as a
    // model name in a request.
    let empty: BackendConfig = toml::from_str("endpoint=\"http://h:1\"\nmodel=\"\"\n").unwrap();
    assert_eq!(empty.effective_model(), None);
}

/// Serde compatibility: a `Config` never serializes receipt state, and
/// an OLD drop-in body carrying `record = "operator_v1"` (plus keys newt
/// does not model) still loads as a `BackendConfig`.
#[test]
fn serde_receipts_never_serialize_and_old_records_still_load() {
    let cfg = Config::default();
    let body = toml::to_string_pretty(&cfg).unwrap();
    assert!(!body.contains("record"), "no record key: {body}");
    assert!(!body.contains("receipt"), "no receipt state: {body}");
    // The public type tolerates the (now file-private) tag key and
    // unknown siblings — forward/backward compatible.
    let b: BackendConfig =
        toml::from_str("endpoint = \"http://h:1\"\nrecord = \"operator_v1\"\nfuture_key = 1\n")
            .unwrap();
    assert_eq!(b.endpoint, "http://h:1");
}

#[test]
fn provider_model_is_optional_for_legacy_configs() {
    let cfg: Config = toml::from_str(
        r#"
[[providers]]
name = "legacy-cloud"
command = "newt-cloud-shim"
env_pass = ["CLOUD_TOKEN"]
tiers = ["COMPLEX"]
"#,
    )
    .unwrap();

    assert_eq!(cfg.providers.len(), 1);
    assert_eq!(cfg.providers[0].model, None);
}

fn openai_backend(api_key_file: Option<String>, api_key_env: Option<String>) -> BackendConfig {
    BackendConfig {
        name: "remote".into(),
        endpoint: "https://example.test".into(),
        model: Some("some-model".into()),
        model_path: None,
        tiers: vec![Tier::Fast],
        kind: Some(BackendKind::Openai),
        api: Default::default(),
        api_key_file,
        api_key_env,
        ..Default::default()
    }
}

#[test]
fn backend_kind_absent_means_probe_at_connect() {
    let toml = r#"
            [[backends]]
            name = "local"
            endpoint = "http://localhost:8000"
            model = "m"
            tiers = ["FAST"]
        "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.backends[0].kind, None);
    assert!(cfg.backends[0].needs_kind_probe());
    assert_eq!(cfg.backends[0].kind_label(), "auto");
    assert!(cfg.backends[0].api_key_file.is_none());
    assert!(cfg.backends[0].api_key_env.is_none());
}

#[test]
fn backend_kind_parses_openai_and_aliases() {
    for kind_str in ["openai", "vllm", "openai-compatible"] {
        let toml = format!(
                "[[backends]]\nname=\"x\"\nendpoint=\"http://e\"\nmodel=\"m\"\ntiers=[\"FAST\"]\nkind=\"{kind_str}\"\n"
            );
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(
            cfg.backends[0].kind,
            Some(BackendKind::Openai),
            "kind={kind_str}"
        );
    }
}

#[test]
fn backend_kind_label_is_protocol_name() {
    assert_eq!(BackendKind::Ollama.label(), "ollama");
    assert_eq!(BackendKind::Openai.label(), "openai");
}

#[test]
fn backend_config_roundtrips_auth_fields() {
    let cfg = openai_backend(Some("~/.newt/token".into()), Some("MY_TOKEN".into()));
    let toml = toml::to_string(&cfg).unwrap();
    assert!(toml.contains("kind = \"openai\""));
    assert!(toml.contains("api_key_file"));
    assert!(toml.contains("api_key_env"));
    let back: BackendConfig = toml::from_str(&toml).unwrap();
    assert_eq!(back.kind, Some(BackendKind::Openai));
    assert_eq!(back.api_key_file.as_deref(), Some("~/.newt/token"));
    assert_eq!(back.api_key_env.as_deref(), Some("MY_TOKEN"));
}

#[test]
fn resolve_api_key_reads_first_nonempty_line_of_file() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    // Leading blank line + surrounding whitespace must be skipped/trimmed.
    write!(f, "\n  secret-token-123  \nignored-second-line\n").unwrap();
    let cfg = openai_backend(Some(f.path().to_string_lossy().into_owned()), None);
    assert_eq!(cfg.resolve_api_key().as_deref(), Some("secret-token-123"));
}

#[test]
fn resolve_api_key_env_takes_precedence_over_file() {
    let var = "NEWT_TEST_API_KEY_PRECEDENCE";
    std::env::set_var(var, "  from-env  ");
    let mut f = tempfile::NamedTempFile::new().unwrap();
    writeln!(f, "from-file").unwrap();
    let cfg = openai_backend(
        Some(f.path().to_string_lossy().into_owned()),
        Some(var.into()),
    );
    assert_eq!(cfg.resolve_api_key().as_deref(), Some("from-env"));
    std::env::remove_var(var);
}

#[test]
fn resolve_api_key_none_when_unconfigured() {
    assert_eq!(openai_backend(None, None).resolve_api_key(), None);
}

#[test]
fn resolve_api_key_none_for_missing_file() {
    let cfg = openai_backend(Some("/no/such/newt/token/file".into()), None);
    assert_eq!(cfg.resolve_api_key(), None);
}

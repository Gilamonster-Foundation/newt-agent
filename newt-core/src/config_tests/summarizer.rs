use super::*;

// Separate-file summarizer configuration defaults and parsing.

#[test]
fn summarizer_config_defaults_and_parse() {
    let d = SummarizerConfig::default();
    assert_eq!(d.endpoint, None);
    assert_eq!(d.model, None);
    assert_eq!(d.kind, None);
    assert_eq!(d.timeout_secs, 60);
    assert_eq!(d.retries, 1);
    assert_eq!(d.fallback_model, None);

    let cfg = SummarizerConfig::from_toml_str(
        "endpoint = \"http://REDACTED-HOST:11434\"\n\
             model = \"qwen2.5-coder:3b\"\n\
             kind = \"openai\"\n\
             timeout_secs = 45\n\
             retries = 2\n\
             fallback_model = \"nemotron-mini:4b\"\n\
             keep_alive = \"10m\"",
    )
    .unwrap();
    assert_eq!(cfg.endpoint.as_deref(), Some("http://REDACTED-HOST:11434"));
    assert_eq!(cfg.model.as_deref(), Some("qwen2.5-coder:3b"));
    assert_eq!(cfg.kind, Some(BackendKind::Openai));
    assert_eq!(cfg.timeout_secs, 45);
    assert_eq!(cfg.retries, 2);
    assert_eq!(cfg.fallback_model.as_deref(), Some("nemotron-mini:4b"));
    assert_eq!(cfg.keep_alive.as_deref(), Some("10m"));
}

/// A partial file fills only the keys present; the rest stay at defaults
/// (so an `endpoint`-only file reuses the session model but a fast box).
#[test]
fn summarizer_config_partial_keeps_defaults() {
    let cfg = SummarizerConfig::from_toml_str("endpoint = \"http://fast.box:11434\"").unwrap();
    assert_eq!(cfg.endpoint.as_deref(), Some("http://fast.box:11434"));
    assert_eq!(cfg.model, None); // reuse session model
    assert_eq!(cfg.timeout_secs, 60); // default
    assert_eq!(cfg.retries, 1); // default
}

use super::{default_summarizer_choice, SummarizerChoice};

#[cfg(feature = "embedded")]
#[tokio::test]
async fn embedded_summarizer_honors_configured_zero_timeout_before_loading() {
    let dir = tempfile::tempdir().unwrap();
    let gguf = dir.path().join("fixture.gguf");
    std::fs::write(&gguf, b"not model weights").unwrap();
    std::fs::write(dir.path().join("tokenizer.json"), b"{}").unwrap();
    let cfg = newt_core::SummarizerConfig {
        timeout_secs: 0,
        ..Default::default()
    };
    let opts = super::summarizer_opts(&cfg, &newt_core::Config::default(), None, false);
    let summarize = super::make_loop_summarizer(
        String::new(),
        "qwen2.5-0.5b".into(),
        newt_core::BackendKind::Embedded,
        None,
        Some(gguf.to_string_lossy().into_owned()),
        opts,
    );
    let error = summarize("summarize this".into()).await.unwrap_err();
    assert!(
        error.to_string().contains("deadline"),
        "configured timeout was ignored: {error:#}"
    );
}

#[test]
fn summarizer_defaults_to_embedded_never_the_session_model() {
    // REGRESSION GUARD (#661 / feedback_summarizer_defaults_to_embedded_cpu):
    // with NO [summarizer] override and the embedded engine available, the
    // summarizer MUST default to the on-host embedded CPU engine — never the
    // session GPU model. If this flips, the codebase has slipped back.
    assert_eq!(
        default_summarizer_choice(Some("/models/qwen2.5-0.5b/x.gguf".to_string())),
        SummarizerChoice::Embedded("/models/qwen2.5-0.5b/x.gguf".to_string()),
    );
    // Only a genuinely-unavailable embedded engine degrades to the session.
    assert_eq!(
        default_summarizer_choice(None),
        SummarizerChoice::DegradedSession,
    );
}

/// The explicit ownership rule (Issue 2): which summarizers FOLLOW a live
/// `/model` / `/backend` switch and which stay PINNED.
#[test]
fn summarizer_ownership_decides_follow_vs_pinned() {
    use super::summarizer_follows_session;
    use newt_core::{BackendKind, SummarizerConfig};
    let gguf = || Some("/models/embed.gguf".to_string());

    // No override + embedded available → embedded engine, independent → PINNED.
    assert!(!summarizer_follows_session(
        &SummarizerConfig::default(),
        gguf()
    ));
    // No override + embedded UNavailable → degraded session reuse → FOLLOWS.
    assert!(summarizer_follows_session(
        &SummarizerConfig::default(),
        None
    ));
    // Pinned off-box endpoint → PINNED (never leaks onto the session host).
    assert!(!summarizer_follows_session(
        &SummarizerConfig {
            endpoint: Some("http://sum-host:11434".into()),
            model: Some("qwen2.5-1.5b".into()),
            ..Default::default()
        },
        gguf()
    ));
    // Pinned embedded kind / pinned GGUF path → independent → PINNED.
    assert!(!summarizer_follows_session(
        &SummarizerConfig {
            kind: Some(BackendKind::Embedded),
            ..Default::default()
        },
        None
    ));
    assert!(!summarizer_follows_session(
        &SummarizerConfig {
            model_path: Some("/models/pinned.gguf".into()),
            ..Default::default()
        },
        None
    ));
    // Partial override that only pins a MODEL but leaves the endpoint to
    // inherit inf_url → still targets the session backend → FOLLOWS.
    assert!(summarizer_follows_session(
        &SummarizerConfig {
            model: Some("small-summariser".into()),
            ..Default::default()
        },
        gguf()
    ));
}

use super::*;

fn openai(name: &str, api: OpenAiApi, endpoint: &str) -> BackendConfig {
    BackendConfig {
        name: name.into(),
        endpoint: endpoint.into(),
        model: Some("m".into()),
        tiers: vec![Tier::Fast],
        kind: Some(BackendKind::Openai),
        api: Some(api),
        ..Default::default()
    }
}

fn ollama(name: &str, endpoint: &str) -> BackendConfig {
    BackendConfig {
        name: name.into(),
        endpoint: endpoint.into(),
        model: Some("llama3.1:8b".into()),
        tiers: vec![Tier::Fast],
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    }
}

fn plugin(name: &str) -> ProviderConfig {
    ProviderConfig {
        name: name.into(),
        command: "newt-provider-openai".into(),
        model: Some("gpt-test".into()),
        env_pass: vec![],
        tiers: vec![Tier::Complex],
    }
}

fn cfg(
    backends: Vec<BackendConfig>,
    providers: Vec<ProviderConfig>,
    default: Option<&str>,
) -> Config {
    Config {
        backends,
        providers,
        default_backend: default.map(str::to_string),
        ..Config::default()
    }
}

/// An owned, comparable summary of a [`SelectionOutcome`] so a test can drop
/// the borrow on `Config` before asserting (keeps env-restore panic-safe).
fn summary(c: &Config) -> String {
    match c.select_backend() {
        SelectionOutcome::Selected(SelectedBackend::Configured(b)) => {
            format!("configured:{}:{}", b.name, b.endpoint)
        }
        SelectionOutcome::Selected(SelectedBackend::Provider(p)) => {
            format!("provider:{}", p.name)
        }
        SelectionOutcome::UnknownNamed(n) => format!("unknown:{n}"),
        SelectionOutcome::UnroutableNamed(n) => format!("unroutable:{n}"),
        SelectionOutcome::Unset => "unset".to_string(),
    }
}

/// Run `f` with `$NEWT_PROVIDER=value`, restoring the prior value afterwards.
/// The closure returns an OWNED value so no borrow escapes the restore.
fn with_newt_provider<T>(value: &str, f: impl FnOnce() -> T) -> T {
    // Serialize against every other NEWT_PROVIDER-touching test — the
    // guard's shared lock is what config/runtime tests hold; without it
    // this helper raced them (env is process-global).
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    let prev = std::env::var("NEWT_PROVIDER").ok();
    unsafe { std::env::set_var("NEWT_PROVIDER", value) };
    let out = f();
    match prev {
        Some(p) => unsafe { std::env::set_var("NEWT_PROVIDER", p) },
        None => unsafe { std::env::remove_var("NEWT_PROVIDER") },
    }
    out
}

/// Guarantee `$NEWT_PROVIDER` is unset for an env-free scenario (so the lane
/// is deterministic regardless of a stray ambient value), restoring after.
fn without_newt_provider<T>(f: impl FnOnce() -> T) -> T {
    // Same shared-lock serialization as `with_newt_provider`.
    let _g = crate::test_guard::GlobalSettingsGuard::acquire();
    let prev = std::env::var("NEWT_PROVIDER").ok();
    unsafe { std::env::remove_var("NEWT_PROVIDER") };
    let out = f();
    if let Some(p) = prev {
        unsafe { std::env::set_var("NEWT_PROVIDER", p) };
    }
    out
}

// 1. default_backend selects Ollama while OpenAI is ALSO configured.
//    "mixed ⇒ OpenAI wins" is WRONG when Ollama was explicitly selected.
#[test]
#[serial_test::serial(newt_provider_env)]
fn default_backend_selects_ollama_over_configured_openai() {
    let c = cfg(
        vec![
            ollama("local", "http://ollama:11434/"),
            openai("cloud", OpenAiApi::ChatCompletions, "http://vllm:8000/"),
        ],
        vec![],
        Some("local"),
    );
    assert_eq!(
        without_newt_provider(|| summary(&c)),
        "configured:local:http://ollama:11434/"
    );
}

// 2. $NEWT_PROVIDER selects Ollama (over an also-configured OpenAI backend).
#[test]
#[serial_test::serial(newt_provider_env)]
fn newt_provider_selects_ollama() {
    let c = cfg(
        vec![
            ollama("local", "http://ollama:11434/"),
            openai("cloud", OpenAiApi::ChatCompletions, "http://vllm:8000/"),
        ],
        vec![],
        None,
    );
    assert_eq!(
        with_newt_provider("local", || summary(&c)),
        "configured:local:http://ollama:11434/"
    );
}

// 3. $NEWT_PROVIDER selects the OpenAI *Chat Completions* backend by name.
#[test]
#[serial_test::serial(newt_provider_env)]
fn newt_provider_selects_openai_chat_completions() {
    let c = cfg(
        vec![
            openai(
                "cloud-chat",
                OpenAiApi::ChatCompletions,
                "http://chat:8000/",
            ),
            openai("cloud-resp", OpenAiApi::Responses, "http://resp:8000/"),
        ],
        vec![],
        None,
    );
    assert_eq!(
        with_newt_provider("cloud-chat", || summary(&c)),
        "configured:cloud-chat:http://chat:8000/"
    );
}

// 4. $NEWT_PROVIDER selects the OpenAI *Responses* backend by name — the same
//    config as (3), a different selector, a different destination.
#[test]
#[serial_test::serial(newt_provider_env)]
fn newt_provider_selects_openai_responses() {
    let c = cfg(
        vec![
            openai(
                "cloud-chat",
                OpenAiApi::ChatCompletions,
                "http://chat:8000/",
            ),
            openai("cloud-resp", OpenAiApi::Responses, "http://resp:8000/"),
        ],
        vec![],
        None,
    );
    assert_eq!(
        with_newt_provider("cloud-resp", || summary(&c)),
        "configured:cloud-resp:http://resp:8000/"
    );
}

// 5. A selected provider-plugin backend (named via default_backend), even
//    with an OpenAI backend also present.
#[test]
#[serial_test::serial(newt_provider_env)]
fn selects_provider_plugin_when_named() {
    let c = cfg(
        vec![openai(
            "cloud",
            OpenAiApi::ChatCompletions,
            "http://vllm:8000/",
        )],
        vec![plugin("myplugin")],
        Some("myplugin"),
    );
    assert_eq!(without_newt_provider(|| summary(&c)), "provider:myplugin");
}

// 6. An explicitly selected UNSUPPORTED backend still selects *that* entry —
//    the "unsupported" verdict is the instantiator's job (worker suite), not
//    a reason for the selector to pick a different backend.
#[test]
#[serial_test::serial(newt_provider_env)]
fn explicitly_selected_backend_is_returned_even_if_unusual_kind() {
    let mut embedded = BackendConfig {
        name: "in-proc".into(),
        endpoint: "http://in-proc/".into(),
        kind: Some(BackendKind::Embedded),
        model: Some("tiny".into()),
        ..Default::default()
    };
    embedded.tiers = vec![Tier::Fast];
    let c = cfg(
        vec![
            embedded,
            openai("cloud", OpenAiApi::ChatCompletions, "http://vllm:8000/"),
        ],
        vec![],
        Some("in-proc"),
    );
    // The Embedded backend is what was selected — NOT the OpenAI one.
    assert_eq!(
        without_newt_provider(|| summary(&c)),
        "configured:in-proc:http://in-proc/"
    );
}

// 7. No configured backend ⇒ Unset, which alone permits local discovery.
#[test]
#[serial_test::serial(newt_provider_env)]
fn no_configured_backend_is_unset() {
    let c = cfg(vec![], vec![], None);
    assert_eq!(without_newt_provider(|| summary(&c)), "unset");
}

// 8. An explicit selector naming a nonexistent entry is UnknownNamed — an
//    operator error, NOT a silent fallback to the present OpenAI backend.
#[test]
#[serial_test::serial(newt_provider_env)]
fn unknown_named_backend_is_an_error_not_a_fallback() {
    let c = cfg(
        vec![openai(
            "cloud",
            OpenAiApi::ChatCompletions,
            "http://vllm:8000/",
        )],
        vec![],
        Some("ghost"),
    );
    assert_eq!(without_newt_provider(|| summary(&c)), "unknown:ghost");
    // And the same via $NEWT_PROVIDER (the live override), which must not
    // silently defer to default_backend or to preference.
    let c2 = cfg(
        vec![openai(
            "cloud",
            OpenAiApi::ChatCompletions,
            "http://vllm:8000/",
        )],
        vec![],
        None,
    );
    assert_eq!(with_newt_provider("typo", || summary(&c2)), "unknown:typo");
}

// Guard: preference still prefers OpenAI when NOTHING is explicitly selected
// (the historical default is preserved — only explicit selection overrides it).
#[test]
#[serial_test::serial(newt_provider_env)]
fn prefers_openai_when_nothing_is_explicitly_selected() {
    let c = cfg(
        vec![
            ollama("local", "http://ollama:11434/"),
            openai("cloud", OpenAiApi::ChatCompletions, "http://vllm:8000/"),
        ],
        vec![],
        None,
    );
    assert_eq!(
        without_newt_provider(|| summary(&c)),
        "configured:cloud:http://vllm:8000/"
    );
}

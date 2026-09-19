//! #2450: ground initiative resolution and counter semantics in outgoing Ollama
//! requests. Real temporary files ground the read/write classification; the
//! backend alone is scripted, so no inference or external service is needed.
use super::*;
use crate::initiative::{self, Initiative, InitiativeConfig, InitiativeRounds};

fn configure() {
    initiative::clear_cli_initiative();
    initiative::set_persona_initiative(None);
    initiative::set_active_model_family(Some("fixture-family".to_string()));
    initiative::set_initiative_config(InitiativeConfig {
        default: Some(Initiative::Measured),
        families: [("fixture-family".to_string(), Initiative::Patient)].into(),
        rounds: InitiativeRounds {
            patient: 5,
            measured: 4,
            decisive: 3,
            eager: 2,
        },
    });
}

/// Return the request indices at which a NEW action nudge appears. Index zero
/// is the first generation, before any tool round has completed.
async fn nudge_rounds(
    write_at: Option<usize>,
    enabled: bool,
    disposition: PromptDisposition,
    republish: bool,
) -> Vec<usize> {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..7 {
        std::fs::write(
            dir.path().join(format!("input{i}.txt")),
            "fixture content\n",
        )
        .unwrap();
    }
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |_req: &Request| {
            let i = seen.fetch_add(1, Ordering::SeqCst);
            if republish && i == 0 {
                // A publisher in a sibling context must not retune this turn.
                initiative::set_initiative_config(InitiativeConfig::default());
                initiative::set_cli_initiative(Initiative::Eager);
            }
            let message = if i < 7 {
                let (name, args) = if write_at == Some(i) {
                    (
                        "write_file",
                        serde_json::json!({"path":"output.txt","content":"changed\n"}),
                    )
                } else {
                    (
                        "read_file",
                        serde_json::json!({"path":format!("input{i}.txt")}),
                    )
                };
                serde_json::json!({"role":"assistant","content":"", "tool_calls":[
                    {"function":{"name":name,"arguments":args}}
                ]})
            } else {
                serde_json::json!({"role":"assistant","content":"Finished."})
            };
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message":message,"done":true}))
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let mut caveats = Caveats::top();
    caveats.fs_write = crate::caveats::Scope::only([dir.path().to_string_lossy().into_owned()]);
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.workspace = dir.path().to_str().unwrap();
    c.max_tool_rounds = 12;
    c.action_nudges = enabled;
    c.prompt_disposition = disposition;
    c.mid_loop_trim_threshold = 100;
    let turn = crate::psyche::capture_turn_psyche();
    let (reply, ..) = chat_complete(c, &mut NoMcp).await.expect("scripted turn");
    drop(turn);
    assert_eq!(reply, "Finished.");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        8,
        "seven tools and one answer"
    );
    if write_at.is_some() {
        assert_eq!(
            std::fs::read_to_string(dir.path().join("output.txt")).unwrap(),
            "changed\n"
        );
    }
    let requests = server.received_requests().await.unwrap();
    let mut previous = 0;
    requests
        .iter()
        .enumerate()
        .filter_map(|(i, request)| {
            let body = body_json(request);
            let count = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| {
                    message["role"] == "user"
                        && message["content"].as_str().is_some_and(|content| {
                            content.contains("read-only rounds so far. Stop AIMLESS")
                        })
                })
                .count();
            let added = count > previous;
            previous = count;
            added.then_some(i)
        })
        .collect()
}

/// Before #2450 every case used Measured's four rounds, ignoring both the
/// family-selected Patient and explicit operator overrides in the live loop.
#[tokio::test]
async fn selected_levels_and_family_default_drive_ollama_requests() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    for (level, expected) in [
        (None, vec![5]),
        (Some(Initiative::Measured), vec![4]),
        (Some(Initiative::Decisive), vec![3, 6]),
        (Some(Initiative::Eager), vec![2, 4, 6]),
    ] {
        configure();
        if let Some(level) = level {
            initiative::set_cli_initiative(level);
        }
        assert_eq!(
            nudge_rounds(None, true, PromptDisposition::Act, false).await,
            expected,
            "selection {level:?}"
        );
    }
}

/// #2450 must preserve reset-on-write: two reads, a successful write, then
/// four more reads never spend Patient's five-consecutive-read budget.
#[tokio::test]
async fn successful_write_resets_the_ollama_counter() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    configure();
    assert!(nudge_rounds(Some(2), true, PromptDisposition::Act, false)
        .await
        .is_empty());
}

/// Selecting Eager must not bypass either the operator's nudge switch or
/// the prompt-disposition action gate.
#[tokio::test]
async fn initiative_preserves_disabled_and_non_action_gates() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    configure();
    initiative::set_cli_initiative(Initiative::Eager);
    for (enabled, disposition) in [
        (false, PromptDisposition::Act),
        (true, PromptDisposition::Explain),
    ] {
        assert!(nudge_rounds(None, enabled, disposition, false)
            .await
            .is_empty());
    }
}

/// #2450 reads the captured level AND captured configured number: publishing
/// defaults during the first request cannot change Patient's five to twelve,
/// or switch the running turn to the new Eager override.
#[tokio::test]
async fn ollama_keeps_captured_rounds_when_config_is_republished() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    configure();
    assert_eq!(
        nudge_rounds(None, true, PromptDisposition::Act, true).await,
        vec![5]
    );
    assert_eq!(
        initiative::effective_initiative(),
        Initiative::Eager,
        "control: publication actually happened"
    );
    assert_eq!(Initiative::Patient.read_only_nudge_after(), 12);
}

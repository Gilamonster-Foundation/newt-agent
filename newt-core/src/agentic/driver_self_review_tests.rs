// #2449 actual HTTP regressions for the missing independent completion gate.
// Selection is test-host assembly from real profile/persona/plan parsers; this
// does not claim newt-cli LocalCrewRunner or TUI host forwarding is complete.

#[derive(Clone, Copy)]
enum SelfReviewSelector {
    Profile,
    Persona,
    Plan,
    None,
}

#[derive(Default)]
struct SelfReviewPlanProjection(std::sync::Mutex<Vec<serde_json::Value>>);

#[async_trait::async_trait]
impl CrewRunner for SelfReviewPlanProjection {
    async fn dispatch(
        &self,
        op: &str,
        args: &serde_json::Value,
        caveats: &Caveats,
        _context: crate::agentic::CrewDispatchContext<'_>,
    ) -> Result<String, String> {
        assert_eq!(op, "crew");
        assert!(!crate::CaveatsExt::permits_exec(caveats, "git"));
        self.0.lock().unwrap().push(args.clone());
        Ok("fixture projection only; no branch artifact".into())
    }
}

async fn self_review_policy(
    selector: SelfReviewSelector,
) -> Option<crate::kit::CapturedTechniques> {
    use crate::config::{Config, Loadout, PickVia, ProfileConfig, ProfilePick};
    use crate::kit::{CapturedTechniques, TechniqueSource};
    let pick = ProfilePick {
        name: "review".into(),
        via: PickVia::Profile,
    };
    let knobs: ProfileConfig = toml::from_str("[self_review]\nmax_rounds = 3").unwrap();
    let policy = match selector {
        SelfReviewSelector::None => return None,
        SelfReviewSelector::Profile => {
            let mut cfg = Config::default();
            cfg.profiles.insert(
                "review".into(),
                toml::from_str::<ProfileConfig>("techniques = [\"self_review\"]").unwrap(),
            );
            let loadout: Loadout = toml::from_str("profile = \"review\"").unwrap();
            loadout.validate(&cfg).unwrap();
            let selected = cfg
                .pick_active_profile(loadout.profile.as_deref(), None, None)
                .unwrap()
                .unwrap();
            CapturedTechniques::capture(
                Some((&selected, cfg.resolve_profile(&selected.name).unwrap())),
                [],
            )
            .unwrap()
        }
        SelfReviewSelector::Persona => {
            let role = crate::RoleProfile::parse(
                "+++\ntechniques = [\"self_review\"]\n+++\nInspect the supplied artifact.",
            )
            .unwrap();
            assert_eq!(role.cognition, None);
            CapturedTechniques::capture(
                Some((&pick, &knobs)),
                [(
                    TechniqueSource::Persona {
                        name: "reviewer".into(),
                    },
                    role.techniques,
                )],
            )
            .unwrap()
        }
        SelfReviewSelector::Plan => {
            let mut plan = crate::plan::Plan::from_toml_str("[[subtask]]\nid = \"inspect\"\ninstruction = \"Inspect supplied artifact\"\ntechniques = [\"self_review\"]").unwrap();
            let runner = SelfReviewPlanProjection::default();
            let result =
                super::super::plan_exec::run_plan(&mut plan, &Caveats::top(), &runner).await;
            assert!(result.complete);
            let args = runner.0.lock().unwrap()[0].clone();
            let selectors: Vec<String> =
                serde_json::from_value(args["techniques"].clone()).unwrap();
            CapturedTechniques::capture(
                Some((&pick, &knobs)),
                [(
                    TechniqueSource::PlanStep {
                        id: plan.subtasks[0].id.clone(),
                    },
                    selectors,
                )],
            )
            .unwrap()
        }
    };
    assert_eq!(policy.self_review().unwrap().max_rounds, 3);
    Some(policy)
}

async fn self_review_wire_case(wire: &'static str, smart: bool, selector: SelfReviewSelector) {
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    let _wire_env = super::super::anthropic_loop_tests::test_env(false);
    let _output_env = super::super::TestEnvGuard::unset("NEWT_ANTHROPIC_MAX_TOKENS");
    crate::cognition::set_cli_cognition(crate::cognition::CognitionOverride::Off);
    crate::tenacity::set_cli_tenacity(crate::Tenacity::Normal);
    let server = MockServer::start().await;
    let endpoint = match wire {
        "ollama" => "/api/chat",
        "responses" => "/v1/responses",
        "anthropic" => "/v1/messages",
        _ => "/v1/chat/completions",
    };
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    Mock::given(method("POST")).and(path(endpoint)).respond_with(move |_request: &Request| {
        // A bare final answer first; even if review is requested later, merely
        // saying "reviewed" is not structured, subject-bound review evidence.
        let text = if count.fetch_add(1, Ordering::SeqCst) == 0 { "done" } else { "reviewed" };
        let body = match wire {
            "ollama" => serde_json::json!({"message":{"role":"assistant","content":text},"done":true}),
            "responses" => serde_json::json!({"id":"resp_fixture","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":text}]}]}),
            "anthropic" => serde_json::json!({"id":"msg_fixture","type":"message","role":"assistant","model":"fixture","stop_reason":"end_turn","content":[{"type":"text","text":text}],"usage":{"input_tokens":10,"output_tokens":5}}),
            _ => serde_json::json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]}),
        };
        ResponseTemplate::new(200).set_body_json(body)
    }).mount(&server).await;
    let workspace = tempfile::tempdir().unwrap();
    const SUBJECT: &str =
        "ACTUAL_REVIEW_SUBJECT_SENTINEL: unchecked index access in the supplied artifact";
    std::fs::write(workspace.path().join("subject.txt"), SUBJECT).unwrap();
    let kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    let mut config = TurnDriverConfig::new(
        server.uri(),
        "fixture",
        kind,
        workspace.path().to_string_lossy(),
    );
    config.openai_api = if wire == "responses" {
        crate::OpenAiApi::Responses
    } else {
        crate::OpenAiApi::ChatCompletions
    };
    config.max_tool_rounds = 5;
    config.workflow_grace_rounds = 0;
    config.narration_nudge_cap = 0;
    config.inference_timeout_secs = 5;
    config.connect_timeout_secs = 2;
    config.run_allowance = Some(5);
    config.techniques = self_review_policy(selector).await;
    // Typed host input owns membership; the task's prose is not an authority
    // source or a parser for read-only review subjects.
    config.review_subject = Some(crate::self_review::ReviewSubject::Artifacts {
        paths: vec!["subject.txt".into()],
    });
    let selected = config.techniques.is_some();
    if smart {
        config.smart_harness = Some(Arc::new(
            super::super::smart_harness::SmartHarness::new(
                agent_harness::Session::new(Default::default()).unwrap(),
                Arc::new(|_| Box::pin(async { Ok(("\"answer\"".to_string(), None)) })),
                Default::default(),
            )
            .unwrap(),
        ));
    }
    let mut driver = TurnDriver::new(config).with_cognition(None);
    driver
        .submit("Review the existing subject.txt artifact before completing. Do not modify files.")
        .unwrap();
    let TurnStatus::Completed(outcome) = pump_to_done(&mut driver).await else {
        panic!("no terminal outcome")
    };
    let all_requests = server.received_requests().await.unwrap();
    let requests: Vec<_> = all_requests
        .iter()
        .filter(|request| request.url.path() == endpoint)
        .collect();
    assert!(
        !requests.is_empty(),
        "fixture never reached {endpoint}: {outcome:?}"
    );
    assert_eq!(outcome.semantic_cognition, None);
    for request in &requests {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("thinking").is_none());
        assert!(body.get("think").is_none());
    }
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("subject.txt")).unwrap(),
        SUBJECT
    );
    assert!(
        outcome.tool_events.is_empty(),
        "the fixture supplies no tool calls"
    );
    if selected {
        assert!(
            requests.len() >= 2,
            "independent self_review bypassed: wire={wire}, smart={smart}, outcome={outcome:?}"
        );
        assert!(
            requests
                .iter()
                .skip(1)
                .any(|request| String::from_utf8_lossy(&request.body).contains(SUBJECT)),
            "review request omitted actual subject bytes"
        );
        assert_ne!(
            outcome.end_reason,
            Some(crate::TurnEndReason::Completed),
            "bare reviewed text cannot discharge selected review"
        );
    } else {
        assert_eq!(
            requests.len(),
            1,
            "Normal without a selected technique must stay unchanged"
        );
        assert_eq!(outcome.error, None);
        // Preserve the pre-existing optional-off Responses return: unlike
        // its strict/Smart path, ordinary Normal success leaves this unset.
        let legacy_end = if wire == "responses" && !smart {
            None
        } else {
            Some(crate::TurnEndReason::Completed)
        };
        assert_eq!(outcome.end_reason, legacy_end);
        assert_eq!(outcome.reply, "done");
    }
}

macro_rules! self_review_wire_test {
    ($name:ident, $wire:literal, $smart:literal, $selector:ident) => {
        /// #2449: an independent selector must reach the real wire completion
        /// boundary with cognition off; no production review phase is mocked.
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env)]
        async fn $name() {
            self_review_wire_case($wire, $smart, SelfReviewSelector::$selector).await;
        }
    };
}
self_review_wire_test!(
    self_review_wire_ollama_ordinary_profile,
    "ollama",
    false,
    Profile
);
self_review_wire_test!(
    self_review_wire_ollama_ordinary_persona,
    "ollama",
    false,
    Persona
);
self_review_wire_test!(self_review_wire_ollama_ordinary_plan, "ollama", false, Plan);
self_review_wire_test!(
    self_review_wire_ollama_ordinary_unselected,
    "ollama",
    false,
    None
);
self_review_wire_test!(
    self_review_wire_ollama_smart_profile,
    "ollama",
    true,
    Profile
);
self_review_wire_test!(
    self_review_wire_ollama_smart_persona,
    "ollama",
    true,
    Persona
);
self_review_wire_test!(self_review_wire_ollama_smart_plan, "ollama", true, Plan);
self_review_wire_test!(
    self_review_wire_ollama_smart_unselected,
    "ollama",
    true,
    None
);
self_review_wire_test!(
    self_review_wire_chat_ordinary_profile,
    "chat",
    false,
    Profile
);
self_review_wire_test!(
    self_review_wire_chat_ordinary_persona,
    "chat",
    false,
    Persona
);
self_review_wire_test!(self_review_wire_chat_ordinary_plan, "chat", false, Plan);
self_review_wire_test!(
    self_review_wire_chat_ordinary_unselected,
    "chat",
    false,
    None
);
self_review_wire_test!(self_review_wire_chat_smart_profile, "chat", true, Profile);
self_review_wire_test!(self_review_wire_chat_smart_persona, "chat", true, Persona);
self_review_wire_test!(self_review_wire_chat_smart_plan, "chat", true, Plan);
self_review_wire_test!(self_review_wire_chat_smart_unselected, "chat", true, None);
self_review_wire_test!(
    self_review_wire_responses_ordinary_profile,
    "responses",
    false,
    Profile
);
self_review_wire_test!(
    self_review_wire_responses_ordinary_persona,
    "responses",
    false,
    Persona
);
self_review_wire_test!(
    self_review_wire_responses_ordinary_plan,
    "responses",
    false,
    Plan
);
self_review_wire_test!(
    self_review_wire_responses_ordinary_unselected,
    "responses",
    false,
    None
);
self_review_wire_test!(
    self_review_wire_responses_smart_profile,
    "responses",
    true,
    Profile
);
self_review_wire_test!(
    self_review_wire_responses_smart_persona,
    "responses",
    true,
    Persona
);
self_review_wire_test!(
    self_review_wire_responses_smart_plan,
    "responses",
    true,
    Plan
);
self_review_wire_test!(
    self_review_wire_responses_smart_unselected,
    "responses",
    true,
    None
);
self_review_wire_test!(
    self_review_wire_anthropic_ordinary_profile,
    "anthropic",
    false,
    Profile
);
self_review_wire_test!(
    self_review_wire_anthropic_ordinary_persona,
    "anthropic",
    false,
    Persona
);
self_review_wire_test!(
    self_review_wire_anthropic_ordinary_plan,
    "anthropic",
    false,
    Plan
);
self_review_wire_test!(
    self_review_wire_anthropic_ordinary_unselected,
    "anthropic",
    false,
    None
);
self_review_wire_test!(
    self_review_wire_anthropic_smart_profile,
    "anthropic",
    true,
    Profile
);
self_review_wire_test!(
    self_review_wire_anthropic_smart_persona,
    "anthropic",
    true,
    Persona
);
self_review_wire_test!(
    self_review_wire_anthropic_smart_plan,
    "anthropic",
    true,
    Plan
);
self_review_wire_test!(
    self_review_wire_anthropic_smart_unselected,
    "anthropic",
    true,
    None
);

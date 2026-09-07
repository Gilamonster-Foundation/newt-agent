use super::*;

pub(in crate::agentic) const STALE_BLOCKER: &str =
    "The lib.rs file I was editing grew from ~9400 to ~16808 lines between reads. \
     Why I'm blocked: I cannot safely use edit_file because the file has been \
     modified out from under me. The operator should restore lib.rs.";
pub(in crate::agentic) const STALE_RECOVERED: &str =
    "Ground truth checked; lib.rs is clean, so I am continuing.";

/// Catalog-only collaborator: these scripted prose turns must not execute Git.
pub(in crate::agentic) struct StaleCatalogGit;
impl crate::agentic::git_tool::GitTool for StaleCatalogGit {
    fn dispatch(
        &self,
        op: &str,
        _: &serde_json::Value,
        _: &crate::git_caveats::GitCaveats,
        _: &Caveats,
    ) -> Result<String, String> {
        panic!("unexpected Git dispatch in stale-guidance fixture: {op}");
    }
}

pub(in crate::agentic) fn stale_workspace() -> (tempfile::TempDir, Caveats) {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("lib.rs"), "// unchanged\n").unwrap();
    let caveats = Caveats {
        fs_read: crate::Scope::only([workspace.path().to_string_lossy().into_owned()]),
        ..tools::plan_phase_clamp()
    };
    (workspace, caveats)
}

pub(in crate::agentic) fn assert_scoped_stale_guidance(requests: &[Request]) {
    let mut saw_scoped_git = false;
    for request in requests {
        let body = body_json(request);
        if let Some(defs) = body["tools"].as_array() {
            let git = defs
                .iter()
                .find(|tool| tool["function"]["name"] == "git" || tool["name"] == "git")
                .expect("the scoped native Git catalog is present");
            let operations = git
                .pointer("/function/parameters/properties/op/enum")
                .or_else(|| git.pointer("/parameters/properties/op/enum"))
                .or_else(|| git.pointer("/input_schema/properties/op/enum"));
            assert_eq!(operations, Some(&serde_json::json!(["branch-list"])));
            saw_scoped_git = true;
        }
        let messages = body.get("messages").or_else(|| body.get("input")).unwrap();
        for message in messages.as_array().unwrap() {
            if message["role"] == "user" {
                let text = message["content"].to_string();
                for forbidden in [
                    "git status",
                    "git diff",
                    "wc -l",
                    "run_command",
                    "edit_file",
                    "write_file",
                    "request_permissions",
                ] {
                    assert!(
                        !text.contains(forbidden),
                        "impossible stale-file guidance: {text}"
                    );
                }
            }
        }
    }
    assert!(saw_scoped_git);
}

#[tokio::test]
async fn confined_act_openai_stale_blocker_respects_scoped_read_authority() {
    run_stale_http_wire(false, false, false).await;
    run_stale_http_wire(false, false, true).await;
}

#[tokio::test]
async fn confined_act_ollama_stale_blocker_respects_scoped_read_authority() {
    run_stale_http_wire(true, false, false).await;
    run_stale_http_wire(true, false, true).await;
}

#[tokio::test]
async fn confined_act_responses_stale_blocker_keeps_no_pressure() {
    run_stale_http_wire(false, true, false).await;
}

/// Exercise the native scoped catalog through the real converter and wire
/// validator, then pin strict optional-scope semantics without a second schema
/// validator: every field is required, while null retains the default scope.
#[test]
fn confined_act_scoped_git_catalog_preserves_responses_strict_contract() {
    let definition = crate::agentic::git_tool::definition_for_read_scope(&crate::Scope::only([
        "workspace".to_string(),
    ]));
    let tools = tools_to_responses(&serde_json::json!([definition]));
    let body = serde_json::json!({
        "model": "test-model",
        "store": crate::responses_wire::STORE_RESPONSE_SERVER_SIDE,
        "instructions": "be terse",
        "input": [{"role": "user", "content": "Count the branches."}],
        "tools": tools,
    });
    let policy = responses_wire_validation::ResponsesWirePolicy {
        store: crate::responses_wire::STORE_RESPONSE_SERVER_SIDE,
        tools_permitted: true,
        model: "test-model",
        authoritative_budget: None,
        calibration: 1.0,
        estimation: crate::tokens::TokenEstimation::default(),
        spill: None,
        compaction: None,
    };
    responses_wire_validation::validate_responses_request(&body, &policy)
        .expect("the actual native scoped Git schema must be usable on Responses");
    let git = &body["tools"][0];
    assert_eq!(git["strict"], true);
    let parameters = &git["parameters"];
    assert_eq!(parameters["additionalProperties"], false);
    assert_eq!(parameters["required"], serde_json::json!(["op", "scope"]));
    let properties = parameters["properties"].as_object().unwrap();
    assert_eq!(
        properties.len(),
        2,
        "no broader Git operations or arguments"
    );
    assert_eq!(properties["op"]["enum"], serde_json::json!(["branch-list"]));
    assert_eq!(
        properties["scope"]["type"],
        serde_json::json!(["string", "null"])
    );
    assert_eq!(
        properties["scope"]["enum"],
        serde_json::json!(["local", "remote", "all", null])
    );
}

async fn run_stale_http_wire(ollama: bool, responses: bool, full_authority: bool) {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::tenacity::Tenacity::Standard);
    let (workspace, scoped) = stale_workspace();
    let caveats = if full_authority {
        Caveats::top()
    } else {
        scoped
    };
    let original = caveats.clone();
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    let round_seen = round.clone();
    Mock::given(method("POST")).respond_with(move |request: &Request| {
        let stream = body_json(request)["stream"] == true;
        let index = if stream { round_seen.load(Ordering::SeqCst).saturating_sub(1) }
            else { round_seen.fetch_add(1, Ordering::SeqCst) };
        let text = if index == 0 { STALE_BLOCKER } else { STALE_RECOVERED };
        let reply = if responses {
            serde_json::json!({"status": "completed", "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}]})
        } else if ollama {
            serde_json::json!({"message": {"role": "assistant", "content": text}, "done": true})
        } else if stream {
            let frame = serde_json::json!({"choices": [{"delta": {"content": text}}]});
            return ResponseTemplate::new(200).set_body_raw(format!("data: {frame}\n\ndata: [DONE]\n\n"), "text/event-stream");
        } else {
            serde_json::json!({"choices": [{"message": {"role": "assistant", "content": text}}]})
        };
        ResponseTemplate::new(200).set_body_json(reply)
    }).mount(&server).await;
    let task = "Inspect lib.rs and report what is blocking the task.";
    let messages = vec![MemMessage::user(task)];
    let uri = server.uri();
    let workspace_path = workspace.path().to_string_lossy();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = if ollama {
        BackendKind::Ollama
    } else {
        BackendKind::Openai
    };
    c.workspace = &workspace_path;
    c.task = task;
    c.git_tool = Some(&StaleCatalogGit);
    let (reply, _, _, _) = if responses {
        openai_responses_complete(c, &mut NoMcp).await
    } else {
        chat_complete(c, &mut NoMcp).await
    }
    .unwrap();
    let rounds = round.load(Ordering::SeqCst);
    assert!(
        reply == STALE_BLOCKER || reply == STALE_RECOVERED,
        "{reply}"
    );
    assert_eq!(caveats, original);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("lib.rs")).unwrap(),
        "// unchanged\n"
    );
    let requests = server.received_requests().await.unwrap();
    if full_authority {
        assert_eq!(rounds, 2, "full authority retains the existing recovery");
        assert_eq!(reply, STALE_RECOVERED);
    } else {
        assert!((1..=2).contains(&rounds));
        assert_scoped_stale_guidance(&requests);
    }
}

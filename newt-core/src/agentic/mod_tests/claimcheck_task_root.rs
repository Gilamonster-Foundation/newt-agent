//! #2787: real linked checkouts ground final-answer claim checks, not a mocked root.
use super::*;
use wiremock::{Mock, MockServer, ResponseTemplate};

pub(super) fn git(root: &std::path::Path, home: &std::path::Path, args: &[&str]) {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(root).env_clear();
    for key in ["PATH", "SystemRoot", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join("empty-config"))
        .env("GIT_TEMPLATE_DIR", home.join("templates"))
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid");
    let output = cmd
        .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath="])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("empty-config"), "").unwrap();
    std::fs::create_dir(temp.path().join("templates")).unwrap();
    let original = temp.path().join("original");
    // #2787 round 2: an input spelling need not equal the canonical task
    // locator (Windows temp roots can use short names). Exercise that on all OSes.
    let task = temp.path().join(".").join("task [bound]");
    std::fs::create_dir(&original).unwrap();
    git(&original, temp.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(original.join("tracked.txt"), "base\n").unwrap();
    git(&original, temp.path(), &["add", "."]);
    git(&original, temp.path(), &["commit", "-qm", "base"]);
    git(
        &original,
        temp.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task",
            task.to_str().unwrap(),
        ],
    );
    // A distinct task HEAD proves the baseline must also come from this tree.
    git(
        &task,
        temp.path(),
        &["commit", "--allow-empty", "-qm", "task baseline"],
    );
    std::fs::create_dir(task.join("src")).unwrap();
    std::fs::write(task.join("src/task.rs"), "// task only\n").unwrap();
    std::fs::write(task.join("tracked.txt"), "modified\n").unwrap();
    (temp, original, task)
}

async fn check(bound: bool, citation: bool, wire: &str) -> Vec<serde_json::Value> {
    use crate::agentic::tools::disable_ocap_tests::env_lock;
    let _lock = env_lock().await;
    let (_temp, original, task) = fixture();
    let session = crate::worktree_adoption::WorktreeSession::default();
    if bound {
        session.record_task_worktree(&task, "task");
    }
    let server = MockServer::start().await;
    let summary = if citation {
        "Updated src/task.rs."
    } else {
        "Committed the changes."
    };
    let reply = match wire {
        "anthropic" => serde_json::json!({"id":"msg_1","type":"message","role":"assistant",
            "model":"test-model","stop_reason":"end_turn",
            "content":[{"type":"text","text":summary}],"usage":{"input_tokens":10,"output_tokens":5}}),
        "responses" => serde_json::json!({"id":"resp_1","status":"completed",
            "model":"test-model","output":[{"type":"message","id":"msg_1","role":"assistant",
            "status":"completed","content":[{"type":"output_text","text":summary,"annotations":[]}]}]}),
        "openai" => {
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":summary},"finish_reason":"stop"}]})
        }
        _ => serde_json::json!({"message":{"role":"assistant","content":summary},"done":true}),
    };
    let anthropic = wire == "anthropic";
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            // Honor the request without changing any process-global provider
            // mode. Other Anthropic fixtures own their own serial env lane.
            if anthropic && body["stream"] == true {
                crate::agentic::anthropic_loop_tests::sse_text_reply(&[summary], 10, 5)
            } else {
                ResponseTemplate::new(200).set_body_json(reply.clone())
            }
        })
        .mount(&server)
        .await;
    let uri = server.uri();
    let messages = vec![crate::MemMessage::user("Give a final report.")];
    let caveats = crate::Caveats::top();
    let workspace = original.to_str().unwrap();
    let mut context = ctx(&uri, &messages, &caveats);
    context.workspace = workspace;
    context.worktree_session = Some(&session);
    context.action_nudges = false;
    context.kind = match wire {
        "ollama" => crate::BackendKind::Ollama,
        "anthropic" => crate::BackendKind::Anthropic,
        _ => crate::BackendKind::Openai,
    };
    let (text, ..) = if wire == "responses" {
        openai_responses_complete(context, &mut NoMcp).await
    } else {
        chat_complete(context, &mut NoMcp).await
    }
    .unwrap();
    assert!(text.starts_with("## Observed"), "{wire}: {text}");
    if !citation || !bound {
        let root = if bound {
            task.canonicalize().unwrap()
        } else {
            original.clone()
        };
        assert!(
            text.contains(&format!("`{}`", dunce::simplified(&root).display())),
            "{wire}: {text}"
        );
    }
    if citation {
        assert_eq!(text.contains("claim check (#867)"), !bound, "{text}");
    } else {
        assert!(text.contains("HEAD did not move"), "{text}");
        if bound {
            assert!(
                text.contains("`src/task.rs`") && text.contains("`tracked.txt`"),
                "{text}"
            );
            assert!(!text.contains("uncommitted changes: none"), "{text}");
        } else {
            assert!(text.contains("uncommitted changes: none"), "{text}");
        }
    }
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

/// #2787: a task-only citation verifies; without a binding it remains missing.
#[tokio::test]
async fn claimcheck_2787_task_only_citation() {
    for wire in ["ollama", "openai", "responses", "anthropic"] {
        check(false, true, wire).await;
        check(true, true, wire).await;
    }
}

/// #2787: report task dirt and compare its own HEAD, retaining the unbound behavior.
#[tokio::test]
async fn claimcheck_2787_task_git_facts() {
    for wire in ["ollama", "openai", "responses", "anthropic"] {
        check(false, false, wire).await;
        check(true, false, wire).await;
    }
}

/// #2787: mid-turn binding never compares another checkout's HEAD; denied
/// task reads stay unverified, and lift/unlink restore the original root.
#[test]
fn claimcheck_2787_root_changes_and_authority() {
    let (_temp, original, task) = fixture();
    let workspace = original.to_str().unwrap();
    let session = crate::worktree_adoption::WorktreeSession::default();
    let turn = claim_check::TurnClaims::capture(workspace, &crate::Scope::All, Some(&session));
    session.record_task_worktree(&task, "task");
    let text = turn.annotate(
        "Committed src/task.rs.".into(),
        workspace,
        &[],
        &crate::Scope::All,
    );
    assert!(!text.contains("#867"), "{text}");
    assert!(text.contains("no same-checkout baseline"), "{text}");
    assert!(
        text.contains("uncommitted changes: `src/task.rs`, `tracked.txt`"),
        "{text}"
    );
    assert!(
        !text.contains("HEAD moved") && !text.contains("HEAD did not move"),
        "{text}"
    );
    let missing = turn.annotate(
        "Updated src/missing.rs.".into(),
        workspace,
        &[],
        &crate::Scope::All,
    );
    let canonical_task = task.canonicalize().unwrap();
    let task_text = dunce::simplified(&canonical_task).display().to_string();
    assert!(
        missing.contains(&format!("root `{task_text}`")),
        "{missing}"
    );
    #[cfg(feature = "markdown")]
    {
        let rendered = crate::tty::width::strip_ansi(&markdown::render_markdown(
            &missing,
            markdown::RenderOpts {
                color: true,
                cols: 4096,
            },
        ));
        assert!(
            rendered.contains(&format!("root {task_text}")),
            "{rendered}"
        );
    }
    let denied = turn.annotate(
        "Committed src/task.rs.".into(),
        workspace,
        &[],
        &crate::Scope::none(),
    );
    assert!(denied.contains("unverified outside workspace"), "{denied}");
    assert!(
        !denied.contains("#2683") && !denied.contains("not found"),
        "{denied}"
    );
    let bound = claim_check::TurnClaims::capture(workspace, &crate::Scope::All, Some(&session));
    session.lift();
    let lifted = bound.annotate(
        "Committed src/task.rs.".into(),
        workspace,
        &[],
        &crate::Scope::All,
    );
    assert!(
        lifted.contains("#867") && lifted.contains("uncommitted changes: none"),
        "{lifted}"
    );
    assert!(lifted.contains("no same-checkout baseline"), "{lifted}");
    session.record_task_worktree(&task, "task");
    std::fs::remove_file(task.join(".git")).unwrap();
    let unlinked = turn.annotate(
        "Updated src/task.rs.".into(),
        workspace,
        &[],
        &crate::Scope::All,
    );
    assert!(unlinked.contains("#867"), "{unlinked}");
    let missing = turn.annotate(
        "Updated src/missing.rs.".into(),
        workspace,
        &[],
        &crate::Scope::All,
    );
    assert!(
        missing.contains(&format!(
            "root `{}`",
            dunce::simplified(&original).display()
        )),
        "{missing}"
    );
}

/// #2787 round 2: the fixture must honor the Anthropic lane's streaming mode,
/// not mutate it under a different lock and race sibling provider tests.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn claimcheck_2787_anthropic_fixture_preserves_streaming_mode() {
    let _lock = crate::agentic::tools::disable_ocap_tests::env_lock().await;
    let _env = crate::agentic::anthropic_loop_tests::test_env(true);
    let requests = check(false, true, "anthropic").await;
    assert!(!requests.is_empty());
    assert!(
        requests.iter().all(|body| body["stream"] == true),
        "fixture must preserve streaming requests"
    );
}

/// Ground report voice separation on all four actual provider request shapes.
/// Restored pre-fix replies must not speak as the assistant; current facts come
/// only from report state in a labelled harness note, including plain mode.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn observed_report_four_wire_replay_has_separate_harness_voice() {
    let _lock = crate::agentic::tools::disable_ocap_tests::env_lock().await;
    for smart in [false, true] {
        for wire in ["ollama", "openai", "responses", "anthropic"] {
            let (_temp, original, _) = fixture();
            let workspace = original.to_str().unwrap();
            let session = crate::worktree_adoption::WorktreeSession::default();
            let claims =
                claim_check::TurnClaims::capture(workspace, &crate::Scope::All, Some(&session));
            claims.observe_report(
                workspace,
                &crate::Scope::All,
                &observed_report::Capture::default(),
                None,
                Some(&crate::git_staging::Outcome::PrCreated {
                    url: "https://github.com/example/project/pull/7".into(),
                }),
            );
            let first_reply = finalize_final_text(
                "I extracted the helper.".into(),
                workspace,
                &crate::Scope::All,
                &capability_check::Evidence::default(),
                None,
                &claims,
                &self_verify::VerificationLedger::default(),
            );
            assert!(first_reply.starts_with("## Observed\n"));
            let messages = vec![
                crate::MemMessage::system("You are a test assistant."),
                crate::MemMessage::user("refactor"),
                crate::MemMessage::assistant(first_reply),
                crate::MemMessage::user("continue"),
            ];
            let server = MockServer::start().await;
            let reply = match wire {
                "anthropic" => {
                    serde_json::json!({"id":"msg_1","type":"message","role":"assistant", "model":"test-model","stop_reason":"end_turn","content":[{"type":"text","text":"Done."}],"usage":{"input_tokens":10,"output_tokens":5}})
                }
                "responses" => {
                    serde_json::json!({"id":"resp_1","status":"completed","model":"test-model","output":[{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Done.","annotations":[]}]}]})
                }
                "openai" => {
                    serde_json::json!({"choices":[{"message":{"role":"assistant","content":"Done."},"finish_reason":"stop"}]})
                }
                _ => {
                    serde_json::json!({"message":{"role":"assistant","content":"Done."},"done":true})
                }
            };
            Mock::given(wiremock::matchers::method("POST"))
                .respond_with(move |request: &wiremock::Request| {
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    if wire == "anthropic" && body["stream"] == true {
                        crate::agentic::anthropic_loop_tests::sse_text_reply(&["Done."], 10, 5)
                    } else {
                        ResponseTemplate::new(200).set_body_json(reply.clone())
                    }
                })
                .mount(&server)
                .await;
            let uri = server.uri();
            let caveats = crate::Caveats::top();
            let harness = crate::agentic::smart_harness::SmartHarness::new(
                agent_harness::Session::new(crate::test_guard::unbudgeted_session_config())
                    .unwrap(),
                std::sync::Arc::new(|_| Box::pin(async { Ok(("\"answer\"".to_string(), None)) })),
                crate::agentic::smart_harness::AdjudicationSettings::default(),
            )
            .unwrap();
            let mut context = ctx(&uri, &messages, &caveats);
            context.smart_harness = smart.then_some(&harness);
            context.workspace = workspace;
            context.worktree_session = Some(&session);
            context.action_nudges = false;
            context.kind = match wire {
                "ollama" => crate::BackendKind::Ollama,
                "anthropic" => crate::BackendKind::Anthropic,
                _ => crate::BackendKind::Openai,
            };
            let (output, ..) = if wire == "responses" {
                openai_responses_complete(context, &mut NoMcp).await
            } else {
                chat_complete(context, &mut NoMcp).await
            }
            .unwrap();
            assert!(output.starts_with("## Observed\n"), "{wire}: {output}");
            let requests = server.received_requests().await.unwrap();
            assert!(!requests.is_empty());
            for request in requests {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let messages = body[if wire == "responses" {
                    "input"
                } else {
                    "messages"
                }]
                .as_array()
                .unwrap();
                let mut found = ["system", "instructions"].iter().any(|field| {
                    let text = body[field].to_string();
                    text.contains("[Harness observed facts")
                        && text.contains("https://github.com/example/project/pull/7")
                });
                for message in messages {
                    let text = message.to_string();
                    if message["role"] == "assistant" {
                        assert!(
                            !text.contains("## Observed")
                                && !text.contains("https://github.com/example/project/pull/7"),
                            "{wire}: {text}"
                        );
                    }
                    if text.contains("https://github.com/example/project/pull/7") {
                        assert_eq!(message["role"], "system", "{wire}: {text}");
                        assert!(
                            text.contains("[Harness observed facts")
                                && text.contains("not assistant-authored"),
                            "{wire}: {text}"
                        );
                        found = true;
                    }
                }
                assert!(found, "{wire}: missing harness facts: {body}");
            }
        }
    }
}

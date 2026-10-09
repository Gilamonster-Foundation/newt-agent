//! #2831: ground the advisory predicate in real Git/check execution and ChatCtx replay.
use super::claimcheck_task_root_tests::git;
use super::*;
use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};

#[tokio::test]
async fn publish_early_reaches_provider_after_check_and_commit() {
    let _settings = default_loop_settings();
    let _lock = env_lock().await;
    let _ambient = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let _verify = EnvVar::set("NEWT_SELF_VERIFY", "0");
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("templates")).unwrap();
    std::fs::write(temp.path().join("empty-config"), "").unwrap();
    let _global = EnvVar::set(
        "GIT_CONFIG_GLOBAL",
        temp.path().join("empty-config").to_str().unwrap(),
    );
    let _system = EnvVar::set("GIT_CONFIG_NOSYSTEM", "1");
    let _count = EnvVar::set("GIT_CONFIG_COUNT", "0");
    let _parameters = EnvVar::set("GIT_CONFIG_PARAMETERS", "");
    let original = temp.path().join("original");
    let task = temp.path().join("task");
    std::fs::create_dir(&original).unwrap();
    git(&original, temp.path(), &["init", "-q", "-b", "main"]);
    git(
        &original,
        temp.path(),
        &["config", "core.autocrlf", "false"],
    );
    git(
        &original,
        temp.path(),
        &["commit", "--allow-empty", "-qm", "base"],
    );
    let caveats = Caveats::top();
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
    let session = crate::worktree_adoption::WorktreeSession::default();
    session.record_task_worktree(&task, "task");

    std::fs::create_dir(task.join("src")).unwrap();
    std::fs::write(task.join("src/lib.rs"), "pub fn extracted() {}\n").unwrap();
    std::fs::write(
        task.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(task.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    git(&task, temp.path(), &["add", "."]);
    let commands = ["cargo check --offline", "git -c core.hooksPath= -c commit.gpgsign=false -c user.name=Fixture -c user.email=fixture@example.invalid commit -qm extraction", "git status --short"];
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let task_path = task.clone();
    let admin = original.join(".git/worktrees/task");
    let git_env = serde_json::json!({
        "RUSTC_WRAPPER":"", "CARGO_BUILD_JOBS":"1", "CARGO_TARGET_DIR":task.join("target"),
        "GIT_DIR":admin, "GIT_WORK_TREE":task, "GIT_COMMON_DIR":original.join(".git"),
        "GIT_INDEX_FILE":admin.join("index"), "GIT_OBJECT_DIRECTORY":original.join(".git/objects"),
        "GIT_ALTERNATE_OBJECT_DIRECTORIES":"", "GIT_NAMESPACE":"", "GIT_CONFIG_COUNT":"0",
        "GIT_CONFIG_PARAMETERS":"", "GIT_CONFIG_NOSYSTEM":"1",
        "GIT_CONFIG_GLOBAL":temp.path().join("empty-config"),
        "GIT_AUTHOR_NAME":"Fixture", "GIT_AUTHOR_EMAIL":"fixture@example.invalid",
        "GIT_COMMITTER_NAME":"Fixture", "GIT_COMMITTER_EMAIL":"fixture@example.invalid"
    });
    Mock::given(method("POST")).respond_with(move |_: &Request| {
        let index = calls.fetch_add(1, Ordering::SeqCst);
        let message = if let Some(command) = commands.get(index) {
            let args = serde_json::json!({"command":command,"cwd":task_path,"env":git_env});
            serde_json::json!({"role":"assistant","content":"","tool_calls":[{"id":format!("call_{index}"),"type":"function","function":{"name":"run_command","arguments":args.to_string()}}]})
        } else { serde_json::json!({"role":"assistant","content":"Stopped."}) };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices":[{"message":message,"finish_reason":if index<3 {"tool_calls"} else {"stop"}}]}))
    }).mount(&server).await;
    let (uri, messages) = (server.uri(), msgs());
    let mut context = ctx(&uri, &messages, &caveats);
    context.kind = BackendKind::Openai;
    context.workspace = original.to_str().unwrap();
    context.task = "Refactor in a worktree and open a PR. Stop after the three requested commands.";
    context.worktree_session = Some(&session);
    context.max_tool_rounds = 4;
    chat_complete(context, &mut NoMcp).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert!(!bodies[1].to_string().contains("Publish early:"));
    let after_commit = bodies[2]["messages"].as_array().unwrap();
    assert!(
        after_commit.iter().any(|m| m["role"] == "tool"
            && m["content"]
                .as_str()
                .is_some_and(|s| s.contains("Publish early:"))),
        "{}",
        bodies[2]
    );
    assert_eq!(
        bodies[3]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("Publish early:")))
            .count(),
        1
    );
}

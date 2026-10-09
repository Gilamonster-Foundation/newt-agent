//! Grounds optional smart-harness startup in the real CLI, without an auxiliary.
use super::*;

/// Parent-directory TUI launch: bare command grants must degrade with notice.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn smart_parent_path_interactive_startup_continues() {
    startup_continues(true).await;
}

/// Headless has ambient exec; its explicit mutable read anchor must degrade too.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn smart_mutable_anchor_refusal_disables_feature_not_process() {
    startup_continues(false).await;
}

#[cfg(unix)]
async fn startup_continues(interactive: bool) {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureThenFinish {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    let mut command = common::newt();
    let home = command.home().to_path_buf();
    let workspace = home.join("workspace");
    let bin = workspace.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let frame = home.join("private-frame");
    let config = home.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"default_backend = "fixture"
[[backends]]
name = "fixture"
endpoint = "{}"
model = "fixture"
kind = "openai"
api = "chat_completions"
[smart_harness]
enabled = true
device = "cuda"
frame_dir = {}
[tui.permissions]
preset = "workspace_dev"
net = []
prompt = false
"#,
            server.uri(),
            toml::Value::String(frame.to_string_lossy().into_owned())
        ),
    )
    .unwrap();
    let instruction = workspace.join("task.md");
    std::fs::write(&instruction, "Finish without tools.").unwrap();
    command
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .arg("--config")
        .arg(&config);
    if interactive {
        command
            .current_dir(&workspace)
            .arg("--config-dir")
            .arg(home.join(".newt"))
            .args([
                "--no-splash",
                "--plain",
                "--ephemeral",
                "--no-agents-file",
                "--no-prompt-for-permissions",
            ])
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("NEWT_NO_MODEL_PULL", "1")
            .env("NEWT_NUDGE", "off")
            .write_stdin("Finish without tools.\n/quit\n");
    } else {
        command
            .env("NEWT_READ_PATHS", &bin)
            .args(["headless", "--cwd"])
            .arg(&workspace)
            .arg("--instruction-file")
            .arg(&instruction)
            .arg("--frame-dir")
            .arg(&frame)
            .args(["--max-rounds", "1", "--context-window", "32768"]);
    }
    let output = command.assert().success().get_output().clone();
    let stderr = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stderr.contains("smart harness disabled for this session"),
        "{stderr}"
    );
    assert!(
        stderr.contains(bin.to_str().unwrap()),
        "missing offending path: {stderr}"
    );
    assert!(stderr.contains("restart"), "missing remedy: {stderr}");
    assert!(
        !frame.exists(),
        "disabled feature must not open private frames"
    );
    assert!(
        !requests.lock().unwrap().is_empty(),
        "primary must still run"
    );
}

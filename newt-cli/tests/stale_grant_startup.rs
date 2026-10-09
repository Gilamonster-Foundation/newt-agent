//! Real startup grounds the permission filter against signed on-disk approvals.
mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

async fn startup(grant: impl FnOnce(&Path, &Path) -> std::path::PathBuf) -> (String, String) {
    let home = common::isolated_root();
    let root = home.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let config_dir = root.join(".newt");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&config_dir).unwrap();
    let peer = common::inference_peer("llama3.1:8b").await;
    let config = config_dir.join("config.toml");
    std::fs::write(
        &config,
        "[tui.permissions]\npreset = 'workspace_edit'\nprompt = false\n",
    )
    .unwrap();
    let path = grant(&workspace, &root);
    let key = newt_identity::load_or_generate(&config_dir.join("identity.pem")).unwrap();
    newt_core::ocap_store::persist_approve(
        &config,
        newt_core::ocap_store::ApproveEntry::Fs {
            path: path.to_str().unwrap().into(),
            write: true,
        },
        |_, _| false,
        |bytes| key.sign(bytes).to_bytes(),
    )
    .unwrap();
    let store = config_dir.join("ocap/approve.toml");
    let before = std::fs::read(&store).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
    common::isolate(&mut cmd, &root);
    cmd.current_dir(&workspace)
        .args(["--no-splash", "--plain", "--ephemeral", "--no-agents-file"])
        .arg("--config")
        .arg(&config)
        .arg("--config-dir")
        .arg(&config_dir)
        .env("OLLAMA_HOST", peer.uri())
        .env("NEWT_NO_MODEL_PULL", "1")
        .env("TERM", "dumb")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/byline\n/quit\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let transcript = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{transcript}");
    assert!(
        transcript.contains(common::GIT_FIXTURE_NAME),
        "startup must reach operator commands: {transcript}"
    );
    assert_eq!(
        std::fs::read(store).unwrap(),
        before,
        "startup must not rewrite signed approvals"
    );
    (transcript, path.to_str().unwrap().into())
}

/// A deleted signed child used to become a redundant root and abort startup.
#[tokio::test]
async fn dead_covered_grant_does_not_abort_startup() {
    let (text, _) = startup(|workspace, _| workspace.join("deleted/plan.md")).await;
    assert!(
        !text.contains("dropped durable grant"),
        "already covered: {text}"
    );
}

/// Missing outside names must still reach validation, never become covered.
#[tokio::test]
async fn dead_uncovered_grant_keeps_ordinary_validation() {
    let (text, _) = startup(|_, root| root.join("outside/deleted.md")).await;
    assert!(
        !text.contains("dropped durable grant"),
        "stable external anchor: {text}"
    );
}

/// A recalled alias with a writable anchor used to abort the whole session.
#[cfg(unix)]
#[tokio::test]
async fn unsafe_recalled_grant_is_dropped_with_notice() {
    let (text, path) = startup(|workspace, root| {
        let external = root.join("outside");
        std::fs::create_dir(&external).unwrap();
        let alias = workspace.join("alias");
        std::os::unix::fs::symlink(external, &alias).unwrap();
        alias.join("deleted.md")
    })
    .await;
    assert!(text.contains("dropped durable grant"), "{text}");
    assert!(text.contains(&format!("{path:?}")), "{text}");
    assert!(text.contains("~/.newt/ocap/approve.toml"), "{text}");
    assert!(
        text.contains("run `newt doctor` to inspect and repair"),
        "{text}"
    );
}

//! Real signed-store and PTY proof for doctor's report-only/confirmed repair boundary.
mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

struct Fixture {
    _home: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    config: PathBuf,
    dead: PathBuf,
    store: PathBuf,
    before: Vec<u8>,
    key: newt_identity::UserKey,
}

impl Fixture {
    fn new() -> Self {
        let home = common::isolated_root();
        let root = home.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let config = root.join(".newt/config.toml");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "[tui.permissions]\npreset = 'workspace_edit'\n").unwrap();
        let key = newt_identity::load_or_generate(&root.join(".newt/identity.pem")).unwrap();
        let dead = workspace.join("deleted.md");
        let live = workspace.join("live.md");
        std::fs::write(&live, "keep").unwrap();
        for path in [&dead, &live] {
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
        }
        let store = root.join(".newt/ocap/approve.toml");
        let before = std::fs::read(&store).unwrap();
        Self {
            _home: home,
            root,
            workspace,
            config,
            dead,
            store,
            before,
            key,
        }
    }

    fn command(&self, peer: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
        common::isolate(&mut cmd, &self.root);
        cmd.current_dir(&self.workspace)
            .arg("--config")
            .arg(&self.config)
            .arg("--config-dir")
            .arg(self.config.parent().unwrap())
            .arg("doctor")
            .env("OLLAMA_HOST", peer)
            .env("TERM", "xterm-256color")
            .kill_on_drop(true);
        cmd
    }
}

/// Before the repair, doctor never reported a signed approval's missing target.
#[tokio::test]
async fn doctor_reports_dead_grants_and_never_fixes_without_a_terminal() {
    let f = Fixture::new();
    let peer = common::inference_peer("llama3.1:8b").await;
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        f.command(&peer.uri())
            .arg("--fix")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("Durable grants:"), "{text}");
    assert!(
        text.contains(&format!("{:?}", f.dead.to_str().unwrap())),
        "{text}"
    );
    assert!(text.contains("target does not exist"), "{text}");
    assert!(text.contains("--fix needs a terminal"), "{text}");
    assert_eq!(std::fs::read(&f.store).unwrap(), f.before);
    let (verified, warnings) =
        newt_core::ocap_store::load_store(&f.config, Some(f.key.public().as_bytes()));
    assert!(warnings.is_empty());
    assert_eq!(
        verified.files[&newt_core::ocap_store::Verdict::Approve]
            .fs
            .len(),
        2
    );
    assert_eq!(
        std::fs::read_dir(f.store.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("backup-"))
            .count(),
        0
    );
}

/// Real confirmation must precede backup and mutation; remaining signatures survive.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_confirmed_prune_backs_up_and_preserves_signatures() {
    let mut f = Fixture::new();
    let later = f.workspace.join("later-deleted.md");
    newt_core::ocap_store::persist_approve(
        &f.config,
        newt_core::ocap_store::ApproveEntry::Fs {
            path: later.to_str().unwrap().into(),
            write: true,
        },
        |_, _| false,
        |bytes| f.key.sign(bytes).to_bytes(),
    )
    .unwrap();
    f.before = std::fs::read(&f.store).unwrap();
    let peer = common::inference_peer("llama3.1:8b").await;
    let pty = tests_pty::Pty::open_with_cursor_reply(1, 1);
    let mut cmd = f.command(&peer.uri());
    cmd.arg("--fix")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(pty.slave_stdio());
    let child = cmd.spawn().unwrap();
    drop(cmd);
    assert!(
        pty.wait_for_screen("Prune stale durable grant", Duration::from_secs(45)),
        "{}",
        pty.screen()
    );
    assert_eq!(
        std::fs::read(&f.store).unwrap(),
        f.before,
        "no mutation before approval"
    );
    pty.type_in("n\n");
    assert!(
        pty.wait_for_screen("later-deleted.md", Duration::from_secs(45)),
        "{}",
        pty.screen()
    );
    assert_eq!(std::fs::read(&f.store).unwrap(), f.before);
    pty.type_in("y\n");
    let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let text = pty.screen_to_eof();
    assert!(output.status.success(), "{text}");
    let backups: Vec<_> = std::fs::read_dir(f.store.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains("backup-"))
        .collect();
    assert_eq!(backups.len(), 1, "{text}");
    assert_eq!(std::fs::read(backups[0].path()).unwrap(), f.before);
    let (verified, warnings) =
        newt_core::ocap_store::load_store(&f.config, Some(f.key.public().as_bytes()));
    assert!(warnings.is_empty(), "{warnings:?}");
    let entries = &verified.files[&newt_core::ocap_store::Verdict::Approve].fs;
    assert_eq!(entries.len(), 2, "{text}");
    assert!(
        entries
            .iter()
            .any(|entry| entry.path == f.dead.to_str().unwrap()),
        "declined entry must survive"
    );
    assert!(!entries
        .iter()
        .any(|entry| entry.path == later.to_str().unwrap()));
    let original =
        newt_core::ocap_store::PolicyFile::parse(std::str::from_utf8(&f.before).unwrap()).unwrap();
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.path.ends_with("live.md"))
            .unwrap()
            .sig,
        original
            .fs
            .iter()
            .find(|e| e.path.ends_with("live.md"))
            .unwrap()
            .sig
    );
}

/// Doctor must explain both signed-name drift and the same anchor refusal as startup.
#[cfg(unix)]
#[tokio::test]
async fn doctor_reports_missing_exec_and_unsafe_signed_alias() {
    let f = Fixture::new();
    let peer = common::inference_peer("llama3.1:8b").await;
    let outside = f.root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    let alias = f.workspace.join("alias");
    std::os::unix::fs::symlink(outside, &alias).unwrap();
    let missing = f.root.join("missing-program");
    for entry in [
        newt_core::ocap_store::ApproveEntry::Fs {
            path: alias.to_str().unwrap().into(),
            write: true,
        },
        newt_core::ocap_store::ApproveEntry::Exec {
            target: missing.to_str().unwrap().into(),
        },
    ] {
        newt_core::ocap_store::persist_approve(
            &f.config,
            entry,
            |_, _| false,
            |bytes| f.key.sign(bytes).to_bytes(),
        )
        .unwrap();
    }
    let before = std::fs::read(&f.store).unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        f.command(&peer.uri()).stdin(Stdio::null()).output(),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains(missing.to_str().unwrap()), "{text}");
    assert!(
        text.contains("target no longer resolves to its signed name"),
        "{text}"
    );
    assert!(
        text.contains("workspace-protection filesystem grant anchor"),
        "{text}"
    );
    assert_eq!(std::fs::read(&f.store).unwrap(), before);
}

use super::*;
use crate::agentic::{mcp::NoMcp, smart_harness::SmartHarness};
use crate::caveats::Caveats;
use std::{path::Path, sync::Arc};

fn harness(directory: &Path) -> (SmartHarness, content_addressable::ContentId) {
    let mut session = agent_harness::Session::open(directory, Default::default()).unwrap();
    let source = session
        .retain_tool_output("fixture", b"retained original source")
        .unwrap();
    (
        SmartHarness::new(
            session,
            Arc::new(|_| panic!("filesystem isolation never needs inference")),
            Default::default(),
        )
        .unwrap(),
        source,
    )
}

async fn dispatch(
    harness: &SmartHarness,
    workspace: &Path,
    caveats: &Caveats,
    name: &str,
    args: serde_json::Value,
    permission_gate: Option<&mut dyn PermissionGate>,
) -> String {
    let batch = harness.fixture_tool_batch(name, args.clone());
    let invocation = batch.start(0, None).unwrap();
    let mut display = super::super::display::ToolDisplay::new(Vec::new(), false, 80, 20, false);
    execute_tool_with_display_cancellable(
        &mut display,
        name,
        &args,
        workspace.to_str().unwrap(),
        false,
        20,
        caveats,
        &mut NoMcp,
        ToolCollaborators {
            invocation: Some(&invocation),
            permission_gate,
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap()
}

/// Grounds the smart-session authority gate in actual file operations: a
/// workspace grant must never expose a frame that lies beneath that grant.
#[tokio::test]
#[serial_test::serial]
async fn exposed_frame_refuses_generic_tool_dispatch() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", "0");
    let workspace = tempfile::tempdir().unwrap();
    let directory = workspace.path().join(".newt/frame");
    let (harness, _) = harness(&directory);
    let marker = directory.join("marker");
    std::fs::write(&marker, "frame contents").unwrap();
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    for name in ["read_file", "write_file", "list_dir", "delete_file"] {
        let target = if name == "list_dir" {
            &directory
        } else {
            &marker
        };
        let output = dispatch(
            &harness,
            workspace.path(),
            &caveats,
            name,
            serde_json::json!({"path":target,"content":"changed"}),
            None,
        )
        .await;
        assert!(output.contains("frame isolation"), "{name}: {output}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "frame contents");
    }
}

struct WidenAll;
impl PermissionGate for WidenAll {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        PermissionDecision::Allow(Caveats::top())
    }
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        HumanQuestionOutcome::Answer("operator answer".into())
    }
}

/// Grounds the mocked permission decision against a private payload file:
/// re-minting authority during a turn cannot bypass the frame read boundary.
#[tokio::test]
#[serial_test::serial]
async fn permission_widening_cannot_expose_private_frame() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", "0");
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (harness, _) = harness(directory.path());
    let marker = directory.path().join("marker");
    std::fs::write(&marker, "private generated material").unwrap();
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    let output = dispatch(
        &harness,
        workspace.path(),
        &caveats,
        "read_file",
        serde_json::json!({"path":marker}),
        Some(&mut WidenAll),
    )
    .await;
    assert!(!output.contains("private generated material"), "{output}");
    assert!(
        output.contains("denied") || output.contains("frame isolation"),
        "{output}"
    );
}

/// Grounds ordinary filesystem denials and mediated retrieval against the
/// same durable source, without granting the primary model raw frame access.
#[tokio::test]
#[serial_test::serial]
async fn private_frame_denies_raw_tools_and_preserves_mediated_reads() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", "0");
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (harness, source) = harness(directory.path());
    let marker = directory.path().join("marker");
    std::fs::write(&marker, "private generated material").unwrap();
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    if crate::agentic::smart_harness::validate_isolation_runtime().is_err() {
        let output = dispatch(
            &harness,
            workspace.path(),
            &caveats,
            "re_read",
            serde_json::json!({"cid":source.to_string(),"max_bytes":16}),
            None,
        )
        .await;
        assert!(output.contains("frame isolation"), "{output}");
        return;
    }
    for name in ["read_file", "write_file", "list_dir", "delete_file"] {
        let target = if name == "list_dir" {
            directory.path()
        } else {
            &marker
        };
        let output = dispatch(
            &harness,
            workspace.path(),
            &caveats,
            name,
            serde_json::json!({"path":target,"content":"changed"}),
            None,
        )
        .await;
        assert!(output.contains("denied"), "{name}: {output}");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "private generated material"
        );
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        std::os::unix::fs::symlink(directory.path(), workspace.path().join("private-link"))
            .unwrap();
        for name in ["read_file", "write_file", "list_dir", "delete_file"] {
            let path = if name == "list_dir" {
                "private-link"
            } else {
                "private-link/marker"
            };
            let output = dispatch(
                &harness,
                workspace.path(),
                &caveats,
                name,
                serde_json::json!({"path":path,"content":"changed"}),
                None,
            )
            .await;
            assert!(output.contains("denied"), "symlink {name}: {output}");
            assert_eq!(
                std::fs::read_to_string(&marker).unwrap(),
                "private generated material"
            );
        }
    }
    let output = dispatch(
        &harness,
        workspace.path(),
        &caveats,
        "re_read",
        serde_json::json!({"cid":source.to_string(),"max_bytes":16}),
        None,
    )
    .await;
    let receipt: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(receipt["text"], "retained origina");
    assert_eq!(receipt["complete"], false);
}

/// Grounds the dispatch scope with a real shell child: the existing kernel
/// fence must deny raw frame reads and writes while allowing workspace work.
/// The Git control explicitly grants GIT_CONFIG_GLOBAL/GIT_CONFIG_SYSTEM=/dev/null
/// and omits HOME through the existing shell environment seam; ambient Git config
/// and ignore files remain unreadable.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
#[serial_test::serial]
async fn real_shell_cannot_read_or_mutate_private_frame() {
    use crate::caveats::Scope;

    let _env = super::disable_ocap_tests::env_lock().await;
    let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", "0");
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "host");
    // Git's ambient config is outside the grant. Use the existing explicit
    // shell-env seam to select empty config, as hardened_git does for setup.
    let _env_grant = super::disable_ocap_tests::EnvVar::set(
        "NEWT_SHELL_ENV_PASSTHROUGH",
        "GIT_CONFIG_GLOBAL:GIT_CONFIG_SYSTEM",
    );
    let _global = super::disable_ocap_tests::EnvVar::set("GIT_CONFIG_GLOBAL", "/dev/null");
    let _system = super::disable_ocap_tests::EnvVar::set("GIT_CONFIG_SYSTEM", "/dev/null");
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (harness, _) = harness(directory.path());
    let marker = directory.path().join("marker");
    std::fs::write(&marker, "private generated material").unwrap();
    let mut caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    caveats.exec = Scope::All;
    // The host-shell contract requires open network authority; this proof
    // exercises its independent filesystem boundary and performs no network IO.
    caveats.net = Scope::All;
    let command = format!(
        "printf allowed > inside.txt; cat '{}'; printf changed > '{}'; ln '{}' hardlink.txt",
        marker.display(),
        marker.display(),
        marker.display()
    );
    let output = dispatch(
        &harness,
        workspace.path(),
        &caveats,
        "run_command",
        serde_json::json!({"command":command}),
        None,
    )
    .await;
    assert!(!output.contains("private generated material"), "{output}");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "private generated material"
    );
    assert!(!workspace.path().join("hardlink.txt").exists());
    if crate::ocap_l3_backend().1 {
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("inside.txt"))
                .unwrap_or_else(|error| panic!("workspace control failed ({error}): {output}")),
            "allowed"
        );
        let found = dispatch(
            &harness,
            workspace.path(),
            &caveats,
            "run_command",
            serde_json::json!({"command":"find . -name inside.txt"}),
            None,
        )
        .await;
        assert!(
            found.contains("./inside.txt"),
            "confined shell find must run: {found}"
        );
        let initialized = crate::git_hardening::hardened_git(workspace.path(), &["init", "-q"])
            .unwrap()
            .output()
            .unwrap();
        assert!(
            initialized.status.success(),
            "fixture repository initialization failed: {}",
            String::from_utf8_lossy(&initialized.stderr)
        );
        std::fs::write(workspace.path().join("visible.txt"), "visible").unwrap();
        let status = dispatch(
            &harness,
            workspace.path(),
            &caveats,
            "run_command",
            serde_json::json!({"command":"git status --short"}),
            None,
        )
        .await;
        assert!(
            status.contains("?? visible.txt"),
            "scoped shell Git must run: {status}"
        );
        let staged = dispatch(
            &harness,
            workspace.path(),
            &caveats,
            "run_command",
            serde_json::json!({"command":"git add visible.txt"}),
            None,
        )
        .await;
        assert!(!staged.starts_with("error:"), "{staged}");
        let index = crate::git_hardening::hardened_git(workspace.path(), &["ls-files", "--cached"])
            .unwrap()
            .output()
            .unwrap();
        assert!(index.status.success());
        assert_eq!(String::from_utf8(index.stdout).unwrap(), "visible.txt\n");
        let commit = dispatch(
            &harness,
            workspace.path(),
            &caveats,
            "run_command",
            serde_json::json!({"command":"git commit -m fixture"}),
            None,
        )
        .await;
        assert!(
            commit.contains("harness-managed commit attribution"),
            "{commit}"
        );
    } else {
        assert!(output.contains("frame isolation"), "{output}");
    }
}

struct UnconfinedDelegate(std::path::PathBuf);

/// Grounds the find-adapter refusal in its real descriptor-discard boundary:
/// replacing the validated path makes the legacy walker disclose private names.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
#[serial_test::serial]
async fn smart_find_refuses_the_unbound_recursive_walker() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", "0");
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (harness, _) = harness(directory.path());
    std::fs::write(directory.path().join("private-frame-record"), "private").unwrap();
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    let root = workspace.path().join("search");
    std::fs::create_dir(&root).unwrap();
    assert!(find_root_contained(
        &caveats.fs_read,
        workspace.path().to_str().unwrap(),
        &root,
        root.to_str().unwrap(),
    ));
    std::fs::remove_dir(&root).unwrap();
    std::os::unix::fs::symlink(directory.path(), &root).unwrap();
    let args = serde_json::json!({});
    let opts = find_opts_from_args(&args);
    let (hits, _) = find_walk(&root, workspace.path(), &opts, None, |_| {}).unwrap();
    assert!(hits.iter().any(|hit| hit.contains("private-frame-record")));
    let output = dispatch(
        &harness,
        workspace.path(),
        &caveats,
        "find",
        serde_json::json!({"path":"."}),
        None,
    )
    .await;
    assert!(output.contains("Error: frame isolation:"), "{output}");
}

impl super::super::git_tool::GitTool for UnconfinedDelegate {
    fn dispatch(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &crate::git_caveats::GitCaveats,
        _: &Caveats,
    ) -> Result<String, String> {
        Ok(std::fs::read_to_string(&self.0).unwrap())
    }
}

#[async_trait::async_trait]
impl super::super::crew_tool::CrewRunner for UnconfinedDelegate {
    async fn dispatch(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &Caveats,
    ) -> Result<String, String> {
        Ok(std::fs::read_to_string(&self.0).unwrap())
    }
}

/// Grounds delegate refusals against a real private file. The injected
/// delegate models native Git's unrestricted add and crew's std::fs access.
#[tokio::test]
#[serial_test::serial]
async fn smart_delegates_cannot_read_private_payloads() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", "0");
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (harness, _) = harness(directory.path());
    let marker = directory.path().join("marker");
    std::fs::write(&marker, "private generated material").unwrap();
    let delegate = UnconfinedDelegate(marker.clone());
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    for (name, args) in [
        ("git", serde_json::json!({"op":"add","paths":[marker]})),
        ("crew", serde_json::json!({"task":"read private frame"})),
    ] {
        let batch = harness.fixture_tool_batch(name, args.clone());
        let invocation = batch.start(0, None).unwrap();
        let mut display = super::super::display::ToolDisplay::new(Vec::new(), false, 80, 20, false);
        let output = execute_tool_with_display_cancellable(
            &mut display,
            name,
            &args,
            workspace.path().to_str().unwrap(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            ToolCollaborators {
                invocation: Some(&invocation),
                git_tool: Some(&delegate),
                crew_runner: Some(&delegate),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            !output.contains("private generated material"),
            "{name}: {output}"
        );
        assert!(
            output.contains("scoped fs_read") || output.contains("frame isolation"),
            "{name}: {output}"
        );
    }
}

/// Grounds launch-authority refusal at the tool entry point, including
/// embedders that create a Session directly instead of using frontend config.
#[tokio::test]
#[serial_test::serial]
async fn smart_tools_refuse_unconfined_launch_authority() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (harness, _) = harness(directory.path());
    std::fs::write(workspace.path().join("inside.txt"), "allowed content").unwrap();
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    for (yolo, full) in [("1", "0"), ("0", "1")] {
        let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", yolo);
        let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", full);
        let output = dispatch(
            &harness,
            workspace.path(),
            &caveats,
            "read_file",
            serde_json::json!({"path":"inside.txt"}),
            None,
        )
        .await;
        assert!(output.contains("frame isolation"), "{output}");
        assert!(!output.contains("allowed content"), "{output}");
    }
}

/// A native positive control prevents a macOS run from passing only because
/// every dispatch was refused before any file operation.
#[cfg(target_os = "macos")]
#[tokio::test]
#[serial_test::serial]
async fn macos_smart_runtime_admits_confined_seatbelt() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", "0");
    assert!(
        agent_bridle::seatbelt_is_supported(),
        "native suite requires Seatbelt"
    );
    crate::agentic::smart_harness::validate_isolation_runtime().unwrap();
}

/// The shrink guard is a read too: a symlink into private storage must neither
/// leak its line count nor let edit_file return a private content preview.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
#[serial_test::serial]
async fn private_frame_cannot_leak_through_write_shrink_or_edit_preview() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", "0");
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (harness, _) = harness(directory.path());
    let marker = directory.path().join("marker");
    let private = "private generated material\n".repeat(100);
    std::fs::write(&marker, &private).unwrap();
    std::os::unix::fs::symlink(&marker, workspace.path().join("link")).unwrap();
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    for (name, args) in [
        (
            "write_file",
            serde_json::json!({"path":"link","content":"changed"}),
        ),
        (
            "edit_file",
            serde_json::json!({"path":"link","old_string":"absent","new_string":"changed"}),
        ),
    ] {
        let output = dispatch(&harness, workspace.path(), &caveats, name, args, None).await;
        assert!(
            output.contains("denied") || output.contains("frame isolation"),
            "{output}"
        );
        assert!(!output.contains("100"), "shrink oracle: {output}");
        assert!(
            !output.contains("private generated material"),
            "preview leak: {output}"
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), private);
    }
}

/// Ground worktree support in real Git administrative files and confined file
/// edits. An external worktree is usable only with its own explicit grant;
/// its parent directory and private frame remain outside that grant.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
#[serial_test::serial]
async fn smart_shell_worktrees_support_scoped_creation_editing_and_staging() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _yolo = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _full = super::disable_ocap_tests::EnvVar::set("NEWT_FULL_ACCESS", "0");
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "host");
    let _env_grant = super::disable_ocap_tests::EnvVar::set(
        "NEWT_SHELL_ENV_PASSTHROUGH",
        "GIT_CONFIG_GLOBAL:GIT_CONFIG_SYSTEM",
    );
    let _global = super::disable_ocap_tests::EnvVar::set("GIT_CONFIG_GLOBAL", "/dev/null");
    let _system = super::disable_ocap_tests::EnvVar::set("GIT_CONFIG_SYSTEM", "/dev/null");
    let workspace = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let external_parent = tempfile::tempdir().unwrap();
    let external = external_parent.path().join("task-worktree");
    std::fs::create_dir(&external).unwrap();
    let sibling = external_parent.path().join("ungranted");
    std::fs::write(&sibling, "outside authority").unwrap();
    let (harness, _) = harness(private.path());
    let git_fixture = |args: &[&str]| {
        let output = crate::git_hardening::hardened_git(workspace.path(), args)
            .unwrap()
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git_fixture(&["init", "-q"]);
    std::fs::write(workspace.path().join("task.txt"), "baseline\n").unwrap();
    std::fs::write(workspace.path().join(".gitignore"), ".worktrees/\n").unwrap();
    git_fixture(&["add", "task.txt", ".gitignore"]);
    git_fixture(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "fixture baseline",
    ]);
    let mut caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    caveats.net = crate::caveats::Scope::All;
    for (index, destination) in [workspace.path().join(".worktrees/task"), external.clone()]
        .into_iter()
        .enumerate()
    {
        if index == 1 {
            // The external directory alone is delegated, never its parent.
            caveats.fs_read = crate::caveats::Scope::only([
                workspace.path().to_string_lossy().into_owned(),
                external.to_string_lossy().into_owned(),
            ]);
            caveats.fs_write = caveats.fs_read.clone();
        }
        let create = dispatch(&harness, workspace.path(), &caveats, "run_command",
            serde_json::json!({"command":format!("git worktree add -b fixture-{index} '{}'", destination.display())}), None).await;
        assert!(destination.join("task.txt").exists(), "{create}");
        let edited = dispatch(&harness, workspace.path(), &caveats, "edit_file",
            serde_json::json!({"path":destination.join("task.txt"), "old_string":"baseline", "new_string":"worktree change"}), None).await;
        assert_eq!(
            std::fs::read_to_string(destination.join("task.txt")).unwrap(),
            "worktree change\n",
            "{edited}"
        );
        let staged = dispatch(&harness, workspace.path(), &caveats, "run_command",
            serde_json::json!({"command":"git add task.txt && git status --short", "cwd":destination}), None).await;
        assert!(staged.contains("M  task.txt"), "{staged}");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("task.txt")).unwrap(),
            "baseline\n"
        );
    }
    let denied = dispatch(
        &harness,
        workspace.path(),
        &caveats,
        "read_file",
        serde_json::json!({"path":sibling}),
        None,
    )
    .await;
    assert!(denied.contains("denied"), "{denied}");
    assert!(!denied.contains("outside authority"));
}

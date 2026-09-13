//! Real-resource witnesses for the pure file-change version-to-patch contract:
//! actual files, confirmation timing, mutation outcomes, and the production
//! terminal dispatcher establish that observations and safe display agree.

use super::*;

#[cfg(unix)]
#[tokio::test]
async fn nonregular_file_errors_escape_path_controls_in_the_production_display() {
    let ws = tempfile::TempDir::new().unwrap();
    let path = "directory-\u{1b}[31m\tname";
    std::fs::create_dir(ws.path().join(path)).unwrap();
    for (name, args) in [
        (
            "write_file",
            serde_json::json!({"path": path, "content": "new\n"}),
        ),
        (
            "edit_file",
            serde_json::json!({"path": path, "old_string": "old", "new_string": "new"}),
        ),
    ] {
        let mut display = ToolDisplay::new(Vec::new(), false, 4096, 0, false);
        let raw = execute_tool_with_display_cancellable(
            &mut display,
            name,
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats_rw(ws.path()),
            &mut NoMcp,
            ToolCollaborators::default(),
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(raw.starts_with("error:"), "{raw:?}");
        assert!(raw.contains("nonregular target"), "{raw:?}");
        assert!(raw.contains(path), "raw result retains the actual path");
        let visible = String::from_utf8(display.into_inner()).unwrap();
        assert!(
            visible.chars().all(|ch| !ch.is_control() || ch == '\n'),
            "{name} early error leaked pathname controls: {visible:?}"
        );
        assert!(visible.contains("directory-<U+001B>[31m<U+0009>name"));
        assert!(visible.contains("display escapes control characters"));
        assert!(ws.path().join(path).is_dir());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn pathname_controls_are_safe_in_file_headers_and_result_prefixes() {
    let ws = tempfile::TempDir::new().unwrap();
    let path = "file-\u{1b}[31m\tname.rs";
    for (name, args) in [
        (
            "write_file",
            serde_json::json!({"path": path, "content": "new\n"}),
        ),
        (
            "edit_file",
            serde_json::json!({"path": path, "old_string": "old", "new_string": "new"}),
        ),
        ("delete_file", serde_json::json!({"path": path})),
    ] {
        std::fs::write(ws.path().join(path), "old\n").unwrap();
        let mut display = ToolDisplay::new(Vec::new(), false, 4096, 0, false);
        let raw = execute_tool_with_display_cancellable(
            &mut display,
            name,
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats_rw(ws.path()),
            &mut NoMcp,
            ToolCollaborators::default(),
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(raw.contains(path), "raw result retains the actual path");
        let visible = String::from_utf8(display.into_inner()).unwrap();
        assert!(
            visible.chars().all(|ch| !ch.is_control() || ch == '\n'),
            "{name} leaked pathname controls: {visible:?}"
        );
        assert!(
            visible.contains("file-<U+001B>[31m<U+0009>name.rs"),
            "{visible}"
        );
        assert!(
            visible.contains("display escapes control characters"),
            "{visible}"
        );
    }
}

#[tokio::test]
async fn file_receipts_keep_raw_source_but_neutralize_terminal_controls() {
    let ws = tempfile::TempDir::new().unwrap();
    let original = "start\u{1b}]8;;https://example.test/\u{7}link\u{1b}]8;;\u{7}\u{1b}]52;c;c2VjcmV0\u{1b}\\\r\tend\n";
    let replacement = "\tnew\u{1b}[31m\r\n";
    for (name, args, after) in [
        (
            "write_file",
            serde_json::json!({"path": "state.txt", "content": replacement}),
            Some(replacement.to_string()),
        ),
        (
            "edit_file",
            serde_json::json!({"path": "state.txt", "old_string": "start", "new_string": "changed"}),
            Some(original.replacen("start", "changed", 1)),
        ),
        (
            "delete_file",
            serde_json::json!({"path": "state.txt"}),
            None,
        ),
    ] {
        std::fs::write(ws.path().join("state.txt"), original).unwrap();
        let mut display = ToolDisplay::new(Vec::new(), false, 4096, 0, false);
        let raw = execute_tool_with_display_cancellable(
            &mut display,
            name,
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats_rw(ws.path()),
            &mut NoMcp,
            ToolCollaborators::default(),
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let expected_receipt = crate::agentic::tools::file_change::receipt(
            "state.txt",
            Some(original),
            after.as_deref(),
        )
        .unwrap();
        assert!(raw.contains(&expected_receipt), "raw {name} result changed");
        assert!(raw.contains("\u{1b}]52;"), "raw source was lost");
        let visible = String::from_utf8(display.into_inner()).unwrap();
        assert!(
            visible.chars().all(|ch| !ch.is_control() || ch == '\n'),
            "{name} terminal bytes still contain source controls: {visible:?}"
        );
        assert!(visible.contains("<U+001B>]8;"), "{visible}");
        assert!(visible.contains("<U+001B>]52;"), "{visible}");
        assert!(visible.contains("<U+000D>"), "{visible}");
        assert!(visible.contains("<U+0009>"), "{visible}");
        assert!(
            visible.contains("display escapes control characters"),
            "{visible}"
        );
        assert!(visible.contains("not a raw patch"), "{visible}");
        assert!(!raw.contains("display escapes control characters"));
    }
}

#[cfg(unix)]
#[test]
fn file_mutations_refuse_a_fifo_before_the_legacy_reads_can_block() {
    const CHILD: &str = "NEWT_RECEIPT_FIFO_CHILD";
    if let Some(done) = std::env::var_os(CHILD) {
        let ws = tempfile::TempDir::new().unwrap();
        let path = ws.path().join("fifo");
        let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the live path is NUL-terminated; mkfifo does not retain it.
        assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (tool, args) in [
            (
                "write_file",
                serde_json::json!({"path": "fifo", "content": "text"}),
            ),
            (
                "edit_file",
                serde_json::json!({"path": "fifo", "old_string": "a", "new_string": "b"}),
            ),
        ] {
            let output = runtime.block_on(run_tool(
                tool,
                args,
                ws.path(),
                &caveats_rw(ws.path()),
                None,
            ));
            assert!(output.starts_with("error:"), "{output}");
            assert!(output.contains("nonregular target"), "{output}");
        }
        assert!(matches!(
            crate::agentic::tools::file_capture::capture(&Scope::All, &path),
            crate::agentic::tools::file_capture::TextSnapshot::Unavailable(_)
        ));
        std::fs::write(done, "passed").unwrap();
        return;
    }
    let parent = tempfile::TempDir::new().unwrap();
    let done = parent.path().join("child-passed");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "agentic::tools::execute_tool_branch_tests::file_receipts::file_mutations_refuse_a_fifo_before_the_legacy_reads_can_block",
            "--nocapture",
        ])
        .env(CHILD, &done)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "FIFO tool child failed: {status}");
            assert!(done.exists(), "the exact FIFO child test did not run");
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("a file mutation blocked on a FIFO with no writer");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

struct ChangeWhileConfirming {
    path: std::path::PathBuf,
}

impl PermissionGate for ChangeWhileConfirming {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        panic!("the configured scope already permits this write")
    }

    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        std::fs::write(&self.path, "changed while confirming\n").unwrap();
        HumanQuestionOutcome::Answer("y".into())
    }
}

#[tokio::test]
async fn a_write_captures_its_preimage_after_confirmation() {
    use super::super::disable_ocap_tests::{env_lock, EnvVar};

    let _lock = env_lock().await;
    let _normal_mode = EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let ws = tempfile::TempDir::new().unwrap();
    let path = ws.path().join("state.txt");
    std::fs::write(&path, "before confirmation\n").unwrap();
    let mut gate = ChangeWhileConfirming { path };
    let caveats = Caveats {
        fs_write: Scope::All,
        ..caveats_rw(ws.path())
    };
    let output = execute_tool(
        "write_file",
        &serde_json::json!({"path": "state.txt", "content": "written\n"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        Some(&mut gate),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        output.contains("-changed while confirming\n+written\n"),
        "{output}"
    );
    assert!(!output.contains("-before confirmation"), "{output}");
}

#[tokio::test]
async fn sequential_file_tools_show_the_immediate_change_without_an_artifact_sink() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let added = run_tool(
        "write_file",
        serde_json::json!({"path": "state.txt", "content": "one\n"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(added.starts_with("wrote state.txt"), "{added}");
    assert!(added.contains("Added (+1 -0)"), "{added}");
    assert!(added.contains("--- /dev/null\n"), "{added}");
    assert!(added.contains("+one\n"), "{added}");

    let edited = run_tool(
        "edit_file",
        serde_json::json!({"path": "state.txt", "old_string": "one", "new_string": "two"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(edited.starts_with("edited state.txt"), "{edited}");
    assert!(edited.contains("Modified (+1 -1)"), "{edited}");
    assert!(edited.contains("-one\n+two\n"), "{edited}");
    assert!(!edited.contains("--- /dev/null\n"), "{edited}");

    let deleted = run_tool(
        "delete_file",
        serde_json::json!({"path": "state.txt"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(deleted.starts_with("deleted state.txt"), "{deleted}");
    assert!(deleted.contains("Deleted (+0 -1)"), "{deleted}");
    assert!(deleted.contains("-two\n"), "{deleted}");
    assert!(!ws.path().join("state.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_build_check_keeps_the_verified_tool_change() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("state.txt"), "old\n").unwrap();
    let output = run_tool(
        "write_file",
        serde_json::json!({"path": "state.txt", "content": "tool bytes\n"}),
        ws.path(),
        &caveats_rw(ws.path()),
        Some("printf 'build bytes\\n' > state.txt; exit 1"),
    )
    .await;
    assert!(output.contains("Modified (+1 -1)"), "{output}");
    assert!(output.contains("-old\n+tool bytes\n"), "{output}");
    assert!(!output.contains("+build bytes\n"), "{output}");
    assert!(output.contains("build check failed"), "{output}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("state.txt")).unwrap(),
        "build bytes\n"
    );
}

#[tokio::test]
async fn unavailable_preimages_do_not_become_added_file_receipts() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("state.txt"), "private old bytes\n").unwrap();
    let caveats = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let output = run_tool(
        "write_file",
        serde_json::json!({"path": "state.txt", "content": "public result\n"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(output.starts_with("wrote state.txt"), "{output}");
    assert!(
        output.contains("file-change receipt unavailable"),
        "{output}"
    );
    assert!(!output.contains("private old bytes"), "{output}");
    assert!(!output.contains("```diff"), "{output}");

    std::fs::write(ws.path().join("binary.txt"), [0xff, 0xfe]).unwrap();
    let binary = run_tool(
        "write_file",
        serde_json::json!({"path": "binary.txt", "content": "text\n"}),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
    )
    .await;
    assert!(
        binary.contains("file-change receipt unavailable"),
        "{binary}"
    );
    assert!(!binary.contains("Added (+"), "{binary}");
}

#[cfg(unix)]
#[tokio::test]
async fn deleting_a_final_symlink_does_not_report_its_target_contents_as_deleted() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("target.txt"), "target remains\n").unwrap();
    std::os::unix::fs::symlink("target.txt", ws.path().join("link.txt")).unwrap();
    let output = run_tool(
        "delete_file",
        serde_json::json!({"path": "link.txt"}),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
    )
    .await;
    assert!(output.starts_with("deleted link.txt"), "{output}");
    assert!(
        output.contains("file-change receipt unavailable"),
        "{output}"
    );
    assert!(!output.contains("-target remains"), "{output}");
    assert!(!output.contains("```diff"), "{output}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("target.txt")).unwrap(),
        "target remains\n"
    );
}

use super::*;

#[tokio::test]
async fn edit_file_replaces_unique_match_and_reports_delta() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("f.txt"), "hello world\nsecond line\n").unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "edit_file",
        serde_json::json!({
            "path": "f.txt",
            "old_string": "world",
            "new_string": "rust\nand more"
        }),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("edited f.txt (+1 lines"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("f.txt")).unwrap(),
        "hello rust\nand more\nsecond line\n"
    );
}

#[tokio::test]
async fn edit_file_rejects_empty_missing_and_ambiguous_old_string() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("f.txt"), "dup\ndup\n").unwrap();
    let caveats = caveats_rw(ws.path());

    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "", "new_string": "x"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("old_string must not be empty"), "got: {out}");

    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "absent", "new_string": "x"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("old_string not found in f.txt"), "got: {out}");
    // The miss error now shows the file's actual contents so the model can
    // copy the exact text instead of blind-guessing old_string again.
    assert!(out.contains("do not guess again"), "got: {out}");
    assert!(
        out.contains("dup"),
        "miss error must include the file content: {out}"
    );

    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "dup", "new_string": "x"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("matches 2 locations"), "got: {out}");
    // The ambiguous edit must NOT have touched the file.
    assert_eq!(
        std::fs::read_to_string(ws.path().join("f.txt")).unwrap(),
        "dup\ndup\n"
    );
}

#[tokio::test]
async fn edit_file_denied_outside_fs_write_scope_and_missing_file() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "a", "new_string": "b"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(
        out.contains("capability denied: fs_write"),
        "denied before any fs access, got: {out}"
    );

    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "missing.txt", "old_string": "a", "new_string": "b"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("error reading missing.txt"), "got: {out}");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn edit_file_symlink_under_workspace_escaping_is_denied() {
    // step-52.5: under a CONFINED fs_write, a symlink UNDER the workspace
    // pointing outside must not let edit_file read OR write the outside file.
    // Both the read of `existing` (which could leak the outside head on a
    // no-match) and the write are object-bound; the outside file is unchanged
    // and its contents never appear in the output. Verified red→green.
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "OUTSIDE SECRET\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let out = run_tool(
        "edit_file",
        serde_json::json!({
            "path": "link/secret.txt",
            "old_string": "OUTSIDE",
            "new_string": "EDITED",
        }),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
    )
    .await;

    assert!(
        !out.contains("OUTSIDE SECRET"),
        "object-bound edit must not leak the outside file: {out}"
    );
    assert_eq!(
        out,
        denied_fs_result("fs_write", "link/secret.txt"),
        "the symlink-escape edit must be denied: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
        "OUTSIDE SECRET\n",
        "the outside file must be UNCHANGED"
    );
}

#[tokio::test]
async fn edit_file_appends_build_check_result() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("f.txt"), "old\n").unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "old", "new_string": "new"}),
        ws.path(),
        &caveats,
        Some(passing_build_check_cmd()),
    )
    .await;
    // build_check runs CONFINED (P4). On Linux+Landlock the check runs and
    // its outcome is reflected; off it (e.g. Windows without the AppContainer
    // launcher) it fails closed — either way the tool APPENDS a build-check
    // line, which is what this test guards.
    let confinable = crate::confined_exec::kernel_fs_fence_available();
    if confinable {
        assert!(out.contains("✓ build check passed"), "got: {out}");
    } else {
        assert!(
            out.contains("build check"),
            "build-check line appended: {out}"
        );
    }

    let failing_check = failing_build_check_cmd("broke");
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "new", "new_string": "newer"}),
        ws.path(),
        &caveats,
        Some(&failing_check),
    )
    .await;
    if confinable {
        assert!(out.contains("✗ build check failed"), "got: {out}");
        assert!(out.contains("broke"), "model sees the failure text: {out}");
    } else {
        assert!(
            out.contains("build check"),
            "build-check line appended: {out}"
        );
    }
}

#[tokio::test]
async fn write_file_shrink_guard_refuses_large_deletion() {
    let ws = tempfile::TempDir::new().unwrap();
    let big: String = (0..100).map(|i| format!("line {i}\n")).collect();
    std::fs::write(ws.path().join("big.txt"), &big).unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "write_file",
        serde_json::json!({"path": "big.txt", "content": "tiny\n"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(
        out.contains("would shrink big.txt from 100 → 1 lines"),
        "got: {out}"
    );
    assert!(out.contains("edit_file"), "points at the safer tool: {out}");
    // The guard refused — the original file must be intact.
    assert_eq!(
        std::fs::read_to_string(ws.path().join("big.txt")).unwrap(),
        big
    );
}

#[tokio::test]
async fn write_file_creates_parent_directories() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "write_file",
        serde_json::json!({"path": "a/b/c.txt", "content": "nested"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("wrote a/b/c.txt"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("a/b/c.txt")).unwrap(),
        "nested"
    );
}

#[tokio::test]
async fn delete_file_removes_one_file_and_appends_build_check() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("old.rs"), "fn main() {}\n").unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "old.rs"}),
        ws.path(),
        &caveats,
        Some(passing_build_check_cmd()),
    )
    .await;
    assert!(out.starts_with("deleted old.rs"), "got: {out}");
    // Confined build_check (P4): outcome-checked on Linux+Landlock, else the
    // fail-closed line still counts as an appended build-check result.
    if crate::confined_exec::kernel_fs_fence_available() {
        assert!(out.contains("✓ build check passed"), "got: {out}");
    } else {
        assert!(
            out.contains("build check"),
            "build-check line appended: {out}"
        );
    }
    assert!(
        !ws.path().join("old.rs").exists(),
        "delete_file must remove the target file"
    );
}

#[tokio::test]
async fn delete_file_denies_missing_files_directories_and_fs_write_misses() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    std::fs::create_dir(ws.path().join("dir")).unwrap();

    let denied = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert!(out.contains("capability denied: fs_write"), "got: {out}");
    assert!(
        ws.path().join("secret.txt").exists(),
        "denied delete must not remove the file"
    );

    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "missing.txt"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("file does not exist"), "got: {out}");

    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "dir"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("refuses directories"), "got: {out}");
    assert!(ws.path().join("dir").is_dir(), "directory must remain");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn delete_file_symlink_under_workspace_escaping_is_denied() {
    // step-52.6: under a CONFINED fs_write, a symlink UNDER the workspace
    // pointing outside must not let delete_file remove the outside file.
    // Object-bound via `unlinkat` on the resolved parent — the escape is
    // refused and the outside file survives. Before the rewire `remove_file`
    // followed the intermediate symlink and deleted outside. Verified
    // red→green.
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("victim.txt"), "keep me\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "link/victim.txt"}),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
    )
    .await;

    assert_eq!(
        out,
        denied_fs_result("fs_write", "link/victim.txt"),
        "the symlink-escape delete must be denied: {out}"
    );
    assert!(
        outside.path().join("victim.txt").exists(),
        "the outside file must survive — the delete never escaped"
    );
}

#[tokio::test]
async fn read_file_denial_and_missing_file_errors() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert!(out.contains("capability denied: fs_read"), "got: {out}");

    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "nope.txt"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("error reading nope.txt"), "got: {out}");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn read_file_symlink_under_workspace_escaping_is_denied() {
    // step-52.2 (fs-canonical-containment / #522): under a CONFINED fs_read
    // (Only{ws}, not All), a symlink UNDER the workspace whose target is
    // outside it must not let read_file exfiltrate the outside file — even
    // though the lexical gate admits the name `link/secret.txt`. The read is
    // object-bound through `WorkspaceDir` (openat2 RESOLVE_BENEATH), so the
    // escape is refused by the kernel. Before the rewire this returned the
    // secret — the named residual. Real-fs tier (grounds the object gate);
    // Linux-only (openat2).
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "TOP SECRET").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let confined = Caveats {
        fs_read: Scope::only([ws.path().to_string_lossy().into_owned()]),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "link/secret.txt"}),
        ws.path(),
        &confined,
        None,
    )
    .await;

    assert!(
        !out.contains("TOP SECRET"),
        "object-bound read must not follow a symlink out of the workspace: {out}"
    );
    assert_eq!(
        out,
        denied_fs_result("fs_read", "link/secret.txt"),
        "a contained-read escape must surface as an fs_read denial: {out}"
    );
}

#[tokio::test]
async fn list_dir_denial_and_missing_dir_errors() {
    let ws = tempfile::TempDir::new().unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "list_dir",
        serde_json::json!({"path": "."}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert!(out.contains("capability denied: fs_read"), "got: {out}");

    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "list_dir",
        serde_json::json!({"path": "not-a-dir"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.starts_with("error:"), "got: {out}");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn list_dir_symlink_under_workspace_escaping_is_denied() {
    // step-52.3: object-bound listing. Under a CONFINED fs_read (Only{ws}), a
    // symlink UNDER the workspace pointing to an outside directory must not
    // let list_dir enumerate the outside dir — even though the lexical gate
    // admits the name `link`. Before the rewire the outside entries were
    // listed (the #522 residual). Real-fs tier; Linux-only.
    let ws = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("outside_secret.txt"), "x").unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    let confined = Caveats {
        fs_read: Scope::only([ws.path().to_string_lossy().into_owned()]),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "list_dir",
        serde_json::json!({"path": "link"}),
        ws.path(),
        &confined,
        None,
    )
    .await;

    assert!(
        !out.contains("outside_secret.txt"),
        "object-bound list_dir must not enumerate a directory outside the workspace: {out}"
    );
    assert_eq!(out, denied_fs_result("fs_read", "link"), "got: {out}");
}

use super::*;

async fn missing(tool: &str, path: &str, ws: &std::path::Path, caveats: &Caveats) -> String {
    run_tool(
        tool,
        serde_json::json!({"path":path, "old_string":"x", "new_string":"y", "pattern":"x"}),
        ws,
        caveats,
        None,
    )
    .await
}

/// #2755: every path-taking file tool offers the same wrong-prefix repair,
/// including absolute model paths, without performing the suggested operation.
#[tokio::test]
async fn path_suggest_wrong_prefix_across_file_tools() {
    let ws = tempfile::tempdir().unwrap();
    touch(ws.path(), "src/item.rs");
    let caveats = caveats_rw(ws.path());
    for tool in [
        "read_file",
        "edit_file",
        "delete_file",
        "list_dir",
        "find",
        "grep",
    ] {
        let target = if matches!(tool, "list_dir" | "find" | "grep") {
            "src"
        } else {
            "src/item.rs"
        };
        for path in [
            format!("crates/{target}"),
            ws.path()
                .join(format!("crates/{target}"))
                .to_string_lossy()
                .into_owned(),
        ] {
            let out = missing(tool, &path, ws.path(), &caveats).await;
            assert!(
                out.contains(&format!("did you mean {target}?")),
                "{tool}: {out}"
            );
            assert_eq!(out.matches("did you mean").count(), 1, "{out}");
        }
    }
    assert_eq!(
        std::fs::read_to_string(ws.path().join("src/item.rs")).unwrap(),
        "x"
    );
}

/// #2755: absence and inaccessible candidate names must not invent suggestions.
#[tokio::test]
async fn path_suggest_no_candidate_or_read_authority_keeps_plain_error() {
    let ws = tempfile::tempdir().unwrap();
    touch(ws.path(), "src/item.rs");
    for tool in [
        "read_file",
        "edit_file",
        "delete_file",
        "list_dir",
        "find",
        "grep",
    ] {
        let out = missing(tool, "crates/missing", ws.path(), &caveats_rw(ws.path())).await;
        assert!(!out.contains("did you mean"), "{tool}: {out}");
        let mut caveats = caveats_rw(ws.path());
        // Permit the miss, but not the existing candidate, so write authority
        // alone cannot expose the read-restricted candidate.
        std::fs::create_dir_all(ws.path().join("crates")).unwrap();
        caveats.fs_read = Scope::only([ws.path().join("crates").to_string_lossy().into_owned()]);
        let out = missing(tool, "crates/src/item.rs", ws.path(), &caveats).await;
        assert!(!out.contains("did you mean"), "{tool}: {out}");
    }
}

/// #2755: a failed text match is not a missing path and must not redirect edits.
#[tokio::test]
async fn path_suggest_existing_target_does_not_suggest_suffix() {
    let ws = tempfile::tempdir().unwrap();
    touch(ws.path(), "src/item.rs");
    touch(ws.path(), "crates/src/item.rs");
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path":"crates/src/item.rs", "old_string":"absent", "new_string":"y"}),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
    )
    .await;
    assert!(out.contains("old_string not found"), "{out}");
    assert!(!out.contains("did you mean"), "{out}");
}

/// #2755: copy_from's shared authorized read reports the source, never redirects
/// the write to the suggested file or creates its destination on failure.
#[tokio::test]
async fn path_suggest_copy_source_uses_shared_read() {
    let ws = tempfile::tempdir().unwrap();
    touch(ws.path(), "src/item.rs");
    let out = run_tool("write_file", serde_json::json!({"path":"out.rs", "content":"", "copy_from":{"path":"crates/src/item.rs", "start_line":1, "end_line":1}}), ws.path(), &caveats_rw(ws.path()), None).await;
    assert!(out.contains("did you mean src/item.rs?"), "{out}");
    assert!(!ws.path().join("out.rs").exists());
}

/// #2755: bounded suffix selection prefers the longest hit and does not search
/// tracked names, normalize traversal, decorate denials, or rewrite plain errors.
#[test]
fn path_suggest_bounded_suffix_and_plain_error_controls() {
    let ws = tempfile::tempdir().unwrap();
    touch(ws.path(), "src/item.rs");
    touch(ws.path(), "item.rs");
    let root = ws.path().to_str().unwrap();
    let hint = |path: &str, scope: &Scope<String>| {
        super::super::path_suggest::on_error("error: missing".into(), path, root, scope)
    };
    assert_eq!(
        hint("one/two/src/item.rs", &Scope::All),
        "error: missing\ndid you mean src/item.rs?"
    );
    for path in ["one/two/absent.rs", "wrong/../src/item.rs"] {
        assert_eq!(hint(path, &Scope::All), "error: missing");
    }
    assert_eq!(hint("wrong/src/item.rs", &Scope::none()), "error: missing");
    assert_eq!(
        hint(&format!("{}src/item.rs", "wrong/".repeat(33)), &Scope::All),
        "error: missing"
    );
    assert_eq!(
        hint(&format!("{}/src/item.rs", "x".repeat(4096)), &Scope::All),
        "error: missing"
    );
    assert_eq!(
        super::super::path_suggest::on_error(
            "capability denied: fs_read".into(),
            "wrong/src/item.rs",
            root,
            &Scope::All
        ),
        "capability denied: fs_read"
    );
}

/// #2755: lexical read authority cannot reveal a candidate through a symlink
/// outside either the workspace or the granted subtree.
#[cfg(unix)]
#[test]
fn path_suggest_symlink_escape_is_not_a_candidate() {
    let ws = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    touch(outside.path(), "secret.rs");
    std::os::unix::fs::symlink(outside.path(), ws.path().join("escape")).unwrap();
    let hint = |path: &str, scope: &Scope<String>| {
        super::super::path_suggest::on_error(
            "error: missing".into(),
            path,
            ws.path().to_str().unwrap(),
            scope,
        )
    };
    assert_eq!(
        hint("wrong/escape/secret.rs", &Scope::All),
        "error: missing"
    );
    touch(ws.path(), "private/secret.rs");
    std::fs::create_dir(ws.path().join("allowed")).unwrap();
    std::os::unix::fs::symlink(ws.path().join("private"), ws.path().join("allowed/link")).unwrap();
    let scope = Scope::only([ws.path().join("allowed").to_string_lossy().into_owned()]);
    assert_eq!(
        hint("wrong/allowed/link/secret.rs", &scope),
        "error: missing"
    );
}

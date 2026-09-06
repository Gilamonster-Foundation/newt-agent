use super::*;

// --- #496: the embedded `find` tool -----------------------------------

/// Convenience for `find` calls through the real dispatch under a
/// read-everything session.
async fn run_find(args: serde_json::Value, ws: &std::path::Path) -> String {
    run_tool("find", args, ws, &caveats_rw(ws), None).await
}

/// Regression for #496: an agent needed `find . -name pyo3_module.rs` but
/// the build's shell tool was unavailable. The embedded tool must locate the
/// file by basename, ignoring decoys, and return its workspace-relative path
/// (no shell, no `| sort`). Fails before this tool existed (`unknown tool:
/// find`).
#[tokio::test]
async fn find_locates_file_by_name_issue_496() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "newt-core/src/pyo3_module.rs");
    touch(ws.path(), "newt-data/src/other.rs");
    touch(ws.path(), "docs/pyo3_module.md"); // decoy: wrong extension
    let out = run_find(serde_json::json!({ "name": "pyo3_module.rs" }), ws.path()).await;
    assert_eq!(out, "newt-core/src/pyo3_module.rs", "got: {out}");
}

/// 2026-07-26 regression: "code files with the highest line counts" must
/// NOT rank AGENTS.md / Cargo.lock. `code: true` keeps language-pack
/// source only (same allowlist as nav gather).
#[tokio::test]
async fn find_code_true_excludes_docs_and_lockfiles_from_line_ranking() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("tall.rs"), "x\n".repeat(20)).unwrap();
    std::fs::write(ws.path().join("short.rs"), "x\n".repeat(5)).unwrap();
    std::fs::write(ws.path().join("AGENTS.md"), "d\n".repeat(200)).unwrap();
    std::fs::write(ws.path().join("Cargo.lock"), "l\n".repeat(100)).unwrap();
    std::fs::write(ws.path().join("LICENSE"), "L\n".repeat(50)).unwrap();
    let out = run_find(
        serde_json::json!({
            "path": ".",
            "type": "f",
            "code": true,
            "sort": "lines",
            "show_lines": true,
            "max_results": 10
        }),
        ws.path(),
    )
    .await;
    assert!(
        out.contains("20\ttall.rs") && out.contains("5\tshort.rs"),
        "code sources with line counts: {out}"
    );
    assert!(
        !out.contains("AGENTS.md") && !out.contains("Cargo.lock") && !out.contains("LICENSE"),
        "docs/lockfiles/LICENSE must be excluded: {out}"
    );
    let tall = out.find("20\ttall.rs").expect("tall first");
    let short = out.find("5\tshort.rs").expect("short second");
    assert!(tall < short, "lines descending: {out}");
}

/// The other call the blocked agent reached for:
/// `find examples -maxdepth 2 -type f -name '*.py'`. Exercises glob + type
/// filter + max_depth together, and confirms output is pre-sorted.
#[tokio::test]
async fn find_glob_type_and_maxdepth_together() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "examples/a.py"); // depth 1 — match
    touch(ws.path(), "examples/sub/b.py"); // depth 2 — match
    touch(ws.path(), "examples/sub/deep/c.py"); // depth 3 — too deep
    touch(ws.path(), "examples/readme.md"); // wrong extension
    std::fs::create_dir_all(ws.path().join("examples/empty_dir")).unwrap();
    let out = run_find(
        serde_json::json!({
            "path": "examples", "name": "*.py", "type": "f", "max_depth": 2
        }),
        ws.path(),
    )
    .await;
    // Pre-sorted, exactly the two in-depth .py files, no dir, no .md, no
    // depth-3 file — and no shell `| sort` needed.
    assert_eq!(out, "examples/a.py\nexamples/sub/b.py", "got: {out}");
}

/// `code` is a harness-owned semantic category: it includes source files
/// from every registered language pack and excludes docs/manifests/locks.
/// This real-filesystem test grounds the pure language-registry classifier.
#[tokio::test]
async fn find_source_category_filters_repository_metadata_across_languages() {
    let ws = tempfile::TempDir::new().unwrap();
    for file in [
        "src/main.rs",
        "src/app.py",
        "web/app.ts",
        "java/App.java",
        "native/app.cpp",
        "dotnet/App.cs",
        "ruby/app.rb",
        "scripts/build.sh",
        "AGENTS.md",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        touch(ws.path(), file);
    }

    let out = run_find(
        serde_json::json!({ "category": "source", "type": "f" }),
        ws.path(),
    )
    .await;

    for source in [
        "src/main.rs",
        "src/app.py",
        "web/app.ts",
        "java/App.java",
        "native/app.cpp",
        "dotnet/App.cs",
        "ruby/app.rb",
        "scripts/build.sh",
    ] {
        assert!(
            out.lines().any(|line| line == source),
            "missing {source}: {out}"
        );
    }
    for metadata in ["AGENTS.md", "Cargo.toml", "Cargo.lock"] {
        assert!(
            !out.lines().any(|line| line == metadata),
            "metadata is not source code ({metadata}): {out}"
        );
    }
}

/// A named language narrows the generic source category through pack
/// aliases. The mocked tool schema and pure registry tests sit underneath;
/// this real walk proves the filter reaches filesystem behavior.
#[tokio::test]
async fn find_language_alias_narrows_source_files() {
    let ws = tempfile::TempDir::new().unwrap();
    for file in ["native/a.c", "native/b.cpp", "dotnet/App.cs", "src/main.rs"] {
        touch(ws.path(), file);
    }

    let cpp = run_find(serde_json::json!({ "language": "C++" }), ws.path()).await;
    assert_eq!(cpp, "native/a.c\nnative/b.cpp");
    let csharp = run_find(serde_json::json!({ "language": "C#" }), ws.path()).await;
    assert_eq!(csharp, "dotnet/App.cs");
}

/// Output is sorted ascending regardless of filesystem/creation order.
#[tokio::test]
async fn find_output_is_sorted() {
    let ws = tempfile::TempDir::new().unwrap();
    for f in ["m.txt", "a.txt", "z.txt", "c.txt"] {
        touch(ws.path(), f);
    }
    let out = run_find(serde_json::json!({ "name": "*.txt" }), ws.path()).await;
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        vec!["a.txt", "c.txt", "m.txt", "z.txt"],
        "got: {out}"
    );
}

/// `type` restricts to files or directories.
#[tokio::test]
async fn find_type_filter() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "pkg/file.rs");
    std::fs::create_dir_all(ws.path().join("pkg/sub")).unwrap();
    let dirs = run_find(serde_json::json!({ "type": "d" }), ws.path()).await;
    assert!(
        dirs.contains("pkg") && dirs.contains("pkg/sub"),
        "got: {dirs}"
    );
    assert!(!dirs.contains("file.rs"), "dirs-only leaked a file: {dirs}");
    let files = run_find(serde_json::json!({ "type": "f" }), ws.path()).await;
    assert!(files.contains("pkg/file.rs"), "got: {files}");
    assert!(
        !files.lines().any(|l| l == "pkg" || l == "pkg/sub"),
        "files-only leaked a dir: {files}"
    );
}

/// .gitignore + the default build/dep skips are honoured by default and
/// can be disabled with `respect_gitignore=false`.
#[tokio::test]
async fn find_gitignore_and_default_skips() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join(".gitignore"), "ignored.txt\n").unwrap();
    touch(ws.path(), "kept.txt");
    touch(ws.path(), "ignored.txt");
    touch(ws.path(), "target/build_artifact.txt");
    touch(ws.path(), "node_modules/dep.txt");

    let on = run_find(serde_json::json!({ "name": "*.txt" }), ws.path()).await;
    assert!(on.contains("kept.txt"), "got: {on}");
    assert!(!on.contains("ignored.txt"), "gitignore not honoured: {on}");
    assert!(!on.contains("target/"), "target not skipped: {on}");
    assert!(
        !on.contains("node_modules/"),
        "node_modules not skipped: {on}"
    );

    let off = run_find(
        serde_json::json!({ "name": "*.txt", "respect_gitignore": false }),
        ws.path(),
    )
    .await;
    assert!(off.contains("ignored.txt"), "opt-out should show it: {off}");
    assert!(off.contains("target/build_artifact.txt"), "got: {off}");
}

/// `max_results` caps output and the result notes the truncation.
#[tokio::test]
async fn find_max_results_caps_and_notes_truncation() {
    let ws = tempfile::TempDir::new().unwrap();
    for i in 0..10 {
        touch(ws.path(), &format!("f{i}.txt"));
    }
    let out = run_find(
        serde_json::json!({ "name": "*.txt", "max_results": 3 }),
        ws.path(),
    )
    .await;
    let body: Vec<&str> = out.lines().filter(|l| l.ends_with(".txt")).collect();
    assert_eq!(body.len(), 3, "should cap at 3: {out}");
    assert!(out.contains("truncated at 3"), "got: {out}");
}

/// A missing root is a clear error, and an empty match set says so.
#[tokio::test]
async fn find_missing_root_and_no_matches() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.txt");
    let missing = run_find(serde_json::json!({ "path": "does/not/exist" }), ws.path()).await;
    assert!(missing.starts_with("error:"), "got: {missing}");
    let empty = run_find(serde_json::json!({ "name": "*.nope" }), ws.path()).await;
    assert_eq!(empty, "no matches", "got: {empty}");
}

/// fs_read denial: no scope + no prompt gate ⇒ capability denied (same UX
/// as list_dir/read_file).
#[tokio::test]
async fn find_denied_without_fs_read() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "secret.txt");
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "find",
        serde_json::json!({ "name": "*" }),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert!(out.starts_with("capability denied"), "got: {out}");
}

/// A `..` root that escapes the workspace is refused even when the session
/// grants fs_read everywhere (defence-in-depth for a recursive read).
#[tokio::test]
async fn find_refuses_root_outside_workspace() {
    let parent = tempfile::TempDir::new().unwrap();
    std::fs::write(parent.path().join("outside.txt"), b"x").unwrap();
    let ws = parent.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // fs_read: All, so the only thing that can stop the escape is the
    // canonical-root containment check.
    let out = run_find(serde_json::json!({ "path": ".." }), &ws).await;
    assert!(out.starts_with("capability denied"), "got: {out}");
}

/// An empty `name` is treated as "match everything" (the `!g.is_empty()`
/// guard routes `Some("")` to the no-filter path; without it the glob would
/// compile to `^$` and match nothing).
#[tokio::test]
async fn find_empty_name_matches_everything() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.txt");
    touch(ws.path(), "sub/b.rs");
    let out = run_find(serde_json::json!({ "name": "" }), ws.path()).await;
    for expected in ["a.txt", "sub", "sub/b.rs"] {
        assert!(
            out.lines().any(|l| l == expected),
            "empty name should match `{expected}`: {out}"
        );
    }
}

/// Hidden entries (dotfiles / dotdirs) are pruned by default and surface
/// only when `respect_gitignore=false` — relevant because dotfiles can hold
/// secrets (.env, .ssh). Pins the `.hidden(respect_gitignore)` branch.
#[tokio::test]
async fn find_hidden_entries_gated_by_respect_gitignore() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "visible.txt");
    touch(ws.path(), ".hidden.txt");
    touch(ws.path(), ".config/secret.txt");

    let default = run_find(serde_json::json!({ "name": "*" }), ws.path()).await;
    assert!(
        default.lines().any(|l| l == "visible.txt"),
        "got: {default}"
    );
    assert!(
        !default.contains(".hidden") && !default.contains(".config"),
        "hidden entries must be skipped by default: {default}"
    );

    let all = run_find(
        serde_json::json!({ "name": "*", "respect_gitignore": false }),
        ws.path(),
    )
    .await;
    assert!(all.contains(".hidden.txt"), "opt-out should show it: {all}");
    assert!(all.contains(".config/secret.txt"), "got: {all}");
}

/// Security boundary: `find` never follows symlinked directories, so a link
/// pointing outside the workspace cannot leak the target's contents (pins
/// `.follow_links(false)`). Unix-only — Windows symlinks need privileges.
#[cfg(unix)]
#[tokio::test]
async fn find_does_not_follow_symlinks_out_of_workspace() {
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"x").unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "inside.txt");
    std::os::unix::fs::symlink(outside.path(), ws.path().join("link")).unwrap();

    // The symlink is present but is NOT descended into.
    let leaked = run_find(serde_json::json!({ "name": "secret.txt" }), ws.path()).await;
    assert_eq!(
        leaked, "no matches",
        "symlink was followed out of ws: {leaked}"
    );
    // Sanity: a real in-workspace file is still found.
    let found = run_find(serde_json::json!({ "name": "inside.txt" }), ws.path()).await;
    assert_eq!(found, "inside.txt", "got: {found}");
}

#[test]
fn glob_to_regex_anchors_and_escapes() {
    // '*' is a wildcard; '.' is literal (not "any char").
    let re = glob_to_regex("*.py", true).unwrap();
    assert!(re.is_match("foo.py"));
    assert!(!re.is_match("foo.pyc")); // anchored at end
    assert!(!re.is_match("fooxpy")); // '.' is literal
                                     // Exact basename, '?' = single char, case-sensitivity honoured.
    assert!(glob_to_regex("a?c", true).unwrap().is_match("abc"));
    assert!(!glob_to_regex("a?c", true).unwrap().is_match("ac"));
    assert!(glob_to_regex("readme.md", false)
        .unwrap()
        .is_match("README.MD"));
    assert!(!glob_to_regex("readme.md", true)
        .unwrap()
        .is_match("README.MD"));
}

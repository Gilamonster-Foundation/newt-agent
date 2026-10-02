use super::*;

// --- Embedded `grep` tool (native, in-process regex line search) -----

/// Convenience for `grep` calls through the real dispatch under a
/// read-everything session.
async fn run_grep(args: serde_json::Value, ws: &std::path::Path) -> String {
    run_tool("grep", args, ws, &caveats_rw(ws), None).await
}

/// The embedded `grep` must locate text across files WITHOUT a shell or
/// subprocess. Returns one line per hit as `relative/path:LINE:matching-text`
/// (Fails before this tool existed: `unknown tool: grep`).
#[tokio::test]
async fn grep_finds_regex_in_files_without_shell() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    std::fs::write(ws.path().join("a.rs"), b"one\ntwo\nthree\n").unwrap();
    touch(ws.path(), "b.rs");
    std::fs::write(ws.path().join("b.rs"), b"FOO\nbar\n").unwrap();
    let out = run_grep(serde_json::json!({ "pattern": "bar" }), ws.path()).await;
    assert_eq!(out, "b.rs:2:bar", "got: {out}");
}

/// Regression: the embedded tool must never spawn a process, even where
/// `run_command` is refused. It returns text from a file the agent could not
/// otherwise `cat`.
#[tokio::test]
async fn grep_runs_when_shell_is_unavailable() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    std::fs::write(ws.path().join("a.rs"), b"panic: boom\nok\n").unwrap();
    // The `None` gate models a session with no shell; the embedded tool still
    // only needs fs_read.
    let out = run_grep(serde_json::json!({ "pattern": "panic" }), ws.path()).await;
    assert_eq!(out, "a.rs:1:panic: boom", "got: {out}");
}

/// `ignore_case` performs a case-insensitive match.
#[tokio::test]
async fn grep_case_insensitive_match() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    std::fs::write(ws.path().join("a.rs"), b"HELLO\nworld\nHello there\n").unwrap();
    let out = run_grep(
        serde_json::json!({ "pattern": "hello", "ignore_case": true }),
        ws.path(),
    )
    .await;
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "got: {out}");
    // Two hits: line 1 (`HELLO`) and line 3 (`Hello there`), case-insensitive.
    assert!(
        out.contains("a.rs:1:HELLO") && out.contains("a.rs:3:Hello there"),
        "got: {out}"
    );
}

/// `context` N emits N lines before and after each hit, as grep does
/// (`path-N-text`).
#[tokio::test]
async fn grep_emits_context_lines_with_dash_slot() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    std::fs::write(ws.path().join("a.rs"), b"line1\nline2\nHIT\nline4\nline5\n").unwrap();
    let out = run_grep(
        serde_json::json!({ "pattern": "HIT", "context": 1 }),
        ws.path(),
    )
    .await;
    assert_eq!(out, "a.rs-2-line2\na.rs:3:HIT\na.rs-4-line4", "got: {out}");
}

/// Binary files (any NUL byte) are skipped silently, like `grep -I`.
#[tokio::test]
async fn grep_skips_binary_files() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    std::fs::write(ws.path().join("a.rs"), b"\x00\x00\x00\x00\x00").unwrap();
    let out = run_grep(serde_json::json!({ "pattern": "." }), ws.path()).await;
    assert!(out.contains("no matches"), "got: {out}");
}

/// A `glob` restricts the search to matching paths (workspace-relative).
#[tokio::test]
async fn grep_glob_restricts_search() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "src/a.rs");
    std::fs::write(ws.path().join("src/a.rs"), b"needle\n").unwrap();
    touch(ws.path(), "docs/a.rs");
    std::fs::write(ws.path().join("docs/a.rs"), b"needle\n").unwrap();
    let out = run_grep(
        serde_json::json!({ "pattern": "needle", "glob": "src/**" }),
        ws.path(),
    )
    .await;
    assert_eq!(out, "src/a.rs:1:needle", "got: {out}");
}

/// A junk `context` fails loudly rather than being swallowed.
#[tokio::test]
async fn grep_rejects_junk_context() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    std::fs::write(ws.path().join("a.rs"), b"x\n").unwrap();
    let out = run_grep(
        serde_json::json!({ "pattern": "x", "context": "lots" }),
        ws.path(),
    )
    .await;
    assert!(out.starts_with("error"), "got: {out}");
}

/// An empty pattern is a hard error (no silent "match everything").
#[tokio::test]
async fn grep_requires_pattern() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    let out = run_grep(serde_json::json!({}), ws.path()).await;
    assert!(
        out.starts_with("error: grep: `pattern` is required"),
        "got: {out}"
    );
}

/// `max_results` caps the number of hits and notes truncation.
#[tokio::test]
async fn grep_max_results_caps_and_notes_truncation() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    let body: String = (0..50).map(|i| format!("row{i}\n")).collect();
    std::fs::write(ws.path().join("a.rs"), body.as_bytes()).unwrap();
    let out = run_grep(
        serde_json::json!({ "pattern": "row", "max_results": 3 }),
        ws.path(),
    )
    .await;
    // Three hit lines (`row0`, `row1`, `row2`), then the truncation note.
    let hits = out.lines().filter(|l| l.contains(":row")).count();
    assert_eq!(hits, 3, "got: {out}");
    assert!(out.contains("[stopped at 3 results"), "got: {out}");
}

/// Grounds the permission gate's no-prompt refusal in real root containment:
/// a recursive search outside the workspace cannot be enabled by an fs grant.
#[tokio::test]
async fn grep_refuses_root_outside_workspace() {
    let parent = tempfile::TempDir::new().unwrap();
    std::fs::write(parent.path().join("outside.txt"), b"secret\n").unwrap();
    let ws = parent.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    for path in [
        "..".to_string(),
        parent.path().to_string_lossy().into_owned(),
    ] {
        for fs_read in [Scope::All, Scope::none()] {
            let caveats = Caveats {
                fs_read,
                ..caveats_rw(&ws)
            };
            let mut gate = MockGate::new(true, &caveats);
            let out = run_tool_gated(
                "grep",
                serde_json::json!({ "pattern": ".", "path": path }),
                &ws,
                &caveats,
                &mut gate,
            )
            .await;
            assert!(out.starts_with("capability denied"), "got: {out}");
            assert!(out.contains("workspace-only"), "got: {out}");
            assert!(!out.contains("request_permissions"), "got: {out}");
            assert!(gate.asks.is_empty(), "an unsupported root must not prompt");
        }
    }
}

/// `grep` is denied without an `fs_read` grant and is grantable only within
/// the workspace via the permission gate.
#[tokio::test]
async fn grep_denied_without_fs_read() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "secret.txt");
    std::fs::write(ws.path().join("secret.txt"), b"top secret\n").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "grep",
        serde_json::json!({ "pattern": "secret" }),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert!(out.starts_with("capability denied"), "got: {out}");
    let mut gate = MockGate::new(true, &denied);
    let out = run_tool_gated(
        "grep",
        serde_json::json!({ "pattern": "secret" }),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert!(out.contains("secret.txt:1:top secret"), "got: {out}");
    assert_eq!(
        gate.asks.len(),
        1,
        "in-workspace authority remains grantable"
    );
}

/// A file with one invalid UTF-8 byte is still searched (shown lossily), not
/// silently skipped: real source trees carry the odd Latin-1 byte.
#[tokio::test]
async fn grep_searches_a_file_with_invalid_utf8() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("a.txt"), b"caf\xe9\nNEEDLE here\n").unwrap();
    let out = run_grep(serde_json::json!({ "pattern": "NEEDLE" }), ws.path()).await;
    assert_eq!(out, "a.txt:2:NEEDLE here", "got: {out}");
}

/// A hit on line 9,000 of a 13,000-line file reports that line number.
#[tokio::test]
async fn grep_reports_the_right_line_in_a_large_file() {
    let ws = tempfile::TempDir::new().unwrap();
    let body = (1..=13_000)
        .map(|n| {
            if n == 9_000 {
                "NEEDLE".to_string()
            } else {
                format!("line {n}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(ws.path().join("big.rs"), body).unwrap();
    let out = run_grep(serde_json::json!({ "pattern": "^NEEDLE$" }), ws.path()).await;
    assert_eq!(out, "big.rs:9000:NEEDLE", "got: {out}");
}

/// A junk `max_results` fails loudly instead of silently using the default.
#[tokio::test]
async fn grep_rejects_junk_max_results() {
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    let out = run_grep(
        serde_json::json!({ "pattern": "x", "max_results": "lots" }),
        ws.path(),
    )
    .await;
    assert!(out.starts_with("error: grep: `max_results`"), "got: {out}");
}

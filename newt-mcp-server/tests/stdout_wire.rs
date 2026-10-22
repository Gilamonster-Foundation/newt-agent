//! Regression (#2980 review): the real `newt-mcp-server` binary's stdout is
//! the JSON-RPC wire. The file tools once ran through an executor that painted
//! its call/result display on fd 1, so every `tools/call` was preceded by
//! non-JSON lines.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn file_tool_calls_leave_only_json_lines_on_stdout() {
    let ws = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("ok.txt"), "ordinary content\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_newt-mcp-server"))
        .current_dir(ws.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for (id, name, args) in [
        (1, "read_file", r#"{"path":"ok.txt"}"#),
        (2, "list_dir", r#"{"path":"."}"#),
        (3, "grep", r#"{"pattern":"ordinary","path":"."}"#),
    ] {
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
        )
        .unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();

    let frames: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line:?}")))
        .collect();
    assert_eq!(frames.len(), 3, "{stdout}");
    assert!(
        frames[0].to_string().contains("ordinary content"),
        "{stdout}"
    );
}

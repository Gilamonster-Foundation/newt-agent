use super::*;

// The command-name-as-shell-command refusal is a tool execution boundary test.
// It exercises neither round limits, wire protocol nor event recording, so it
// stays separate instead of borrowing an unrelated behavior owner.

/// `run_command` called with a tool name as the first word must return a
/// corrective error message, not shell it through agent-bridle.
#[tokio::test]
async fn run_command_refuses_tool_name_as_shell_command() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    for tool in [
        "list_dir",
        "read_file",
        "write_file",
        "use_skill",
        "web_fetch",
    ] {
        let args = serde_json::json!({ "command": format!("{tool} some/path") });
        let out = execute_tool(
            "run_command",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            None,
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // where_is
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert!(
            out.contains("is a tool, not a shell command"),
            "expected corrective message for '{tool}', got: {out}"
        );
    }
}

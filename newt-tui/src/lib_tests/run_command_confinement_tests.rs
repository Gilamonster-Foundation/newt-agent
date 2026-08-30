use super::*;
use newt_core::agentic::execute_tool;
use newt_core::caveats::{Caveats, CountBound, Scope};

// Only the `#[cfg(unix)]` confinement tests below construct this guard; on
// Windows those tests are gated out, so gate the guard too or it trips the
// `-D dead-code` clippy wall on the Windows CI job.
#[cfg(unix)]
struct ShellEngineGuard(Option<String>);
#[cfg(unix)]
impl ShellEngineGuard {
    fn safe_subset() -> Self {
        let previous = std::env::var("NEWT_SHELL_ENGINE").ok();
        std::env::set_var("NEWT_SHELL_ENGINE", "safe-subset");
        Self(previous)
    }
}
#[cfg(unix)]
impl Drop for ShellEngineGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("NEWT_SHELL_ENGINE", value),
            None => std::env::remove_var("NEWT_SHELL_ENGINE"),
        }
    }
}

/// A `Caveats` granting exec for the given commands and full fs/read+write
/// (so the test's own file-survival assertions are not themselves confined),
/// otherwise read-only-ish. `exec` is `Scope::Only` of the named commands.
#[cfg(unix)]
fn caveats_exec_only(cmds: &[&str]) -> Caveats {
    Caveats {
        fs_read: Scope::All,
        fs_write: Scope::All,
        exec: Scope::Only(cmds.iter().map(|s| s.to_string()).collect()),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// An allow-listed external command runs under the confined shell. Built
/// against the agent-bridle env-seam branch (#783), the bridle ships the
/// REAL safe-subset shell (no stub): with `exec` granting `env` and fs
/// unrestricted (no Landlock), `env` runs and prints the environment.
#[cfg(unix)]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn run_command_allowed_external_succeeds() {
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _engine = ShellEngineGuard::safe_subset();
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_exec_only(&["env"]);
    let args = serde_json::json!({ "command": "env" });
    let out = execute_tool(
        "run_command",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
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
        !out.contains("capability denied") && !out.contains("unavailable in this build"),
        "an allow-listed external command must run, not be denied, got: {out}"
    );
    assert!(
        out.contains('='),
        "`env` must print KEY=VALUE environment lines, got: {out}"
    );
}

/// An out-of-scope command is DENIED by the real safe-subset shell (env-seam
/// branch, #783): `env` is not in the `echo`-only exec grant, so the confined
/// shell refuses it with a capability denial (not the old stub error).
#[cfg(unix)]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn run_command_out_of_scope_is_denied() {
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _engine = ShellEngineGuard::safe_subset();
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_exec_only(&["echo"]);
    let args = serde_json::json!({ "command": "env" });
    let out = execute_tool(
        "run_command",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
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
        out.contains("capability denied"),
        "an out-of-scope command must be denied by the confined shell, got: {out}"
    );
}

/// THE test that justifies the change. `echo ok && rm -r <victim>` under a
/// grant that allows `echo` but NOT `rm`: the `rm` is DENIED inside the
/// confined shell and the victim file SURVIVES. On the old leading-token +
/// `sh -c` path the `echo` check passed and `rm` then ran directly, deleting
/// the victim. Full-command confinement is what stops it here.
#[cfg(unix)]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn compound_command_denies_ungranted_rm_and_victim_survives() {
    // Serialize against env-mutating tests: run_command's confined shell
    // reads NEWT_VENV / VIRTUAL_ENV / NEWT_EXEC_PATHS via venv_cmd_prefix.
    let _env = crate::test_env_guard::env_read_guard_async().await;
    let ws = tempfile::TempDir::new().unwrap();
    let victim = ws.path().join("victim.txt");
    std::fs::write(&victim, b"do not delete me").unwrap();
    assert!(victim.exists(), "precondition: victim file exists");

    // Grant `echo` only — NOT `rm`.
    let caveats = caveats_exec_only(&["echo"]);
    let victim_str = victim.to_string_lossy();
    let args = serde_json::json!({
        "command": format!("echo ok && rm -r {victim_str}"),
    });
    let out = execute_tool(
        "run_command",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
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

    // The victim MUST survive: the `rm` never ran (leash denied the spawn).
    assert!(
        victim.exists(),
        "victim file must survive — the ungranted `rm` must be denied by the \
             confined shell (this would have slipped past the old leading-token \
             + `sh -c` path). run_command returned: {out}"
    );
}

/// read_file still enforces fs_read and returns contents (no regression
/// from the run_command rewrite).
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn read_file_still_works() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("a.txt"), b"hello").unwrap();
    let caveats = Caveats {
        fs_read: Scope::All,
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    };
    let args = serde_json::json!({ "path": "a.txt" });
    let out = execute_tool(
        "read_file",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
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
    assert_eq!(out, "hello", "read_file must still return file contents");
}

/// write_file still enforces fs_write and writes the file (no regression).
/// fs_write is scoped to the workspace (not `Scope::All`) so the y/N prompt
/// is skipped — the preset is the consent.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn write_file_still_works() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats {
        fs_read: Scope::All,
        fs_write: Scope::only([ws.path().to_string_lossy().into_owned()]),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    };
    let args = serde_json::json!({ "path": "b.txt", "content": "written" });
    let out = execute_tool(
        "write_file",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
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
        out.starts_with("wrote"),
        "write_file must succeed, got: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.path().join("b.txt")).unwrap(),
        "written"
    );
}

/// list_dir still enforces fs_read and lists entries (no regression).
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn list_dir_still_works() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("one.txt"), b"x").unwrap();
    std::fs::write(ws.path().join("two.txt"), b"y").unwrap();
    let caveats = Caveats {
        fs_read: Scope::All,
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    };
    let args = serde_json::json!({ "path": "." });
    let out = execute_tool(
        "list_dir",
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
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
    assert!(out.contains("one.txt") && out.contains("two.txt"));
}

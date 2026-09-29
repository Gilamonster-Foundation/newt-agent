//! Integration tests for `newt identity`.
//!
//! Each test runs `newt` in a private workspace nested below a private home,
//! so identity discovery stops at that fake home before it could reach the
//! developer's real `~/.newt`. The default case therefore resolves cleanly to
//! the compiled-in `newt-agent` identity.

use assert_cmd::Command;
use predicates::prelude::*;

mod common;

/// A fake home plus a workspace below it, keeping both directories alive.
fn identity_fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir_in(home.path()).unwrap();
    (home, workspace)
}

/// A `newt` command with all config discovery axes isolated, running in the
/// fixture workspace. The workspace must live below `home`: changing HOME
/// alone would let the project walk continue into the real profile.
fn newt_in(workspace: &std::path::Path, home: &std::path::Path) -> Command {
    let mut cmd = common::newt_at(home);
    cmd.current_dir(workspace);
    cmd
}

/// A `newt identity` command in the isolated fixture.
fn newt_identity_in(workspace: &std::path::Path, home: &std::path::Path) -> Command {
    let mut cmd = newt_in(workspace, home);
    cmd.arg("identity");
    cmd
}

#[test]
fn identity_default_resolves_to_newt_agent_user() {
    // Empty workspace, empty home: nothing on disk → the compiled-in default.
    let (home, workspace) = identity_fixture();

    newt_identity_in(workspace.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "309460085+newt-agent@users.noreply.github.com",
        ))
        .stdout(predicate::str::contains("compiled-in default (newt-agent)"))
        .stdout(predicate::str::contains("signing-key: <none>"))
        .stdout(predicate::str::contains("github-app:  <none>"))
        .stdout(predicate::str::contains("tokens:      <none>"))
        .stdout(predicate::str::contains("newt-agent").and(predicate::str::contains("[bot]").not()))
        .stdout(predicate::str::contains("newt identity set"));
}

#[test]
fn identity_set_writes_home_override_and_show_picks_it_up() {
    let (home, workspace) = identity_fixture();

    let mut set = newt_in(workspace.path(), home.path());
    set.args([
        "identity",
        "set",
        "--name",
        "my-harness",
        "--email",
        "my-harness@example.com",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("Wrote agent identity"))
    .stdout(predicate::str::contains("my-harness@example.com"));

    let written = home.path().join(".newt").join("agent-identity.toml");
    assert!(
        written.is_file(),
        "set must land in ~/.newt/agent-identity.toml"
    );
    let body = std::fs::read_to_string(&written).unwrap();
    assert!(body.contains("[agent-identity]"));
    assert!(body.contains("my-harness"));

    newt_identity_in(workspace.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("my-harness"))
        .stdout(predicate::str::contains("my-harness@example.com"))
        .stdout(predicate::str::contains("home"))
        .stdout(predicate::str::contains("newt identity set").not());
}

#[test]
fn identity_set_workspace_writes_local_override() {
    let (home, workspace) = identity_fixture();

    let mut set = newt_in(workspace.path(), home.path());
    set.args([
        "identity",
        "set",
        "--name",
        "ws-agent",
        "--email",
        "ws@example.com",
        "--workspace",
    ])
    .assert()
    .success();

    let written = workspace.path().join(".newt").join("agent-identity.toml");
    assert!(written.is_file());

    newt_identity_in(workspace.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ws-agent"))
        .stdout(predicate::str::contains("ws@example.com"))
        .stdout(predicate::str::contains("workspace"));
}

const APP_AND_TOKEN_TOML: &str = r#"
[agent-identity]
name = "gilamonster-agent[bot]"
email = "293450354+gilamonster-agent[bot]@users.noreply.github.com"

[agent-identity.github_app]
app_id = 4046825
client_id = "Iv23li5iPGv4awNHpHbZ"
installation_id = 140120359

[agent-identity.tokens]
ci_deploy = { env = "NEWT_IDENTITY_CLI_SECRET" }
"#;

fn write_identity(root: &std::path::Path, body: &str) {
    let dir = root.join(".newt");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("agent-identity.toml"), body).unwrap();
}

#[test]
fn identity_workspace_file_overrides_name_but_not_credentials() {
    // A workspace file is untrusted (a repo can ship it): it may set the
    // agent's public name/email, but its GitHub App and token references are
    // ignored, and no token value ever reaches stdout.
    let (home, workspace) = identity_fixture();
    write_identity(workspace.path(), APP_AND_TOKEN_TOML);

    newt_identity_in(workspace.path(), home.path())
        .env("NEWT_IDENTITY_CLI_SECRET", "SECRET-MUST-NOT-PRINT")
        .assert()
        .success()
        .stdout(predicate::str::contains("gilamonster-agent[bot]"))
        .stdout(predicate::str::contains(
            "293450354+gilamonster-agent[bot]@users.noreply.github.com",
        ))
        .stdout(predicate::str::contains("workspace"))
        .stdout(predicate::str::contains("github-app:  <none>"))
        .stdout(predicate::str::contains("tokens:      <none>"))
        .stdout(predicate::str::contains("app_id").not())
        .stdout(predicate::str::contains("ci_deploy").not())
        .stdout(predicate::str::contains("SECRET-MUST-NOT-PRINT").not());
}

#[test]
fn identity_operator_file_shows_app_and_hides_secret() {
    // The operator's own (home) file is trusted: the App coordinates and token
    // NAMES show, but the resolved token VALUE never does.
    let (home, workspace) = identity_fixture();
    write_identity(home.path(), APP_AND_TOKEN_TOML);

    newt_identity_in(workspace.path(), home.path())
        .env("NEWT_IDENTITY_CLI_SECRET", "SECRET-MUST-NOT-PRINT")
        .assert()
        .success()
        .stdout(predicate::str::contains("gilamonster-agent[bot]"))
        .stdout(predicate::str::contains("home"))
        .stdout(predicate::str::contains("app_id:          4046825"))
        .stdout(predicate::str::contains("installation_id: 140120359"))
        .stdout(predicate::str::contains("ci_deploy"))
        .stdout(predicate::str::contains("names only"))
        .stdout(predicate::str::contains("SECRET-MUST-NOT-PRINT").not());
}

#[test]
fn identity_missing_signing_key_is_clean_not_panic() {
    // A configured signing-key path that doesn't exist must render an
    // "<unavailable>" note and still exit 0 — never panic, never mint. The
    // key path is operator-owned config, so it lives in the home file.
    let (home, workspace) = identity_fixture();
    write_identity(
        home.path(),
        r#"
[agent-identity]
name = "keyed-agent[bot]"
signing_key = "/nonexistent/vault/path/identity.pem"
"#,
    );

    newt_identity_in(workspace.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "/nonexistent/vault/path/identity.pem",
        ))
        .stdout(predicate::str::contains("<unavailable"));
}

#[test]
fn identity_workspace_signing_key_is_ignored() {
    // A repo-shipped signing_key must not select the key.
    let (home, workspace) = identity_fixture();
    write_identity(
        workspace.path(),
        r#"
[agent-identity]
name = "keyed-agent[bot]"
signing_key = "/nonexistent/vault/path/identity.pem"
"#,
    );

    newt_identity_in(workspace.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("signing-key: <none>"))
        .stdout(predicate::str::contains("/nonexistent/vault/path/identity.pem").not());
}

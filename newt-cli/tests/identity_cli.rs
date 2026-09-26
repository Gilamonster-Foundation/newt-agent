//! Integration tests for `newt identity`.
//!
//! Each test runs `newt` in a private working directory with `HOME` pointed at
//! a separate empty tempdir, so resolution never reads the developer's real
//! `~/.newt`. Because the working directory and `$HOME` are distinct empty
//! trees, the default case resolves cleanly to the compiled-in
//! `newt-agent` identity.

use assert_cmd::Command;
use predicates::prelude::*;

/// A `newt identity` command with `HOME` and cwd isolated to fresh tempdirs.
/// Returns the command plus the two tempdirs (kept alive for the test).
fn newt_identity_in(workspace: &std::path::Path, home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("newt").unwrap();
    cmd.current_dir(workspace).env("HOME", home).arg("identity");
    cmd
}

#[test]
fn identity_default_resolves_to_newt_agent_user() {
    // Empty workspace, empty home: nothing on disk → the compiled-in default.
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

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
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let mut set = Command::cargo_bin("newt").unwrap();
    set.current_dir(workspace.path())
        .env("HOME", home.path())
        .args([
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
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let mut set = Command::cargo_bin("newt").unwrap();
    set.current_dir(workspace.path())
        .env("HOME", home.path())
        .args([
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
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
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
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
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
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
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
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
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

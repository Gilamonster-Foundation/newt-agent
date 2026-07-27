//! Integration tests for `newt doctor` against mocked HTTP backends.
//!
//! Backend and DGX probes are pointed at `wiremock` servers so the assertions
//! exercise the real probe code paths (`/api/tags` for Ollama-flavored
//! endpoints, `/v1/models` for vLLM) without any live service. `HOME` and the
//! working directory are redirected to tempdirs so MCP discovery never reads
//! the developer's real `~/.claude.json` / `./.mcp.json`.

use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Write a config TOML to a tempfile the test owns.
fn write_config(contents: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

/// `newt doctor --config <f>` isolated from the developer's environment:
/// fake `$HOME`, tempdir cwd (no `./.mcp.json`), no DGX env overrides, and
/// an unreachable `OLLAMA_HOST` for the discovery step.
fn doctor(config: &tempfile::NamedTempFile, home: &tempfile::TempDir) -> Command {
    let mut cmd = Command::cargo_bin("newt").unwrap();
    cmd.env("HOME", home.path())
        .env("OLLAMA_HOST", "http://127.0.0.1:1")
        .env_remove("NEWT_DGX_OLLAMA_URL")
        .env_remove("NEWT_DGX_OLLAMA_LB_URL")
        .env_remove("NEWT_DGX_IN_CLUSTER_URL")
        .env_remove("NEWT_DGX_VLLM_URL")
        .env_remove("NEWT_DGX_HOST")
        .env_remove("NEWT_DGX_MODEL")
        .current_dir(home.path())
        .args(["--config", config.path().to_str().unwrap(), "doctor"]);
    cmd
}

fn path_with_front(front: &Path) -> std::ffi::OsString {
    let mut paths = vec![front.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).unwrap()
}

fn write_provider_probe(dir: &Path, name: &str, exit_code: i32) -> String {
    let command_name = provider_probe_name(name);
    let path = dir.join(&command_name);
    write_provider_probe_file(&path, exit_code);
    command_name
}

#[cfg(windows)]
fn provider_probe_name(name: &str) -> String {
    format!("{name}.cmd")
}

#[cfg(not(windows))]
fn provider_probe_name(name: &str) -> String {
    name.to_string()
}

#[cfg(windows)]
fn write_provider_probe_file(path: &PathBuf, exit_code: i32) {
    std::fs::write(path, format!("@echo off\r\nexit /b {exit_code}\r\n")).unwrap();
}

#[cfg(not(windows))]
fn write_provider_probe_file(path: &PathBuf, exit_code: i32) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, format!("#!/bin/sh\nexit {exit_code}\n")).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_reports_backend_and_provider_statuses() {
    let ok_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
        .expect(1)
        .mount(&ok_server)
        .await;

    let err_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&err_server)
        .await;

    // #1212: an openai-kind backend must be probed via /v1/models (it
    // false-404'd via /api/tags before the fix — this mock serves ONLY the
    // openai surface, so the old probe path fails loudly here).
    let openai_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"data": [{"id": "test-model"}]})),
        )
        .expect(1)
        .mount(&openai_server)
        .await;

    let provider_dir = tempfile::tempdir().unwrap();
    let present_command = write_provider_probe(provider_dir.path(), "newt-provider-present", 0);
    let broken_command = write_provider_probe(provider_dir.path(), "newt-provider-broken", 1);

    let config = write_config(&format!(
        r#"
[[backends]]
name = "good"
endpoint = "{ok}"
model = "m"
tiers = ["FAST"]

[[backends]]
name = "bad-status"
endpoint = "{err}"
model = "m"
tiers = ["FAST"]

[[backends]]
name = "vllm-like"
endpoint = "{openai}"
model = "test-model"
kind = "openai"
tiers = ["FAST"]

[[backends]]
name = "down"
endpoint = "http://127.0.0.1:1"
model = "m"
tiers = ["FAST"]

default_tier_order = ["FAST"]

[[providers]]
name = "present"
command = "{present_command}"
tiers = ["FAST"]

[[providers]]
name = "broken"
command = "{broken_command}"
tiers = ["FAST"]

[[providers]]
name = "absent"
command = "newt-test-no-such-binary"
tiers = ["FAST"]
"#,
        ok = ok_server.uri(),
        err = err_server.uri(),
        openai = openai_server.uri(),
        present_command = present_command,
        broken_command = broken_command,
    ));
    let home = tempfile::tempdir().unwrap();

    let mut cmd = doctor(&config, &home);
    cmd.env("PATH", path_with_front(provider_dir.path()));

    cmd.assert()
        .success()
        // Absent kind → label "auto" and detect_endpoint (race tags vs models).
        .stdout(predicate::str::contains(format!(
            "good ({}, auto) — OK (detected ollama, 0 model(s) served)",
            ok_server.uri()
        )))
        .stdout(predicate::str::contains(format!(
            "vllm-like ({}, openai) — OK (1 model(s) served)",
            openai_server.uri()
        )))
        .stdout(predicate::str::contains(format!(
            "bad-status ({}, auto) — unreachable: unsupported inference endpoint {}",
            err_server.uri(),
            err_server.uri()
        )))
        .stdout(predicate::str::contains(
            "down (http://127.0.0.1:1, auto) — unreachable: unreachable inference endpoint",
        ))
        .stdout(predicate::str::contains(format!(
            "present (command: {present_command}) — found on PATH"
        )))
        .stdout(predicate::str::contains(format!(
            "broken (command: {broken_command}) — found but exited with error"
        )))
        .stdout(predicate::str::contains(
            "absent (command: newt-test-no-such-binary) — not found on PATH",
        ))
        .stdout(predicate::str::contains("DGX nodes:"))
        .stdout(predicate::str::contains("(none configured)"))
        .stdout(predicate::str::contains("(none discovered)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_probes_dgx_nodes_and_marks_active() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
        .mount(&server)
        .await;
    // The vLLM flavor must be probed via the OpenAI-compatible route.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
        .expect(1)
        .mount(&server)
        .await;

    let config = write_config(&format!(
        r#"
[[backends]]
name = "x"
endpoint = "http://127.0.0.1:1"
model = "m"
tiers = ["FAST"]

default_tier_order = ["FAST"]

[dgx]
active_node = "spark"
active_model = "qwen3:32b"

[[dgx.nodes]]
name = "spark"
ollama = "{uri}"
vllm = "{uri}"

[[dgx.nodes]]
name = "bare"
"#,
        uri = server.uri(),
    ));
    let home = tempfile::tempdir().unwrap();

    doctor(&config, &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("spark [active node]"))
        // The active endpoint flavor carries the `*` marker and probes OK.
        .stdout(predicate::str::contains(format!(
            "ollama ({}) * — OK",
            server.uri()
        )))
        // vLLM is probed via /v1/models (no active marker).
        .stdout(predicate::str::contains(format!(
            "vllm ({}) — OK",
            server.uri()
        )))
        // The endpoint-less node is listed but not active.
        .stdout(predicate::str::contains("bare"))
        .stdout(predicate::str::contains("bare [active node]").not())
        // Active resolution line: model @ flavor → URL.
        .stdout(predicate::str::contains(format!(
            "Active: qwen3:32b @ ollama → {}",
            server.uri()
        )));
}

#[test]
fn doctor_dgx_env_override_without_nodes() {
    let config = write_config(
        r#"
[[backends]]
name = "x"
endpoint = "http://127.0.0.1:1"
model = "m"
tiers = ["FAST"]

default_tier_order = ["FAST"]

[dgx]
"#,
    );
    let home = tempfile::tempdir().unwrap();

    let mut cmd = doctor(&config, &home);
    cmd.env("NEWT_DGX_OLLAMA_URL", "http://127.0.0.1:9999");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains(
            "(no nodes — using env overrides only)",
        ))
        .stdout(predicate::str::contains(
            "Active: (none) @ ollama → http://127.0.0.1:9999",
        ));
}

#[test]
fn doctor_dgx_unresolved_when_unconfigured() {
    let config = write_config(
        r#"
[[backends]]
name = "x"
endpoint = "http://127.0.0.1:1"
model = "m"
tiers = ["FAST"]

default_tier_order = ["FAST"]

[dgx]
"#,
    );
    let home = tempfile::tempdir().unwrap();

    doctor(&config, &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("Active endpoint: unresolved"))
        .stdout(predicate::str::contains("no DGX nodes configured"));
}

#[test]
fn doctor_connects_lists_and_skips_mcp_servers() {
    // `newt mcp` itself is the stdio server under test — doctor must connect,
    // initialize, and list its tools end-to-end. A broken stdio entry must
    // surface as ERROR; an http entry must be reported as skipped.
    let newt_bin = assert_cmd::cargo::cargo_bin("newt");
    let config = write_config(&format!(
        r#"
[[backends]]
name = "x"
endpoint = "http://127.0.0.1:1"
model = "m"
tiers = ["FAST"]

default_tier_order = ["FAST"]

[[mcp_servers]]
name = "self"
# TOML literal string: the path is a Windows path on Windows CI and
# backslashes must not be treated as escapes.
command = '{newt}'
args = ["mcp"]

[[mcp_servers]]
name = "dead"
command = "true"

[[mcp_servers]]
name = "remote"
type = "http"
url = "http://127.0.0.1:1/mcp"
"#,
        newt = newt_bin.display(),
    ));
    let home = tempfile::tempdir().unwrap();

    doctor(&config, &home)
        .assert()
        .success()
        .stdout(predicate::str::contains("self [stdio] — OK"))
        .stdout(predicate::str::contains("code_read"))
        .stdout(predicate::str::contains("dead [stdio] — ERROR:"))
        .stdout(predicate::str::contains(
            "remote [http] — http://127.0.0.1:1/mcp (skipped: only stdio is supported in this build)",
        ));
}

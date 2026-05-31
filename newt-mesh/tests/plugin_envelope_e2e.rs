//! End-to-end test for the phase-1c plugin-envelope handshake.
//!
//! Wires the three pieces together in one process:
//!
//! 1. Mint a parent [`AgentKey`].
//! 2. Build an envelope via [`serialize_for_plugin`] that attenuates the
//!    parent's authority.
//! 3. Configure a [`ProviderPluginBackend`] with that envelope and a
//!    `bash` echo command that reads the `NEWT_AGENT_KEY` env var.
//! 4. Spawn the subprocess via `spawn_command()`, capture stdout, and
//!    assert that (a) the envelope reached the subprocess unchanged, and
//!    (b) the round-tripped chain verifies + produces the same caveats
//!    we delegated.
//!
//! This is the load-bearing proof that the host-side wire glue carries
//! an attenuated cert chain across the process boundary.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use agent_mesh_core::{AgentKey, AgentMetadata, Caveats as AmCaveats, CountBound, Scope, UserKey};
use newt_core::router::Tier;
use newt_inference::provider_plugin::ProviderPluginBackend;
use newt_mesh::plugin_envelope::{caveats_from_envelope, serialize_for_plugin};
use plugins_protocol::AGENT_KEY_ENV;

fn fixture_metadata(role: &str, caveats: AmCaveats) -> AgentMetadata {
    AgentMetadata {
        role: role.to_string(),
        host: "test-host".to_string(),
        capabilities: vec!["test".to_string()],
        issued_at: "2026-05-31T00:00:00Z".to_string(),
        expires_at: None,
        caveats,
    }
}

/// Write an echo script that prints `$NEWT_AGENT_KEY` to stdout. Used by
/// the end-to-end subprocess test to confirm the env var crossed the
/// process boundary unchanged.
fn write_env_echo_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("echo-agent-key");
    // Print the env var on its own line; using `printenv` keeps us free
    // of any quoting hazards a `echo "$VAR"` would introduce on
    // pathological values.
    fs::write(&script, "#!/usr/bin/env bash\nprintenv NEWT_AGENT_KEY\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[tokio::test]
async fn envelope_threads_through_subprocess_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_env_echo_script(tmp.path());

    // 1. Parent agent under unrestricted authority.
    let user = UserKey::generate();
    let parent = AgentKey::issue(&user, fixture_metadata("parent", AmCaveats::top()));

    // 2. Attenuate authority for the plugin: exec={git, cargo}, no net,
    //    at most 4 inference calls per session.
    let child_caveats = AmCaveats {
        exec: Scope::only(["git".to_string(), "cargo".to_string()]),
        net: Scope::none(),
        max_calls: CountBound::AtMost(4),
        ..AmCaveats::top()
    };
    let envelope =
        serialize_for_plugin(&parent, fixture_metadata("child", child_caveats.clone())).unwrap();

    // 3. ProviderPluginBackend stores the envelope opaquely; the spawn
    //    command threads it through as NEWT_AGENT_KEY.
    let backend = ProviderPluginBackend::new(
        "stub-provider",
        script.to_string_lossy().into_owned(),
        "stub-model",
        vec![Tier::Fast],
    )
    .with_agent_key_envelope(&envelope);
    assert_eq!(backend.agent_key_envelope(), Some(envelope.as_str()));

    // 4. Spawn, wait, capture stdout.
    let output = backend.spawn_command().output().await.unwrap();
    assert!(
        output.status.success(),
        "stub script exited {:?}",
        output.status
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let echoed = stdout.trim_end_matches('\n');
    assert_eq!(
        echoed, envelope,
        "env var must reach the subprocess byte-for-byte"
    );

    // 5. Plugin-side verification: the very same string the host minted
    //    must verify and produce the attenuated caveats we asked for.
    let extracted = caveats_from_envelope(echoed).unwrap();
    assert!(extracted.permits_exec("git"));
    assert!(extracted.permits_exec("cargo"));
    assert!(!extracted.permits_exec("rm"));
    assert!(!extracted.permits_net("openai.com"));
    assert!(extracted.max_calls.permits_one_more(3));
    assert!(!extracted.max_calls.permits_one_more(4));
}

#[tokio::test]
async fn missing_envelope_strips_inherited_env_var() {
    // If a backend has no envelope configured, spawn_command must
    // *clear* any inherited NEWT_AGENT_KEY rather than passing it
    // through — otherwise a confused host that itself has the env var
    // set would silently leak its authority to a plugin that the
    // application code did not bless.
    let tmp = tempfile::tempdir().unwrap();
    let script = write_env_echo_script(tmp.path());

    // Set the env var in the parent process, then build a backend
    // *without* calling with_agent_key_envelope.
    std::env::set_var(AGENT_KEY_ENV, "inherited-must-not-leak");
    let backend = ProviderPluginBackend::new(
        "stub-provider",
        script.to_string_lossy().into_owned(),
        "stub-model",
        vec![Tier::Fast],
    );

    let output = backend.spawn_command().output().await.unwrap();
    // `printenv VAR` exits non-zero when the variable is unset, but
    // produces empty stdout. Either way, the inherited value must NOT
    // reach the child.
    let stdout = String::from_utf8(output.stdout).unwrap();
    let echoed = stdout.trim_end_matches('\n');
    assert_ne!(
        echoed, "inherited-must-not-leak",
        "inherited NEWT_AGENT_KEY must not leak to subprocess when no envelope is set"
    );

    // Clean up. The env-set above is process-global so leaving it set
    // could flake other tests in this binary.
    std::env::remove_var(AGENT_KEY_ENV);
}

//! Exact MCP child authority, using real canonical resources and no subprocess.

use super::*;
use newt_core::workspace_protection::WorkspaceProtection;
use newt_core::Scope;

struct Fixture {
    _temp: tempfile::TempDir,
    #[cfg(feature = "confine")]
    protected: std::path::PathBuf,
    program: std::path::PathBuf,
    base: Caveats,
    guard: WorkspaceProtection,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let protected = root.join("operator");
        let public = root.join("public-bin");
        for directory in [&workspace, &protected, &public] {
            std::fs::create_dir(directory).unwrap();
        }
        let program = public.join("server");
        std::fs::write(&program, b"server fixture").unwrap();
        let guard = WorkspaceProtection::new(
            std::slice::from_ref(&protected),
            std::slice::from_ref(&protected),
        )
        .unwrap();
        let base = Caveats {
            fs_read: Scope::only([workspace.to_string_lossy().into_owned()]),
            fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
            exec: Scope::none(),
            net: Scope::none(),
            ..Caveats::top()
        };
        guard.validate_caveats(&base).unwrap();
        Self {
            _temp: temp,
            #[cfg(feature = "confine")]
            protected,
            program,
            base,
            guard,
        }
    }
}

#[cfg(feature = "confine")]
#[test]
fn prepared_stdio_rejects_the_configured_commands_protected_read() {
    let f = Fixture::new();
    let secret_command = f.protected.join("private-server");
    std::fs::write(&secret_command, b"private server fixture").unwrap();
    assert!(
        prepare_stdio_caveats(
            &f.base,
            secret_command.to_str().unwrap(),
            Some(&f.guard),
            Some(&SandboxRoots::from(&agent_bridle::SandboxPolicy::default()))
        )
        .is_err(),
        "the configured command changes actual executable-read authority after session admission"
    );
}

#[cfg(all(not(windows), feature = "confine"))]
#[test]
fn prepared_stdio_validates_its_actual_implicit_read_policy() {
    let f = Fixture::new();
    let mut policy = agent_bridle::SandboxPolicy::default();
    policy
        .base_read_paths
        .extra
        .push(f.protected.to_string_lossy().into_owned());
    assert!(
        prepare_stdio_caveats(
            &f.base,
            f.program.to_str().unwrap(),
            Some(&f.guard),
            Some(&SandboxRoots::from(&policy))
        )
        .is_err(),
        "the launcher's implicit read roots must be checked, not another launcher's defaults"
    );
}

#[test]
fn protected_stdio_refuses_advisory_transport_but_preserves_legacy_selection() {
    let f = Fixture::new();
    assert!(
        prepare_stdio_caveats(&f.base, f.program.to_str().unwrap(), Some(&f.guard), None).is_err(),
        "protected sessions require actual local process confinement"
    );
    assert!(
        prepare_stdio_caveats(&f.base, f.program.to_str().unwrap(), None, None).is_ok(),
        "legacy advisory behavior remains explicit and unchanged"
    );
}

#[cfg(feature = "confine")]
#[test]
fn lawful_prepared_stdio_preserves_each_other_authority_axis() {
    let f = Fixture::new();
    let prepared = prepare_stdio_caveats(
        &f.base,
        f.program.to_str().unwrap(),
        Some(&f.guard),
        Some(&SandboxRoots::from(&agent_bridle::SandboxPolicy::default())),
    )
    .unwrap();
    assert_eq!(
        prepared,
        spawn_caveats(&f.base, f.program.to_str().unwrap())
    );
    assert_eq!(prepared.fs_read, f.base.fs_read);
    assert_eq!(prepared.fs_write, f.base.fs_write);
    assert_eq!(prepared.net, f.base.net);
    assert_eq!(prepared.max_calls, f.base.max_calls);
}

/// Grounds the shared prepared-authority guard in the real confined stdio
/// spawn. The control launches the same symlinked executable and writes a
/// workspace marker; protected admission must prevent that launch entirely.
#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "confine"))]
#[tokio::test]
#[ignore = "real confined stdio subprocess; filesystem integration tier"]
async fn protected_stdio_refuses_before_native_process_effects() {
    use newt_core::mcp::{McpServerEntry, McpTrust, TransportKind};
    let mut f = Fixture::new();
    f.base.net = Scope::All;
    let alias = f.protected.join("server");
    std::os::unix::fs::symlink("/bin/sh", &alias).unwrap();
    let marker = f
        ._temp
        .path()
        .canonicalize()
        .unwrap()
        .join("workspace/spawned");
    let entry = McpServerEntry {
        name: "protected-stdio-fixture".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some(alias.to_string_lossy().into_owned()),
        args: vec![
            "-c".into(),
            format!("printf spawned > '{}'; printf 'done\\n'", marker.display()),
        ],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: McpTrust::Trusted,
        origin: None,
    };
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    assert!(
        StdioTransport::spawn_with_protection(&admitted, &f.base, Some(&f.guard)).is_err(),
        "protected executable aliases must be refused before spawning"
    );
    assert!(!marker.exists());
    let mut control = StdioTransport::spawn(&admitted, &f.base).unwrap();
    assert_eq!(
        control.stdout.next_line().await.unwrap().as_deref(),
        Some("done")
    );
    assert_eq!(std::fs::read(&marker).unwrap(), b"spawned");
}

/// The advisory transport refuses a workspace-protected session before any process exists:
/// it has no sandbox substrate to validate the session's authority against. The command below
/// does not exist, so had a spawn been tried first, the error would be the spawn's. No process
/// runs here; the confined transport's refusal is grounded on a real child by
/// `protected_stdio_refuses_before_native_process_effects`.
#[cfg(not(all(unix, feature = "confine")))]
#[test]
fn the_advisory_arm_refuses_a_protected_session_before_any_process_exists() {
    use newt_core::mcp::{McpServerEntry, McpTrust, TransportKind};
    // Resolving the guarded path reads the filesystem and creates nothing: it is never made.
    let guard =
        WorkspaceProtection::new(&[std::env::temp_dir().join("newt-advisory-guard")], &[]).unwrap();
    let entry = McpServerEntry {
        name: "advisory-protected".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("/nonexistent/newt-never-run".into()),
        args: Vec::new(),
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: McpTrust::Trusted,
        origin: None,
    };
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    let error =
        match StdioTransport::spawn_with_protection(&admitted, &Caveats::top(), Some(&guard)) {
            Ok(_) => panic!("a protected session must be refused by the advisory transport"),
            Err(error) => error,
        };
    let text = format!("{error:#}");
    assert!(
        text.contains("requires native process confinement"),
        "{text}"
    );
    assert!(
        !text.contains("spawning MCP server"),
        "a spawn was attempted before the refusal: {text}"
    );
}

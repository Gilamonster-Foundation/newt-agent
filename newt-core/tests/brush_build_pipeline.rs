//! Real Brush build-pipeline regression. Unlike unit tests, this executable
//! links the production core and serves its authenticated worker re-execs.
//!
//! Real-resource tier (installed Rust toolchain and native filesystem fence):
//! `cargo test -p newt-core --test brush_build_pipeline -- --ignored`
//! No inference service, installed Newt, or operator configuration is used.

fn main() {
    // This MUST precede fixture setup: a confined worker cannot initialize an
    // ambient test runner or mint its own fixture authority.
    if let Some(code) = newt_core::maybe_dispatch() {
        std::process::exit(code);
    }
    newt_core::answer_nextest_listing("brush_build_pipeline");
    #[cfg(target_os = "linux")]
    if std::env::args().any(|arg| arg == "--build-loopback-probe") {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        let automatic = std::env::args().any(|arg| arg == "--build-network-auto");
        if automatic && TcpListener::bind(("127.0.0.1", 0)).is_err() {
            for host in ["127.0.0.1", "127.23.45.67", "::1"] {
                assert_eq!(
                    TcpListener::bind((host, 0)).unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
            }
            println!("BUILD_LOOPBACK_DENIED_FALLBACK");
            return;
        }
        if automatic {
            let interfaces = std::fs::read_to_string("/proc/net/dev").unwrap();
            let names: Vec<_> = interfaces
                .lines()
                .filter_map(|line| line.split_once(':'))
                .map(|(name, _)| name.trim())
                .collect();
            assert_eq!(names, ["lo"], "private namespace must expose only loopback");
        }
        for host in ["127.0.0.1", "127.23.45.67", "::1"] {
            let listener = TcpListener::bind((host, 0))
                .unwrap_or_else(|error| panic!("loopback bind {host}: {error}"));
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            client.write_all(b"ping").unwrap();
            let (mut server, _) = listener.accept().unwrap();
            server
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut payload = [0; 4];
            server.read_exact(&mut payload).unwrap();
            assert_eq!(&payload, b"ping");
        }
        println!("BUILD_LOOPBACK_ROUNDTRIP_CONFIRMED");
        return;
    }
    #[cfg(target_os = "linux")]
    if std::env::args().any(|arg| arg == "--build-external-connect-probe") {
        let private = std::net::TcpListener::bind(("127.0.0.1", 0)).is_ok();
        for address in ["192.0.2.1:9", "[2001:db8::1]:9"] {
            let error = std::net::TcpStream::connect_timeout(
                &address.parse().unwrap(),
                std::time::Duration::from_secs(1),
            )
            .expect_err("external connect must remain denied");
            if private {
                assert!(
                    matches!(
                        error.raw_os_error(),
                        Some(libc::ENETUNREACH | libc::EHOSTUNREACH)
                    ),
                    "{address}: {error}"
                );
            } else {
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "{address}: {error}"
                );
            }
        }
        println!("BUILD_EXTERNAL_CONNECT_DENIED");
        return;
    }
    #[cfg(target_os = "linux")]
    if let Some(fd) = std::env::args().find_map(|arg| {
        arg.strip_prefix("--inherited-socket=")
            .and_then(|fd| fd.parse::<i32>().ok())
    }) {
        let mut kind = 0;
        let mut size = std::mem::size_of_val(&kind) as libc::socklen_t;
        // SAFETY: writable locals of the stated size; a closed fd returns EBADF.
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_TYPE,
                    (&mut kind as *mut i32).cast(),
                    &mut size,
                )
            },
            -1,
            "host socket must not survive the confined spawn"
        );
        println!("BUILD_INHERITED_SOCKET_DENIED");
        return;
    }
    #[cfg(unix)]
    if std::env::args().any(|arg| arg == "--build-unix-socket-probe") {
        use std::io::{Read, Write};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::net::{UnixListener, UnixStream};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("local.sock");
        let listener = UnixListener::bind(&path).unwrap_or_else(|error| {
            panic!(
                "Unix socket bind failed at {} path bytes: {error}",
                path.as_os_str().as_bytes().len()
            )
        });
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"ping").unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut payload = [0; 4];
        server.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"ping");
        std::fs::write(
            "socket-scratch-path",
            std::env::temp_dir().as_os_str().as_bytes(),
        )
        .unwrap();
        println!(
            "BUILD_UNIX_SOCKET_ROUNDTRIP_CONFIRMED ({} path bytes)",
            path.as_os_str().as_bytes().len()
        );
        return;
    }
    #[cfg(unix)]
    if std::env::args().any(|arg| arg == "--build-pty-child") {
        println!("BUILD_PTY_ROUNDTRIP_CONFIRMED");
        return;
    }
    #[cfg(unix)]
    if std::env::args().any(|arg| arg == "--build-pty-probe") {
        if let Some(other) = std::env::args().nth(2) {
            for (read, write) in [(true, false), (false, true), (true, true)] {
                let error = std::fs::OpenOptions::new()
                    .read(read)
                    .write(write)
                    .open(&other)
                    .expect_err("an unrelated host terminal must stay outside the grant");
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            }
        }
        let pty = tests_pty::Pty::open();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--build-pty-child")
            .stdin(pty.slave_stdio())
            .stdout(pty.slave_stdio())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(pty
            .screen_to_eof()
            .contains("BUILD_PTY_ROUNDTRIP_CONFIRMED"));
        println!("BUILD_PTY_TEST_PASSED");
        return;
    }
    if std::env::args().any(|arg| arg == "--scratch-workspace-keys") {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_key = newt_core::workspace_key::workspace_key_v2(first.path()).unwrap();
        let second_key = newt_core::workspace_key::workspace_key_v2(second.path()).unwrap();
        assert_eq!(
            first_key,
            newt_core::workspace_key::workspace_key_v2(first.path()).unwrap()
        );
        assert_ne!(
            first_key, second_key,
            "unrelated temporary fixtures inherited the build repository's Git identity"
        );
        assert_ne!(
            first_key,
            newt_core::workspace_key::workspace_key_v2(".").unwrap()
        );
        std::fs::write(
            "fixture-scratch-path",
            std::env::temp_dir().to_string_lossy().as_bytes(),
        )
        .unwrap();
        println!("NON_GIT_TEMP_FIXTURES_DISTINCT");
        return;
    }
    if !std::env::args().any(|arg| arg == "--ignored") {
        eprintln!(
            "test brush_build_pipeline ... ignored (real toolchain and native filesystem fence)"
        );
        return;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    native::run();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    panic!("real Brush private control is unsupported on this target; native parity remains unverified");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "support/cleanup_authority.rs"]
mod cleanup_authority;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "support/linked_build_shell.rs"]
mod linked_build_shell;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "support/native_git_diff.rs"]
mod native_git_diff;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "support/workspace_policy.rs"]
mod workspace_policy;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod native {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use newt_core::agentic::{execute_tool_with_offload, SpillCid};
    use newt_core::{execute_tool, Caveats, NoMcp, Scope, SessionSpillStore, SpillStore};
    use newt_policy::{
        DenialKind, HumanQuestionOutcome, PermissionDecision, PermissionGate, PermissionRequest,
    };

    const MARKER: &str = "BRUSH_BUILD_PIPELINE_CONFIRMED";

    /// A previously approved workspace Build grant is applied only to the exact
    /// projected invocation. It never changes the shell's standing caveats.
    struct CachedBuildGate {
        workspace: String,
        allow: bool,
        queries: usize,
    }

    impl PermissionGate for CachedBuildGate {
        fn ask_dependency_fetch(
            &mut self,
            _prepared: &newt_policy::caveats::Caveats,
            _request: &newt_policy::permissions::PermissionRequest,
        ) -> newt_policy::permissions::PermissionDecision {
            newt_policy::permissions::PermissionDecision::Deny
        }
        fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
            panic!("build projection must retain the invocation baseline");
        }

        fn ask_with_caveats(
            &mut self,
            baseline: &Caveats,
            requests: &[PermissionRequest],
        ) -> PermissionDecision {
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].kind, DenialKind::Build);
            assert_eq!(requests[0].target, self.workspace);
            assert_eq!(
                baseline.net,
                Scope::All,
                "retain the existing network grant"
            );
            assert_eq!(
                baseline.exec,
                Scope::All,
                "Build covers compiler descendants"
            );
            assert!(matches!(baseline.fs_read, Scope::Only(_)));
            assert!(matches!(baseline.fs_write, Scope::Only(_)));
            self.queries += 1;
            if self.allow {
                PermissionDecision::Allow(baseline.clone())
            } else {
                PermissionDecision::Deny
            }
        }

        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
    }

    pub fn run() {
        #[cfg(target_os = "linux")]
        if std::env::args().any(|arg| arg == "--deny-build-netns") {
            assert!(
                !std::env::args().any(|arg| arg == "--require-build-netns"),
                "cannot both deny and require namespace setup"
            );
            // Test-only fault injection, before starting any runtime threads:
            // descendants inherit EACCES for unshare on permissive hosts too.
            // This is not a production namespace policy or an escape filter.
            let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
            let mut filters = [
                instruction((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0, 0, 0),
                instruction(
                    (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                    0,
                    1,
                    libc::SYS_unshare as u32,
                ),
                instruction(
                    (libc::BPF_RET | libc::BPF_K) as u16,
                    0,
                    0,
                    libc::SECCOMP_RET_ERRNO | libc::EACCES as u32,
                ),
                instruction(
                    (libc::BPF_RET | libc::BPF_K) as u16,
                    0,
                    0,
                    libc::SECCOMP_RET_ALLOW,
                ),
            ];
            let program = libc::sock_fprog {
                len: filters.len() as u16,
                filter: filters.as_mut_ptr(),
            };
            // SAFETY: the kernel copies this live filter during prctl. Offset
            // zero is seccomp_data.nr; all other syscalls remain allowed here.
            unsafe {
                assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
                assert_eq!(
                    libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program),
                    0
                );
            }
            // unshare(0) changes nothing. This control must observe the injected
            // errno, independently of the host's user-namespace restrictions.
            assert_eq!(unsafe { libc::unshare(0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EACCES)
            );
            eprintln!("BUILD_NETNS_SETUP_FAILURE_INJECTED");
        }
        #[cfg(target_os = "macos")]
        let native_fence = agent_bridle::seatbelt_is_supported();
        #[cfg(target_os = "linux")]
        let native_fence = newt_core::confined_exec::kernel_fs_fence_available();
        assert!(
            native_fence,
            "this explicitly selected real-resource test requires a native filesystem fence"
        );
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        isolate(&root);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            #[cfg(target_os = "linux")]
            if selected("cached_git_dependency_builds_offline") {
                cached_git_dependency_builds_offline(&root).await;
            }
            if selected("linked_worktree_build_reads") {
                super::linked_build_shell::run().await;
            }
            if selected("saved_workspace_profile_confines_native_commands") {
                super::workspace_policy::run(&root).await;
            }
            if selected("native_cleanup_uses_scoped_authority") {
                super::cleanup_authority::run(&root).await;
            }
            if selected("native_git_diff_keeps_ordinary_semantics") {
                super::native_git_diff::run(&root).await;
            }
            if selected("cached_build_runs_the_original_pipeline_once") {
                cached_build_runs_the_original_pipeline_once(&root).await;
            }
            if selected("timeout_build_is_admitted_before_the_original_compound") {
                timeout_build_is_admitted_before_the_original_compound(&root).await;
            }
            if selected("build_temp_fixtures_do_not_inherit_repository_identity") {
                build_temp_fixtures_do_not_inherit_repository_identity(&root).await;
            }
            if selected("build_tests_can_use_a_private_pty") {
                build_tests_can_use_a_private_pty(&root).await;
            }
            if selected("build_tests_can_bind_a_unix_socket_in_a_tempdir") {
                build_tests_can_bind_a_unix_socket_in_a_tempdir(&root).await;
            }
            #[cfg(target_os = "linux")]
            for (name, probe, marker) in [
                (
                    "build_tests_external_connect_stays_denied",
                    "--build-external-connect-probe",
                    "BUILD_EXTERNAL_CONNECT_DENIED",
                ),
                (
                    "build_tests_loopback_roundtrip",
                    "--build-loopback-probe",
                    "BUILD_LOOPBACK_ROUNDTRIP_CONFIRMED",
                ),
                (
                    "build_tests_inherited_host_socket_denied",
                    "--inherited-socket=200",
                    "BUILD_INHERITED_SOCKET_DENIED",
                ),
            ] {
                if selected(name) {
                    build_network_probe(&root, name, probe, marker).await;
                }
            }
            if selected("denied_build_runs_no_pipeline_stage_or_redirection") {
                denied_build_runs_no_pipeline_stage_or_redirection(&root).await;
            }
            if selected("cached_build_formats_with_native_cargo_subcommand") {
                cached_build_formats_with_native_cargo_subcommand(&root).await;
            }
            if selected("cached_build_retains_output_beyond_64kib") {
                cached_build_retains_output_beyond_64kib(&root).await;
            }
            if selected("cached_build_reports_bounded_capture_loss") {
                cached_build_reports_bounded_capture_loss(&root).await;
            }
            if selected("cancelled_build_stops_descendants_before_scratch_cleanup") {
                cancelled_build_stops_descendants_before_scratch_cleanup(&root).await;
            }
            if selected("alternate_engine_retains_scratch_until_execution_finishes") {
                alternate_engine_retains_scratch_until_execution_finishes(&root).await;
            }
        });
    }

    fn selected(name: &str) -> bool {
        let filters: Vec<_> = std::env::args()
            .skip(1)
            .filter(|arg| !arg.starts_with('-'))
            .collect();
        filters.is_empty() || filters.iter().any(|filter| name.contains(filter))
    }

    fn isolate(root: &Path) {
        // Capture executable/toolchain paths before replacing HOME. Temporary
        // HOME/CARGO_HOME isolate operator config and package caches; the real
        // compiler installation remains an explicit toolchain read root.
        let rustup = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")));
        let cargo = PathBuf::from(env!("CARGO")).canonicalize().unwrap();
        let mut paths = vec![cargo.parent().unwrap().to_path_buf()];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        let path = std::env::join_paths(paths).unwrap();
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("NEWT_")
                || key.to_string_lossy().starts_with("AGENT_BRIDLE_")
            {
                std::env::remove_var(key);
            }
        }
        for key in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "VIRTUAL_ENV",
            "CARGO_TARGET_DIR",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
        ] {
            std::env::remove_var(key);
        }
        let home = root.join("home");
        let cargo_home = root.join("cargo-home");
        std::fs::create_dir_all(home.join(".newt")).unwrap();
        std::fs::create_dir(&cargo_home).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_var("CARGO_HOME", &cargo_home);
        if let Some(rustup) = rustup.filter(|path| path.is_dir()) {
            std::env::set_var("RUSTUP_HOME", rustup);
        }
        std::env::set_var("PATH", path);
        std::env::set_var("NEWT_CONFIG_DIR", home.join(".newt"));
        std::env::set_var("NEWT_SHELL_ENGINE", "brush");
        std::env::set_var("NO_COLOR", "1");
        std::env::set_current_dir(root).unwrap();
    }

    fn project(root: &Path, name: &str) -> (PathBuf, PathBuf) {
        let workspace = root.join(name);
        let outside = root.join(format!("{name}-outside"));
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let secret = outside.join("secret");
        let forbidden = outside.join("write");
        std::fs::write(&secret, "sibling-secret-sentinel").unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"brush-build-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            workspace.join("src/lib.rs"),
            "pub fn value() -> u32 { 7 }\n",
        )
        .unwrap();
        std::fs::write(workspace.join("build.rs"), format!(r#"
            fn main() {{
                use std::io::{{ErrorKind, Write}};
                std::fs::write("build-scratch-path", std::env::var("TMPDIR").unwrap()).unwrap();
                assert_eq!(std::fs::read({secret:?}).unwrap_err().kind(), ErrorKind::PermissionDenied);
                assert_eq!(std::fs::write({forbidden:?}, b"poison").unwrap_err().kind(), ErrorKind::PermissionDenied);
                std::fs::OpenOptions::new().create(true).append(true).open("build-count").unwrap().write_all(b"built\n").unwrap();
                println!("cargo:warning={MARKER}");
            }}
        "#)).unwrap();
        (workspace, outside)
    }

    async fn command(workspace: &Path, source: &str, gate: &mut CachedBuildGate) -> String {
        command_with_exec(
            workspace,
            source,
            gate,
            Scope::only(["grep".to_owned(), "printf".to_owned()]),
        )
        .await
    }

    async fn command_with_exec(
        workspace: &Path,
        source: &str,
        gate: &mut CachedBuildGate,
        exec: Scope<String>,
    ) -> String {
        let mut caveats = newt_core::confined_exec::workspace_confined_caveats(workspace);
        caveats.exec = exec;
        // Explicit existing authority, not acquired by approving Build. macOS
        // restricted networking correctly refuses under the current L3 backend.
        caveats.net = Scope::All;
        execute_tool(
            "run_command",
            &serde_json::json!({"command": source}),
            &workspace.to_string_lossy(),
            false,
            100,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            Some(gate),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
    }

    /// Grounds the mocked build-request cache read grants in a real cached
    /// Git dependency, through both the argv executor and the Brush build lane.
    /// Even fixture cache population runs with kernel-denied network access.
    #[cfg(target_os = "linux")]
    async fn cached_git_dependency_builds_offline(root: &Path) {
        use super::linked_build_shell::fixture_support::{fixture, git};
        use newt_core::confined_exec::{
            build_tool_caveats, build_tool_request, ConstrainedExecutor, ExecOrigin, ExecRequest,
            NetGrant,
        };

        let (_fixture, original_repo, _) = fixture();
        let repo = root.join("dependency");
        std::fs::rename(original_repo, &repo).unwrap();
        std::fs::write(repo.join("Cargo.toml"),
            "[package]\nname = \"cached-dependency\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"lib.rs\"\n").unwrap();
        std::fs::write(repo.join("lib.rs"), "pub fn value() -> u32 { 42 }\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--no-gpg-sign", "-qm", "dependency"]);
        let rev = git(&repo, &["rev-parse", "HEAD"]);
        let (workspace, _) = project(root, "cached-git");
        let manifest = workspace.join("Cargo.toml");
        let original = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(&manifest, format!(
            "{original}\n[dependencies]\ncached-dependency = {{ git = \"file://{}\", rev = \"{}\" }}\n",
            repo.display(), rev.trim())).unwrap();
        std::fs::write(
            workspace.join("src/lib.rs"),
            "pub fn value() -> u32 { cached_dependency::value() }\n",
        )
        .unwrap();
        // The fixture owns root, including its Cargo home and local Git remote.
        // Cargo must populate that cache from file://, never from the network.
        let cargo_home = root.join("cargo-home");
        let mut seed_grant = build_tool_caveats(root);
        if let Scope::Only(reads) = &mut seed_grant.fs_read {
            reads.insert("/etc/gitconfig".into());
        }
        let seed = ExecRequest::new(
            ExecOrigin::AgentInfluenced,
            "cargo",
            ["fetch"],
            &workspace,
            seed_grant,
        )
        .net_grant(NetGrant::DenyAll)
        .env("HOME", root.join("home").to_string_lossy())
        .env("CARGO_HOME", cargo_home.to_string_lossy())
        .env("TMPDIR", root.to_string_lossy())
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_INCREMENTAL", "0")
        .envs(
            ["RUSTUP_HOME", "PATH"]
                .into_iter()
                .filter_map(|key| std::env::var(key).ok().map(|value| (key.into(), value))),
        );
        let output = ConstrainedExecutor::run(&seed).unwrap();
        assert!(
            output.success,
            "fixture fetch: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = std::process::Command::new(env!("CARGO"))
            .current_dir(&workspace)
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_INCREMENTAL", "0")
            .args(["metadata", "--locked", "--offline", "--format-version=1"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("unconfined cached Git metadata --locked --offline ... ok");
        let marker = cargo_home.join("git/cache-sentinel");
        let credentials = cargo_home.join("credentials.toml");
        std::fs::write(&marker, "unchanged").unwrap();
        std::fs::write(&credentials, "private-registry-sentinel").unwrap();
        assert!(
            std::fs::read("/etc/hostname").is_ok(),
            "Linux system-file control"
        );
        let build_script = workspace.join("build.rs");
        let script = std::fs::read_to_string(&build_script).unwrap().replace("fn main() {", &format!(r#"fn main() {{
            assert_eq!(std::fs::write({marker:?}, b"poison").unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(std::fs::read({credentials:?}).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(std::fs::read("/etc/hostname").unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        "#));
        std::fs::write(build_script, script).unwrap();
        for cached in [true, false] {
            if !cached {
                std::fs::remove_dir_all(cargo_home.join("git")).unwrap();
            }
            let request = build_tool_request(
                &workspace,
                &workspace,
                "cargo",
                ["build", "--locked", "--offline"],
                &Scope::none(),
            )
            .env("CARGO_LOG", "cargo::sources::git=trace");
            let argv = ConstrainedExecutor::run(&request).unwrap();
            assert_eq!(
                argv.sandbox_kind,
                newt_core::sandbox_seam::SandboxKind::Landlock
            );
            let target = workspace.join("target");
            if target.exists() {
                std::fs::remove_dir_all(&target).unwrap();
            }
            let brush = execute_tool(
                "run_command",
                &serde_json::json!({"command": "cargo build --locked --offline && printf BRUSH_CACHED_GIT_FINISHED"}),
                workspace.to_str().unwrap(),
                false,
                100,
                &build_tool_caveats(&workspace),
                &mut NoMcp,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await;
            let stderr = String::from_utf8_lossy(&argv.stderr);
            if cached {
                assert!(
                    argv.success && brush.contains("BRUSH_CACHED_GIT_FINISHED"),
                    "cached Git build failed:\nargv:\n{stderr}\nBrush:\n{brush}"
                );
                assert!(target.join("debug/libbrush_build_fixture.rlib").is_file());
                assert_eq!(std::fs::read_to_string(&marker).unwrap(), "unchanged");
                eprintln!("cached Git argv and Brush builds succeed offline; cache writes, credentials and unrelated system reads denied");
            } else {
                assert!(
                    !argv.success && !target.join("debug/libbrush_build_fixture.rlib").exists()
                );
                for diagnostic in [stderr.as_ref(), brush.as_str()] {
                    assert!(
                        diagnostic.contains("cached-dependency") && diagnostic.contains("offline"),
                        "{diagnostic}"
                    );
                }
                eprintln!(
                    "missing cached-dependency is named with offline context by argv and Brush"
                );
            }
        }
        eprintln!("test cached_git_dependency_builds_offline ... ok");
    }

    async fn cached_build_runs_the_original_pipeline_once(root: &Path) {
        let (workspace, outside) = project(root, "allowed");
        let mut gate = CachedBuildGate {
            workspace: workspace.to_string_lossy().into_owned(),
            allow: true,
            queries: 0,
        };
        let source = format!(
            "cargo build --lib --offline 2>&1 | grep {MARKER}; printf once >> pipeline-count"
        );
        let output = command(&workspace, &source, &mut gate).await;
        assert!(
            output.contains(MARKER),
            "real Cargo pipeline failed: {output}"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("build-count")).unwrap(),
            "built\n",
            "{output}"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("pipeline-count")).unwrap(),
            "once",
            "the original source must execute once: {output}"
        );
        assert!(
            workspace
                .join("target/debug/libbrush_build_fixture.rlib")
                .is_file(),
            "native compiler artifact missing: {output}"
        );
        let scratch =
            PathBuf::from(std::fs::read_to_string(workspace.join("build-scratch-path")).unwrap());
        assert!(
            !scratch.exists(),
            "normal completion must release worker scratch"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).unwrap(),
            "sibling-secret-sentinel"
        );
        assert!(!outside.join("write").exists());
        assert_eq!(gate.queries, 1, "one prepared Build admission: {output}");
        eprintln!("test cached_build_runs_the_original_pipeline_once ... ok");
    }

    /// Grounds the mocked build-network scope contract in real socket calls
    /// through both build routes: a private namespace supports loopback with
    /// no external route; unavailable namespaces retain the socket-family deny.
    /// Also grounds the inherited-descriptor floor with a real host TCP socket.
    /// `--require-build-netns` rejects fallback on every probe and both routes;
    /// `--deny-build-netns` grounds retry selection by forcing setup to fail.
    #[cfg(target_os = "linux")]
    async fn build_network_probe(root: &Path, name: &str, probe: &str, marker: &str) {
        use newt_core::confined_exec::{
            build_tool_caveats, build_tool_request, ConstrainedExecutor,
        };

        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let host_socket = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        // SAFETY: F_DUPFD returns a new owned fd without CLOEXEC. The fixture
        // reserves a high slot so neither stdio nor a transient pipe can mask it.
        let fd = unsafe { libc::fcntl(host_socket.as_raw_fd(), libc::F_DUPFD, 200) };
        assert!(fd >= 200);
        let inherited = unsafe { OwnedFd::from_raw_fd(fd) };
        assert_eq!(
            unsafe { libc::fcntl(inherited.as_raw_fd(), libc::F_GETFD) },
            0
        );
        let probe = if probe.starts_with("--inherited-socket=") {
            format!("--inherited-socket={fd}")
        } else {
            probe.to_owned()
        };
        let (workspace, outside) = project(root, name);
        let executable = std::env::current_exe().unwrap();
        let argv = ConstrainedExecutor::run(&build_tool_request(
            &workspace,
            &workspace,
            executable.to_str().unwrap(),
            [probe.as_str(), "--build-network-auto"],
            &Scope::none(),
        ))
        .unwrap();
        let source = format!(
            "cargo build --lib --offline && '{}' {probe} --build-network-auto",
            executable.display()
        );
        let brush = execute_tool(
            "run_command",
            &serde_json::json!({"command": source}),
            workspace.to_str().unwrap(),
            false,
            100,
            &build_tool_caveats(&workspace),
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        let stdout = String::from_utf8_lossy(&argv.stdout);
        let stderr = String::from_utf8_lossy(&argv.stderr);
        for (route, output, diagnostic) in [
            ("argv", stdout.as_ref(), stderr.as_ref()),
            ("Brush", brush.as_str(), brush.as_str()),
        ] {
            let fallback = output.contains("BUILD_LOOPBACK_DENIED_FALLBACK");
            assert!(
                output.contains(marker) || (name == "build_tests_loopback_roundtrip" && fallback),
                "{name} ({route}): {output}\n{diagnostic}"
            );
            assert!(
                diagnostic.contains("Build network:"),
                "missing effective network mode ({route}): {diagnostic}"
            );
            for (switch, mode) in [
                ("--require-build-netns", "private loopback only"),
                ("--deny-build-netns", "deny-all fallback"),
            ] {
                if std::env::args().any(|arg| arg == switch) {
                    assert!(
                        diagnostic.contains(mode),
                        "{name} ({route}) requires {mode}: {diagnostic}"
                    );
                    eprintln!("{name} ({route}): required {mode} measured");
                }
            }
            if fallback {
                assert!(diagnostic.contains("deny-all fallback"), "{diagnostic}");
            } else if name == "build_tests_loopback_roundtrip" {
                assert!(diagnostic.contains("private loopback only"), "{diagnostic}");
            }
            eprintln!(
                "{name} ({route}): {}",
                if fallback {
                    "deny-all fallback measured"
                } else {
                    marker
                }
            );
        }
        assert!(argv.success, "{stderr}");
        assert!(!outside.join("write").exists());
        eprintln!("test {name} ... ok (argv and Brush)");
    }

    /// Grounds structural wrapper selection in the actual native timeout and
    /// Cargo pipeline. Denial precedes every stage; allowance runs source once,
    /// keeps sibling filesystem denial, and releases the actual worker lease.
    async fn timeout_build_is_admitted_before_the_original_compound(root: &Path) {
        for allow in [false, true] {
            let (workspace, outside) = project(
                root,
                if allow {
                    "timeout-allowed"
                } else {
                    "timeout-denied"
                },
            );
            let mut gate = CachedBuildGate {
                workspace: workspace.to_string_lossy().into_owned(),
                allow,
                queries: 0,
            };
            let source = "printf once >> pipeline-count; cd . && echo '--- tests dir ---'; ls src/ 2>/dev/null | head; echo '--- cargo check ---'; timeout 120 cargo check --offline 2>&1 | tail -30";
            let output = command_with_exec(&workspace, source, &mut gate, Scope::All).await;
            if !allow {
                assert!(
                    !workspace.join("pipeline-count").exists(),
                    "Build denial must precede the first source effect: {output}"
                );
                assert!(!workspace.join("build-count").exists(), "{output}");
                assert!(!workspace.join("target").exists(), "{output}");
            } else {
                assert!(
                    output.contains(MARKER),
                    "native timeout Cargo check failed: {output}"
                );
                assert_eq!(
                    std::fs::read_to_string(workspace.join("pipeline-count")).unwrap(),
                    "once",
                    "{output}"
                );
                assert_eq!(
                    std::fs::read_to_string(workspace.join("build-count")).unwrap(),
                    "built\n",
                    "{output}"
                );
                assert!(
                    std::fs::read_dir(workspace.join("target/debug/deps"))
                        .unwrap()
                        .any(|entry| entry
                            .unwrap()
                            .path()
                            .extension()
                            .is_some_and(|ext| ext == "rmeta")),
                    "actual Cargo metadata artifact missing: {output}"
                );
                let scratch = PathBuf::from(
                    std::fs::read_to_string(workspace.join("build-scratch-path")).unwrap(),
                );
                assert!(
                    !scratch.exists(),
                    "completed timeout child must release scratch"
                );
            }
            assert_eq!(gate.queries, 1, "one prepared Build admission: {output}");
            assert_eq!(
                std::fs::read_to_string(outside.join("secret")).unwrap(),
                "sibling-secret-sentinel"
            );
            assert!(!outside.join("write").exists(), "{output}");
        }
        eprintln!("test timeout_build_is_admitted_before_the_original_compound ... ok");
    }

    /// Grounds the workspace-key TempDir regression in the actual production
    /// Brush worker and its Build fence, using the real workspace-key function.
    async fn build_temp_fixtures_do_not_inherit_repository_identity(root: &Path) {
        let (workspace, outside) = project(root, "non-git-fixtures");
        std::fs::create_dir(workspace.join(".git")).unwrap();
        std::fs::write(workspace.join(".git/HEAD"), "ref: refs/heads/fixture\n").unwrap();
        std::fs::write(
            workspace.join(".git/config"),
            "[remote \"origin\"]\nurl = https://example.invalid/fixture\n",
        )
        .unwrap();
        let key_before = newt_core::workspace_key::workspace_key_v2(&workspace).unwrap();
        let mut gate = CachedBuildGate {
            workspace: workspace.to_string_lossy().into_owned(),
            allow: true,
            queries: 0,
        };
        let executable = std::env::current_exe().unwrap();
        let source = format!(
            "cargo build --lib --offline && '{}' --scratch-workspace-keys",
            executable.display()
        );
        let output = command(&workspace, &source, &mut gate).await;
        assert!(
            output.contains("NON_GIT_TEMP_FIXTURES_DISTINCT"),
            "{output}"
        );
        assert_eq!(
            key_before,
            newt_core::workspace_key::workspace_key_v2(&workspace).unwrap()
        );
        let scratch =
            PathBuf::from(std::fs::read_to_string(workspace.join("fixture-scratch-path")).unwrap());
        assert!(!scratch.starts_with(&workspace));
        assert!(!scratch.exists(), "normal completion must remove scratch");
        assert!(!outside.join("write").exists());
        assert_eq!(gate.queries, 1);
        eprintln!("test build_temp_fixtures_do_not_inherit_repository_identity ... ok");
    }

    async fn build_tests_can_bind_a_unix_socket_in_a_tempdir(root: &Path) {
        let (workspace, outside) = project(root, "socket-fixture");
        let executable = std::env::current_exe().unwrap();
        let short = tempfile::tempdir_in("/tmp").unwrap();
        let control = std::process::Command::new(&executable)
            .arg("--build-unix-socket-probe")
            .env("TMPDIR", short.path())
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(
            control.status.success(),
            "short-path host control: {}",
            String::from_utf8_lossy(&control.stderr)
        );
        let mut gate = CachedBuildGate {
            workspace: workspace.to_string_lossy().into_owned(),
            allow: true,
            queries: 0,
        };
        let source = format!(
            "cargo build --lib --offline && '{}' --build-unix-socket-probe",
            executable.display()
        );
        let output = command(&workspace, &source, &mut gate).await;
        assert!(
            output.contains("BUILD_UNIX_SOCKET_ROUNDTRIP_CONFIRMED"),
            "{output}"
        );
        eprintln!(
            "{}",
            output
                .lines()
                .find(|line| line.contains("BUILD_UNIX_SOCKET_ROUNDTRIP_CONFIRMED"))
                .unwrap()
        );
        let scratch =
            PathBuf::from(std::fs::read_to_string(workspace.join("socket-scratch-path")).unwrap());
        assert!(!scratch.starts_with(&workspace));
        assert!(!scratch.exists(), "normal completion must remove scratch");
        assert!(!outside.join("write").exists());
        assert_eq!(gate.queries, 1);
        eprintln!("test build_tests_can_bind_a_unix_socket_in_a_tempdir ... ok");
    }

    /// Grounds terminal-test availability in the actual Build fence: a fresh
    /// PTY pair and a re-executed child, with ordinary sibling file denial.
    async fn build_tests_can_use_a_private_pty(root: &Path) {
        let (workspace, outside) = project(root, "pty-fixture");
        // Keep an independently allocated fixture terminal alive throughout
        // the confined probe. Never open or modify the operator's terminal.
        let other = tests_pty::Pty::open();
        let named = std::process::Command::new("/usr/bin/tty")
            .stdin(other.slave_stdio())
            .output()
            .unwrap();
        assert!(named.status.success());
        let other_path = String::from_utf8(named.stdout).unwrap();
        #[cfg(target_os = "macos")]
        {
            use agent_bridle::Sandbox;
            let mut caveats = newt_core::confined_exec::build_tool_caveats(&workspace);
            caveats.net = Scope::All;
            let executable = std::env::current_exe().unwrap();
            if let Scope::Only(paths) = &mut caveats.fs_read {
                paths.insert(executable.to_string_lossy().into_owned());
            }
            // The Bridle default stays closed. Newt explicitly opts its
            // runtime into private PTYs; absence of that choice remains red.
            let prefix = agent_bridle::SeatbeltSandbox::new()
                .command_prefix(&caveats)
                .unwrap();
            assert!(!prefix[2].contains("com.apple.sandbox.pty"));
            let denied = std::process::Command::new(&prefix[0])
                .args(&prefix[1..])
                .arg(&executable)
                .arg("--build-pty-probe")
                .current_dir(&workspace)
                .output()
                .unwrap();
            let error = String::from_utf8_lossy(&denied.stderr);
            assert!(!denied.status.success(), "default policy admitted a PTY");
            assert!(
                error.contains("posix_openpt failed: Operation not permitted"),
                "{error}"
            );
        }
        let mut gate = CachedBuildGate {
            workspace: workspace.to_string_lossy().into_owned(),
            allow: true,
            queries: 0,
        };
        let executable = std::env::current_exe().unwrap();
        let source = format!(
            "cargo build --lib --offline && '{}' --build-pty-probe '{}'",
            executable.display(),
            other_path.trim()
        );
        let output = command(&workspace, &source, &mut gate).await;
        assert!(output.contains("BUILD_PTY_TEST_PASSED"), "{output}");
        assert!(!outside.join("write").exists());
        assert_eq!(gate.queries, 1);
        eprintln!("test build_tests_can_use_a_private_pty ... ok");
    }

    async fn denied_build_runs_no_pipeline_stage_or_redirection(root: &Path) {
        let (workspace, outside) = project(root, "denied");
        let scratch = newt_core::confined_exec::build_scratch_dir(&workspace);
        let mut gate = CachedBuildGate {
            workspace: workspace.to_string_lossy().into_owned(),
            allow: false,
            queries: 0,
        };
        let source = format!("cargo build --lib --offline 2>&1 | grep {MARKER} > denied-output; printf ran > denied-stage");
        let output = command(&workspace, &source, &mut gate).await;
        assert_eq!(
            gate.queries, 1,
            "denial must precede worker execution: {output}"
        );
        for name in [
            "build-count",
            "denied-output",
            "denied-stage",
            "target/debug/libbrush_build_fixture.rlib",
        ] {
            assert!(
                !workspace.join(name).exists(),
                "denied invocation created {name}: {output}"
            );
        }
        assert!(!outside.join("write").exists());
        assert!(!scratch.parent().unwrap().exists());
        eprintln!("test denied_build_runs_no_pipeline_stage_or_redirection ... ok");
    }

    async fn cached_build_formats_with_native_cargo_subcommand(root: &Path) {
        let (workspace, outside) = project(root, "format");
        let original = std::fs::read_to_string(workspace.join("src/lib.rs")).unwrap();
        let mut gate = CachedBuildGate {
            workspace: workspace.to_string_lossy().into_owned(),
            allow: true,
            queries: 0,
        };
        let output = command(
            &workspace,
            "cargo fmt --all && printf formatted > fmt-marker",
            &mut gate,
        )
        .await;
        assert!(
            workspace.join("fmt-marker").is_file(),
            "native cargo fmt did not reach its success marker: {output}"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("fmt-marker")).unwrap(),
            "formatted",
            "native cargo fmt failed: {output}"
        );
        assert_ne!(
            std::fs::read_to_string(workspace.join("src/lib.rs")).unwrap(),
            original,
            "the native formatter must actually change the deliberately unformatted source"
        );
        assert!(
            !workspace.join("build-count").exists(),
            "fmt must not be rewritten into a build"
        );
        assert!(!outside.join("write").exists());
        assert_eq!(gate.queries, 1, "one prepared Build admission: {output}");
        eprintln!("test cached_build_formats_with_native_cargo_subcommand ... ok");
    }

    async fn cached_build_retains_output_beyond_64kib(root: &Path) {
        let (workspace, _) = project(root, "capture");
        // Well within Newt's finite 1 MiB capture policy, but beyond Brush's
        // former 64 KiB default. The body contains no credentials or secrets.
        let mut payload = String::from("CAPTURE_HEAD_SENTINEL\n");
        for index in 0..4096 {
            if index == 2048 {
                payload.push_str("CAPTURE_MIDDLE_SENTINEL\n");
            }
            payload.push_str(&format!(
                "capture-line-{index:04}: deterministic retained output\n"
            ));
        }
        payload.push_str("CAPTURE_TAIL_SENTINEL\n");
        assert!(payload.len() > 64 * 1024 && payload.len() < 1024 * 1024);
        std::fs::write(workspace.join("capture.txt"), &payload).unwrap();
        let (_, retained) =
            captured_command(&workspace, "cargo --version >/dev/null && cat capture.txt").await;
        for marker in [
            "CAPTURE_HEAD_SENTINEL",
            "CAPTURE_MIDDLE_SENTINEL",
            "CAPTURE_TAIL_SENTINEL",
        ] {
            assert!(
                retained.contains(marker),
                "retained source lost {marker}: captured {} of {} payload bytes",
                retained.len(),
                payload.len()
            );
        }
        assert!(
            retained.contains(&payload),
            "the entire approved capture must remain retrievable"
        );
        eprintln!("test cached_build_retains_output_beyond_64kib ... ok");
    }

    async fn cached_build_reports_bounded_capture_loss(root: &Path) {
        let (workspace, _) = project(root, "capture-limit");
        let payload = format!(
            "OVER_LIMIT_HEAD\n{}\nOVER_LIMIT_UNAVAILABLE_TAIL\n",
            "bounded capture payload\n".repeat(50_000)
        );
        assert!(payload.len() > 1024 * 1024);
        std::fs::write(workspace.join("capture.txt"), &payload).unwrap();
        let (output, retained) = captured_command(
            &workspace,
            "cargo --version >/dev/null && cat capture.txt && cat capture.txt >&2",
        )
        .await;
        let notice = "stdout and stderr capture truncated; omitted bytes are unavailable";
        assert!(
            output.contains(notice),
            "presentation must disclose capture loss (retained notice={}, retained bytes={})",
            retained.contains(notice),
            retained.len()
        );
        assert!(
            retained.contains(notice),
            "retained source must disclose capture loss"
        );
        assert!(!retained.contains("OVER_LIMIT_UNAVAILABLE_TAIL"));
        assert_eq!(retained.matches("OVER_LIMIT_HEAD").count(), 2);
        assert!(
            retained.len() <= 2 * 1024 * 1024 + 1024,
            "capture must stay within the finite per-stream policy"
        );
        eprintln!("test cached_build_reports_bounded_capture_loss ... ok");
    }

    async fn captured_command(workspace: &Path, source: &str) -> (String, String) {
        let mut gate = CachedBuildGate {
            workspace: workspace.to_string_lossy().into_owned(),
            allow: true,
            queries: 0,
        };
        let store = SessionSpillStore::new([83; 16]);
        let mut caveats = newt_core::confined_exec::workspace_confined_caveats(workspace);
        caveats.exec = Scope::only(["grep".to_owned(), "printf".to_owned()]);
        caveats.net = Scope::All;
        let output = execute_tool_with_offload(
            "run_command",
            &serde_json::json!({"command": source}),
            &workspace.to_string_lossy(),
            false,
            8,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            Some(&mut gate),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            true,
            Some(&store),
            None,
        )
        .await;
        assert_eq!(gate.queries, 1, "one prepared Build admission");
        let handle = output
            .split("spill:")
            .nth(1)
            .and_then(|part| part.split('"').next())
            .expect("bounded presentation must expose a retained output handle");
        let cid = SpillCid::parse(handle).expect("canonical spill CID");
        let record = store.fetch(&cid).expect("retrievable retained source");
        (output, record.redacted_text)
    }

    fn process_running(pid: u32) -> bool {
        // SAFETY: signal zero probes only the fixture-owned PID and has no
        // memory effects. EPERM would still mean the process exists.
        let exists = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        exists || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    async fn cancelled_build_stops_descendants_before_scratch_cleanup(root: &Path) {
        let (workspace, _) = project(root, "cancel");
        let mut gate = CachedBuildGate {
            workspace: workspace.to_string_lossy().into_owned(),
            allow: true,
            queries: 0,
        };
        // The actual Cargo build-script descendant is bounded so a red run
        // cleans up naturally even when future-drop cancellation is broken.
        // Publishing its own PID avoids depending on shell job-control support.
        std::fs::write(
            workspace.join("build.rs"),
            r#"
            fn main() {
                std::fs::write("scratch-path", std::env::var("TMPDIR").unwrap()).unwrap();
                std::fs::write("child-pid", std::process::id().to_string()).unwrap();
                std::thread::sleep(std::time::Duration::from_secs(5));
                std::fs::write("post-cancel-effect", "finished").unwrap();
            }
        "#,
        )
        .unwrap();
        let source = "printf '%s' \"$$\" > worker-pid; cargo build --lib --offline; printf finished > post-worker-cancel-effect";
        let mut action = Box::pin(command(&workspace, source, &mut gate));
        let ready = async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(workspace.join("child-pid")) {
                    if let Ok(pid) = pid.parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let child = tokio::select! {
            result = &mut action => panic!("worker finished before cancellation marker: {result}"),
            ready = tokio::time::timeout(Duration::from_secs(10), ready) => {
                ready.expect("real worker must publish its descendant PID")
            }
        };
        let worker: u32 = std::fs::read_to_string(workspace.join("worker-pid"))
            .unwrap()
            .parse()
            .unwrap();
        let scratch =
            PathBuf::from(std::fs::read_to_string(workspace.join("scratch-path")).unwrap());
        assert_ne!(worker, std::process::id(), "worker PID must be isolated");
        assert!(worker > 1 && child > 1);
        assert!(
            scratch.is_dir(),
            "worker must own live scratch before cancel"
        );
        assert!(process_running(worker) && process_running(child));
        drop(action);

        let started = Instant::now();
        let mut cleanup_before_stop = false;
        let mut stopped_within_deadline = false;
        loop {
            let running = process_running(worker) || process_running(child);
            // Recheck after observing cleanup so orderly shutdown between the
            // first PID probe and the filesystem probe is not a false failure.
            cleanup_before_stop |=
                running && !scratch.exists() && (process_running(worker) || process_running(child));
            if !running {
                stopped_within_deadline = started.elapsed() < Duration::from_secs(2);
                break;
            }
            if started.elapsed() >= Duration::from_secs(7) {
                // Restrict cleanup to this authenticated fixture worker's own
                // process group; never leave test work behind after a failure.
                newt_core::confined_exec::kill_process_group(worker);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let cleanup_deadline = Instant::now() + Duration::from_secs(2);
        while scratch.exists() && Instant::now() < cleanup_deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            stopped_within_deadline && !cleanup_before_stop,
            "cancel must stop worker and child before cleanup: stopped promptly={stopped_within_deadline}, premature cleanup={cleanup_before_stop}"
        );
        assert!(!scratch.exists(), "scratch must be cleaned after shutdown");
        assert!(
            !workspace.join("post-cancel-effect").exists(),
            "canceled source must not reach its remaining stage"
        );
        assert!(!workspace.join("post-worker-cancel-effect").exists());
        assert_eq!(gate.queries, 1, "one prepared Build admission");
        eprintln!("test cancelled_build_stops_descendants_before_scratch_cleanup ... ok");
    }

    async fn alternate_engine_retains_scratch_until_execution_finishes(root: &Path) {
        // Alternate engines do not claim Brush's new prompt-cancel behavior.
        // They must still retain scratch for their existing blocking owner.
        for engine in ["safe-subset", "host"] {
            std::env::set_var("NEWT_SHELL_ENGINE", engine);
            let (workspace, _) = project(root, &format!("lease-{engine}"));
            let mut gate = CachedBuildGate {
                workspace: workspace.to_string_lossy().into_owned(),
                allow: true,
                queries: 0,
            };
            std::fs::write(
                workspace.join("build.rs"),
                r#"
                fn main() {
                    std::fs::write("scratch-path", std::env::var("TMPDIR").unwrap()).unwrap();
                    std::fs::write("child-pid", std::process::id().to_string()).unwrap();
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            "#,
            )
            .unwrap();
            let mut action = Box::pin(command(
                &workspace,
                "cargo build --lib --offline | cat",
                &mut gate,
            ));
            let ready = async {
                loop {
                    if let Ok(pid) = std::fs::read_to_string(workspace.join("child-pid")) {
                        if let Ok(pid) = pid.parse::<u32>() {
                            break pid;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            let child = tokio::select! {
                result = &mut action => panic!("{engine} finished before descendant marker: {result}"),
                ready = tokio::time::timeout(Duration::from_secs(10), ready) => {
                    ready.expect("real build must publish its descendant PID")
                }
            };
            let scratch =
                PathBuf::from(std::fs::read_to_string(workspace.join("scratch-path")).unwrap());
            assert!(scratch.is_dir() && process_running(child));
            drop(action);
            let mut premature_cleanup = false;
            let deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < deadline {
                let running = process_running(child);
                premature_cleanup |= running && !scratch.exists() && process_running(child);
                if !running && !scratch.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                !premature_cleanup,
                "{engine} removed scratch while its build descendant still ran"
            );
            assert!(
                !process_running(child),
                "bounded {engine} child must finish"
            );
            assert!(!scratch.exists(), "{engine} owner must release scratch");
            assert_eq!(gate.queries, 1);
            std::env::set_var("NEWT_SHELL_ENGINE", "brush");
            eprintln!(
                "test alternate_engine_retains_scratch_until_execution_finishes ({engine}) ... ok"
            );
        }
    }
}

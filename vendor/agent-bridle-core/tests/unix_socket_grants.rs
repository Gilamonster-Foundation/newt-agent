//! Grounds the exact Unix-endpoint policy in real inherited Seatbelt rules.
//! The child fixture only connects/listens on test-owned local endpoints.
//!
//! Forward-port of f28e585 + 1d16a7e(a) (#385) from the 0.7 maintenance line.
//! Adapted: main's `report.rs` holds EVERY restricted Seatbelt `net` shape at
//! `Advisory`/`Unknown` (README: "every restricted Seatbelt `net` scope ...
//! resolves `Unknown` and is refused by admission"), not just deny-all/loopback
//! as on 0.7 — so `enforcement_report`/`effective_sandbox_kind` fold Seatbelt
//! into the same Advisory bucket as every other backend for a Unix grant,
//! rather than promoting it to `Kernel` the way the 0.7 test asserted.

use agent_bridle_core::{
    confinement_unenforceable, effective_sandbox_kind, enforcement_report, loopback_fenced_caveats,
    net_egress_proxy_hosts, AxisEnforcement, Caveats, SandboxKind, Scope,
};

fn scoped(names: &[&str]) -> Caveats {
    Caveats {
        net: Scope::only(names.iter().map(|s| (*s).to_owned())),
        ..Caveats::top()
    }
}

#[test]
fn unix_socket_grants_are_not_proxy_hosts_and_survive_the_fence() {
    let socket = "unix:/private/tmp/service.sock";
    let local = scoped(&[socket]);
    assert_eq!(net_egress_proxy_hosts(&local), None);
    let mixed = scoped(&[socket, "example.test"]);
    assert_eq!(
        net_egress_proxy_hosts(&mixed),
        Some(vec!["example.test".into()])
    );
    let fenced = loopback_fenced_caveats(&mixed);
    let Scope::Only(names) = &fenced.net else {
        panic!("bounded fence")
    };
    assert!(names.contains(socket));
    assert!(!names.contains("example.test"));
    assert_eq!(fenced.fs_read, mixed.fs_read);
    assert_eq!(fenced.fs_write, mixed.fs_write);
    assert_eq!(fenced.exec, mixed.exec);
}

/// Every backend — Seatbelt included — stays Advisory (never reaches a Kernel
/// floor) for a Unix-only or Unix+loopback grant: main's report.rs holds ALL
/// restricted Seatbelt `net` shapes at Advisory (not just deny-all/loopback as
/// on 0.7), so a Unix grant is unenforceable at a Kernel floor everywhere, and
/// `effective_sandbox_kind` still selects Seatbelt as the honestly-engaged
/// backend for it (the profile machinery is real; admission's hold on
/// restricted Seatbelt net is independent of this grant shape, per
/// `agent-bridle-core/README.md`). MicroVm is excluded: its net witness is
/// Kernel unconditionally (no guest network device at all — egress is
/// impossible regardless of the grant shape), so it is never unenforceable.
#[test]
fn unix_socket_grants_stay_advisory_on_every_backend_including_seatbelt() {
    for names in [
        vec!["unix:/private/tmp/service.sock"],
        vec!["unix:/private/tmp/service.sock", "localhost"],
    ] {
        let grant = scoped(&names);
        for kind in [
            SandboxKind::None,
            SandboxKind::Landlock,
            SandboxKind::AppContainer,
            SandboxKind::MinimalRootfs,
            SandboxKind::Seatbelt,
        ] {
            assert!(
                confinement_unenforceable(kind, &grant, AxisEnforcement::Kernel),
                "{kind:?}"
            );
            assert_eq!(
                enforcement_report(&grant, kind).net,
                Some(AxisEnforcement::Advisory),
                "{kind:?}"
            );
        }
        assert_eq!(
            enforcement_report(&grant, SandboxKind::MicroVm).net,
            Some(AxisEnforcement::Kernel),
            "MicroVm has no guest network device, so egress is impossible regardless of grant shape"
        );
        assert_eq!(
            effective_sandbox_kind(SandboxKind::Seatbelt, &grant),
            SandboxKind::Seatbelt,
            "Seatbelt still honestly engages for a Unix grant even though its net witness is Advisory"
        );
    }
}

#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
mod seatbelt {
    use super::*;
    use agent_bridle_core::{
        ConfinedCommand, Gate, Sandbox, SeatbeltSandbox, Tool, ToolContext, ToolError, ToolResult,
    };
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ExitStatus, Stdio};
    use std::time::Duration;

    struct Endpoints {
        dir: PathBuf,
        first: PathBuf,
        second: PathBuf,
        _listeners: [UnixListener; 2],
    }
    impl Endpoints {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "bridle-unix-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            let dir = dir.canonicalize().unwrap();
            let first = dir.join("first.sock");
            let second = dir.join("second.sock");
            let listeners = [
                UnixListener::bind(&first).unwrap(),
                UnixListener::bind(&second).unwrap(),
            ];
            Self {
                dir,
                first,
                second,
                _listeners: listeners,
            }
        }
        fn grant(&self) -> String {
            format!("unix:{}", self.first.display())
        }
    }
    impl Drop for Endpoints {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn unix_socket_profile_is_exact_outbound_only_and_validates_existing_paths() {
        let ep = Endpoints::new();
        let grant = ep.grant();
        let prefix = SeatbeltSandbox::new()
            .command_prefix(&scoped(&[&grant]))
            .unwrap();
        assert!(
            !prefix.is_empty(),
            "Unix-only authority must engage a network fence"
        );
        let profile = &prefix[2];
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains(&format!(
            "(allow network-outbound (literal \"{}\"))",
            ep.first.display()
        )));
        assert!(!profile.contains("(allow network*"));
        assert!(!profile.contains("(allow network-inbound"));
        assert!(
            SeatbeltSandbox::new()
                .command_prefix(&scoped(&[&grant, "example.test"]))
                .is_err(),
            "mixed remote authority requires proxy projection before direct spawn"
        );
        std::os::unix::fs::symlink(&ep.first, ep.dir.join("alias.sock")).unwrap();
        std::fs::write(ep.dir.join("ordinary-file"), b"not a socket").unwrap();
        for value in [
            "unix:relative.sock".to_owned(),
            "unix:".to_owned(),
            format!("unix:{}/*", ep.dir.display()),
            format!("unix:{}/alias.sock", ep.dir.display()),
            format!("unix:{}/ordinary-file", ep.dir.display()),
            format!("unix:{}/missing.sock", ep.dir.display()),
            format!(
                "unix:{}/../{}/first.sock",
                ep.dir.display(),
                ep.dir.file_name().unwrap().to_str().unwrap()
            ),
        ] {
            assert!(
                SeatbeltSandbox::new()
                    .command_prefix(&scoped(&[&value]))
                    .is_err(),
                "must reject {value:?}"
            );
        }
    }

    struct Fixture;
    #[async_trait::async_trait]
    impl Tool for Fixture {
        fn name(&self) -> &str {
            "unix-socket-fixture"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        async fn invoke(
            &self,
            _: serde_json::Value,
            _: &ToolContext,
        ) -> ToolResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
    }

    fn probe_command(mode: &str, path: &Path, expected: &str) -> ConfinedCommand {
        ConfinedCommand::new(std::env::current_exe().unwrap().to_str().unwrap())
            .args([
                "--exact",
                "seatbelt::unix_socket_probe_child",
                "--ignored",
                "--nocapture",
            ])
            .env("BRIDLE_SOCKET_PROBE", mode)
            .env("BRIDLE_SOCKET_PATH", path)
            .env("BRIDLE_SOCKET_EXPECT", expected)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
    }

    fn wait_for_probe(child: &mut Child, timeout: Duration) -> std::io::Result<ExitStatus> {
        let deadline = std::time::Instant::now() + timeout;
        let error = loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(error) => break error,
            }
            if std::time::Instant::now() >= deadline {
                break std::io::Error::new(std::io::ErrorKind::TimedOut, "Unix probe timed out");
            }
            std::thread::sleep(Duration::from_millis(15));
        };
        child.kill()?;
        child.wait()?;
        Err(error)
    }

    fn probe(caveats: &Caveats, mode: &str, path: &Path, expected: &str, asynchronous: bool) {
        let cx = Gate::new(0).authorize(&Fixture, caveats).unwrap();
        let command = probe_command(mode, path, expected);
        let (stdout, stderr) = if asynchronous {
            #[cfg(feature = "spawn-tokio")]
            {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        use tokio::io::AsyncReadExt;
                        let mut child = command.spawn_tokio(&cx).unwrap();
                        assert!(child.egress_proxied());
                        let mut stdout = Vec::new();
                        let mut stderr = Vec::new();
                        let mut out = child.take_stdout().unwrap().take(8192);
                        let mut err = child.take_stderr().unwrap().take(8192);
                        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            let (a, b) = tokio::join!(
                                out.read_to_end(&mut stdout),
                                err.read_to_end(&mut stderr)
                            );
                            a.unwrap();
                            b.unwrap();
                        })
                        .await
                        .unwrap();
                        (stdout, stderr)
                    })
            }
            #[cfg(not(feature = "spawn-tokio"))]
            {
                panic!("async fixture needs spawn-tokio")
            }
        } else {
            let mut child = command.spawn(&cx).unwrap().child;
            wait_for_probe(&mut child, Duration::from_secs(5)).unwrap();
            // This fixed child emits only small markers and spawns no descendants.
            let out = child.wait_with_output().unwrap();
            (out.stdout, out.stderr)
        };
        assert!(
            String::from_utf8_lossy(&stdout).contains("UNIX_PROBE_OK"),
            "probe {mode} {expected} under {:?}: {} {}",
            caveats.net,
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    }

    /// Grounds the probe deadline and kill/reap cleanup in a real stalled child;
    /// the other real-child case independently grounds the mocked socket policy.
    #[test]
    #[ignore = "re-enable when Seatbelt net is promoted from Advisory (1ac8995 hold)"]
    fn unix_socket_probe_timeout_kills_and_reaps_the_child() {
        let ep = Endpoints::new();
        let cx = Gate::new(0)
            .authorize(&Fixture, &scoped(&[&ep.grant()]))
            .unwrap();
        let mut child = probe_command("stall", &ep.first, "allowed")
            .spawn(&cx)
            .unwrap()
            .child;
        assert!(child.try_wait().unwrap().is_none());
        let result = wait_for_probe(&mut child, Duration::from_millis(100));
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(
            !child
                .try_wait()
                .unwrap()
                .expect("child must be reaped")
                .success(),
            "the stalled child must be terminated, not allowed to finish"
        );
    }

    /// Pins the 0.8 posture positively: a `unix:` grant is refused at
    /// admission, not silently downgraded. Every restricted Seatbelt `net`
    /// scope resolves Advisory and is refused before spawn (L3 BOUND) — see
    /// `agent-bridle-core/README.md` and the ignored real-child proofs above,
    /// which re-enable the day this hold lifts.
    #[test]
    fn unix_socket_grant_is_refused_at_admission_under_seatbelt() {
        let ep = Endpoints::new();
        let cx = Gate::new(0)
            .authorize(&Fixture, &scoped(&[&ep.grant()]))
            .unwrap();
        let err = probe_command("connect", &ep.first, "allowed")
            .spawn(&cx)
            .unwrap_err();
        let ToolError::Denied { reason } = err else {
            panic!("expected a Denied refusal, got {err:?}");
        };
        assert!(reason.contains("Net"), "{reason}");
        assert!(reason.contains("L3 BOUND"), "{reason}");
    }

    /// Real inherited kernel proof for the profile and proxy-partition tests:
    /// one granted live socket works; its live sibling and TCP stay denied.
    #[test]
    #[ignore = "re-enable when Seatbelt net is promoted from Advisory (1ac8995 hold)"]
    fn unix_socket_real_child_obeys_exact_connect_scope_and_cannot_listen() {
        let ep = Endpoints::new();
        // Reachability controls avoid confusing a missing service with denial.
        UnixStream::connect(&ep.first).unwrap();
        UnixStream::connect(&ep.second).unwrap();
        let grant = ep.grant();
        let caveats = scoped(&[&grant]);
        probe(&scoped(&[]), "connect", &ep.first, "denied", false);
        probe(&caveats, "connect", &ep.first, "allowed", false);
        probe(&caveats, "connect", &ep.second, "denied", false);
        let narrow = Caveats {
            fs_read: Scope::only([
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                ep.first.to_string_lossy().into_owned(),
            ]),
            fs_write: Scope::only([ep.first.to_string_lossy().into_owned()]),
            ..caveats.clone()
        };
        probe(&narrow, "connect", &ep.first, "allowed", false);
        std::fs::write(ep.dir.join("not-granted.txt"), b"fixture").unwrap();
        probe(
            &narrow,
            "read",
            &ep.dir.join("not-granted.txt"),
            "denied",
            false,
        );
        probe(&caveats, "bind", &ep.dir.join("new.sock"), "denied", false);
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe(
            &caveats,
            "tcp",
            Path::new(&tcp.local_addr().unwrap().to_string()),
            "denied",
            false,
        );
        #[cfg(feature = "spawn-tokio")]
        {
            let mixed = scoped(&[&grant, "example.test"]);
            probe(&mixed, "connect", &ep.first, "allowed", true);
            probe(&mixed, "connect", &ep.second, "denied", true);
        }
    }

    #[test]
    #[ignore = "private child of the real Unix-socket confinement fixture"]
    fn unix_socket_probe_child() {
        let mode = std::env::var("BRIDLE_SOCKET_PROBE").unwrap();
        let path = std::env::var("BRIDLE_SOCKET_PATH").unwrap();
        let result = match mode.as_str() {
            "stall" => {
                std::thread::sleep(Duration::from_secs(2));
                Ok(())
            }
            "connect" => UnixStream::connect(&path).map(|_| ()),
            "bind" => UnixListener::bind(&path).map(|_| ()),
            "read" => std::fs::read(&path).map(|_| ()),
            "tcp" => std::net::TcpStream::connect_timeout(
                &path.parse().unwrap(),
                std::time::Duration::from_secs(1),
            )
            .map(|_| ()),
            _ => panic!("unknown probe"),
        };
        if std::env::var("BRIDLE_SOCKET_EXPECT").unwrap() == "allowed" {
            result.unwrap();
        } else {
            assert_eq!(
                result.unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        println!("UNIX_PROBE_OK");
    }
}

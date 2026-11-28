//! Real-kernel grounding for fdguard's private-proc options, read mask and
//! init argv-range parser. Both fixed spawn funnels opt in; this test image
//! inspects its proc view before the worker's separately staged authentication.
use std::path::Path;

pub fn dispatch() -> bool {
    let probe = std::env::args()
        .find_map(|arg| arg.strip_prefix("--build-proc-probe=").map(str::to_owned))
        .or_else(|| {
            let worker = std::env::args()
                .skip(1)
                .eq(["--agent-bridle-worker", "brush"]);
            worker
                .then(|| std::env::var("AGENT_BRIDLE_WORKER_NONCE").ok())
                .flatten()
                .and_then(|nonce| nonce.strip_prefix("build-proc-probe:").map(str::to_owned))
        });
    let Some(host) = probe else { return false };
    let host: u32 = host.parse().unwrap();
    if std::net::TcpListener::bind(("127.0.0.1", 0)).is_err() {
        for path in ["/proc/self/stat", "/proc/stat"] {
            assert_eq!(
                std::fs::read(path).unwrap_err().raw_os_error(),
                Some(libc::EACCES)
            );
        }
        println!("BUILD_PROC_DENIED_FALLBACK");
        return true;
    }
    let stat =
        std::fs::read_to_string("/proc/self/stat").expect("private own stat must be readable");
    assert!(
        stat.starts_with("2 ("),
        "the probe is the namespace payload"
    );
    for path in [
        "/proc/stat",
        "/proc/meminfo",
        "/proc/cpuinfo",
        "/proc/sys/kernel/hostname",
        "/proc/sys/kernel/random/boot_id",
    ] {
        assert_eq!(
            std::fs::metadata(path).unwrap_err().raw_os_error(),
            Some(libc::ENOENT),
            "{path}"
        );
    }
    assert_eq!(
        std::fs::metadata(format!("/proc/{host}"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOENT)
    );
    assert_eq!(
        std::fs::read("/proc/1/environ").unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
    assert!(
        std::fs::read("/proc/1/cmdline")
            .unwrap()
            .iter()
            .all(|byte| *byte == 0),
        "init must disclose no inherited argv bytes"
    );
    assert!(std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/self/comm")
        .is_err());
    // A causal hold keeps the sibling alive until its proc record is checked.
    let mut gate = [0; 2];
    // SAFETY: this early probe is single-threaded. The fork child only closes,
    // reads, and exits; the parent keeps the PID unreaped until observation.
    unsafe {
        assert_eq!(libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC), 0);
        let sibling = libc::fork();
        assert!(sibling >= 0);
        if sibling == 0 {
            libc::close(gate[1]);
            let mut byte = 0u8;
            libc::read(gate[0], (&mut byte as *mut u8).cast(), 1);
            libc::_exit(0);
        }
        libc::close(gate[0]);
        assert!(std::fs::read_to_string(format!("/proc/{sibling}/stat"))
            .unwrap()
            .starts_with(&format!("{sibling} (")));
        libc::close(gate[1]);
        let mut status = 0;
        assert_eq!(libc::waitpid(sibling, &mut status, 0), sibling);
        assert_eq!(status, 0);
        let nested = libc::fork();
        assert!(nested >= 0);
        if nested == 0 {
            let denied =
                libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID) < 0
                    || libc::mount(
                        c"proc".as_ptr(),
                        c"/proc".as_ptr(),
                        c"proc".as_ptr(),
                        15,
                        std::ptr::null(),
                    ) < 0;
            libc::_exit(i32::from(!denied));
        }
        assert_eq!(libc::waitpid(nested, &mut status, 0), nested);
        assert_eq!(status, 0, "a nested full proc mount must be refused");
    }
    println!("BUILD_PRIVATE_PROCFS_CONFIRMED");
    true
}

pub fn run(root: &Path) {
    use agent_bridle::{BuildIsolation, ConfinedCommand, ConfinedStdio, SandboxedWorker};
    let workspace = root.join("private-proc");
    std::fs::create_dir(&workspace).unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut caveats = newt_core::confined_exec::build_tool_caveats(&workspace);
    // This fixed syscall probe uses no build scratch. Do not declare unused
    // covered inputs, and do not override the product's scratch environment.
    for scope in [&mut caveats.fs_read, &mut caveats.fs_write] {
        if let newt_core::Scope::Only(paths) = scope {
            paths.retain(|path| {
                !Path::new(path).starts_with("/tmp") && !Path::new(path).starts_with("/dev/shm")
            });
        }
    }
    if let newt_core::Scope::Only(paths) = &mut caveats.fs_read {
        paths.insert(executable.to_str().unwrap().to_owned());
    }
    let cx = agent_bridle::Gate::new(0)
        .authorize(&agent_bridle::ShellTool::default(), &caveats)
        .unwrap();
    let policy = std::sync::Arc::new(agent_bridle::SandboxPolicy {
        child_network: agent_bridle::ChildNetworkPolicy::PrivateBuild,
        private_pid_namespace: true,
        ..Default::default()
    });
    let host = std::process::id();
    let mut outcomes = Vec::new();
    for worker in [false, true] {
        let (isolation, namespace, child) = if worker {
            let worker = SandboxedWorker::brush()
                .sandbox_policy(policy.clone())
                .spawn(&cx, &format!("build-proc-probe:{host}"), &workspace)
                .unwrap();
            (worker.build_isolation, worker.build_namespace, worker.child)
        } else {
            let child = ConfinedCommand::new(executable.to_str().unwrap())
                .arg(format!("--build-proc-probe={host}"))
                .sandbox_policy(policy.clone())
                .current_dir(&workspace)
                .new_process_group()
                .stdin(ConfinedStdio::Null)
                .stdout(ConfinedStdio::Piped)
                .stderr(ConfinedStdio::Piped)
                .spawn(&cx)
                .unwrap();
            (child.build_isolation, child.build_namespace, child.child)
        };
        let output = child.wait_with_output().unwrap();
        drop(namespace);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let route = if worker { "Brush worker" } else { "argv" };
        // Run both routes even when the first is red, so the evidence names both.
        let expected = if std::env::args().any(|arg| arg.starts_with("--deny-build-")) {
            BuildIsolation::DenyAllFallback
        } else if std::env::args().any(|arg| arg == "--require-build-netns") {
            BuildIsolation::Private
        } else {
            isolation.unwrap()
        };
        let (marker, mode) = match expected {
            BuildIsolation::Private => (
                "BUILD_PRIVATE_PROCFS_CONFIRMED",
                "private process IDs and /proc (this build's processes only)",
            ),
            BuildIsolation::DenyAllFallback => ("BUILD_PROC_DENIED_FALLBACK", "deny-all fallback"),
        };
        let ok = output.status.success()
            && isolation == Some(expected)
            && stdout.contains(marker)
            && stderr.contains(mode);
        eprintln!(
            "build_tests_private_procfs ({route}): {isolation:?}, status={}: {stdout}{stderr}",
            output.status
        );
        outcomes.push(ok);
    }
    assert!(
        outcomes.into_iter().all(|ok| ok),
        "both procfs funnels must satisfy their reported isolation mode"
    );
    eprintln!("test build_tests_private_procfs ... ok (argv and Brush worker funnels)");
}

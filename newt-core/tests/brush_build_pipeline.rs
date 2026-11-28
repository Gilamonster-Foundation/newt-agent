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
    if let Some(path) = std::env::args().find_map(|arg| {
        arg.strip_prefix("--build-granted-write-probe=")
            .map(std::path::PathBuf::from)
    }) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"granted host write").unwrap();
        println!("BUILD_GRANTED_WRITE_COMPLETED");
        use std::os::unix::fs::MetadataExt;
        let expected = std::env::args()
            .find_map(|arg| arg.strip_prefix("--host-mounts=").map(str::to_owned))
            .unwrap();
        let actual = [
            "/proc/self/ns/mnt",
            "/tmp",
            "/dev/shm",
            "/dev/pts",
            "/dev/ptmx",
            "/etc/passwd",
            "/etc/group",
        ]
        .map(|path| {
            let metadata = std::fs::metadata(path).unwrap();
            format!("{}:{}", metadata.dev(), metadata.ino())
        })
        .join(",");
        if actual == expected {
            println!("BUILD_HOST_MOUNTS_CONFIRMED");
        }
        return;
    }
    #[cfg(target_os = "linux")]
    if std::env::args().any(|arg| arg == "--worker-result-channel-probe") {
        // SAFETY: getppid has no preconditions or memory effects.
        let parent = unsafe { libc::getppid() };
        for fd in [1, 2] {
            let reopened = std::fs::OpenOptions::new()
                .write(true)
                .open(format!("/proc/{parent}/fd/{fd}"));
            println!(
                "WORKER_RESULT_FD{fd}_REOPEN={}",
                if reopened.is_err() {
                    "denied"
                } else {
                    "opened"
                }
            );
        }
        println!("WORKER_RESULT_STDOUT_INTACT");
        eprintln!("WORKER_RESULT_STDERR_INTACT");
        return;
    }
    #[cfg(target_os = "linux")]
    if let Some(probe) =
        std::env::args().find_map(|arg| arg.strip_prefix("--build-pid-probe=").map(str::to_owned))
    {
        let (harness, hold) = match probe.strip_suffix(":hold") {
            Some(harness) => (harness, true),
            None => (probe.as_str(), false),
        };
        let harness: libc::pid_t = harness.parse().unwrap();
        // SAFETY: these calls have no preconditions; signal 0 delivers nothing.
        let (pid, parent, group, signal) = unsafe {
            (
                libc::getpid(),
                libc::getppid(),
                libc::getpgrp(),
                libc::kill(harness, 0),
            )
        };
        let unreachable = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(group, pid, "the payload leads its own process group");
        if std::net::TcpListener::bind(("127.0.0.1", 0)).is_err() {
            // The fresh fallback child never entered a namespace: it is the
            // harness's direct child and host processes stay signalable.
            assert_eq!((parent, signal), (harness, 0));
            println!("BUILD_PIDNS_DENIED_FALLBACK");
        } else {
            assert_eq!((pid, parent), (2, 0), "payload identity");
            assert_eq!(
                (signal, unreachable),
                (-1, Some(libc::ESRCH)),
                "the harness must not be nameable"
            );
            println!("BUILD_PRIVATE_PIDNS_CONFIRMED pid={pid} ppid={parent} pgrp={group}");
        }
        if hold {
            use std::io::{Read, Write};
            // A descendant in its own session. Only the namespace reaches it,
            // so it is created only where a namespace exists.
            // SAFETY: single-threaded here; the child only reads and exits.
            if parent == 0 && unsafe { libc::fork() } == 0 {
                assert_eq!(unsafe { libc::setsid() }, unsafe { libc::getpid() });
                println!("BUILD_PIDNS_DESCENDANT_DETACHED");
            }
            std::io::stdout().flush().unwrap();
            // The test never writes and never closes its end before the kill.
            let _ = std::io::stdin().read(&mut [0]);
        }
        return;
    }
    #[cfg(target_os = "linux")]
    if std::env::args().any(|arg| arg == "--build-identity-probe") {
        let private = std::net::TcpListener::bind(("127.0.0.1", 0)).is_ok();
        let paths = ["/etc/passwd", "/etc/group"];
        if !private {
            for path in paths {
                assert_eq!(
                    std::fs::read(path).unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
            }
            println!("BUILD_IDENTITY_DENIED_FALLBACK");
            return;
        }
        let (expected, other): ([Vec<Vec<u8>>; 2], libc::uid_t) =
            serde_json::from_slice(&std::fs::read("identity-expected.json").unwrap()).unwrap();
        for (path, fields) in paths.into_iter().zip(expected) {
            let contents = std::fs::read(path).expect("private identity file must be readable");
            let lines: Vec<_> = contents.split(|byte| *byte == b'\n').collect();
            assert_eq!(lines.len(), 2, "exactly one terminated account entry");
            assert!(lines[1].is_empty());
            let observed: Vec<_> = lines[0].split(|byte| *byte == b':').collect();
            assert!(
                observed == fields,
                "identity must match the harness NSS snapshot"
            );
            assert!(std::fs::OpenOptions::new().write(true).open(path).is_err());
            assert!(std::fs::hard_link(path, "/tmp/identity-alias").is_err());
        }
        for path in ["/etc/shadow", "/etc/gshadow"] {
            assert!(
                std::fs::read(path).is_err(),
                "password hashes must remain hidden"
            );
        }
        for path in ["/tmp/.newt-build-passwd", "/tmp/.newt-build-group"] {
            assert!(!std::path::Path::new(path).exists(), "no writable alias");
        }
        for path in [
            "/run/systemd/userdb/io.systemd.Multiplexer",
            "/var/lib/sss/pipes/nss",
        ] {
            assert!(std::os::unix::net::UnixStream::connect(path).is_err());
        }
        // Ground the normal NSS lookup, not just the compatibility file bytes.
        let mut buffer = vec![0; 65536];
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        let uid = unsafe { libc::geteuid() };
        assert_eq!(
            unsafe {
                libc::getpwuid_r(
                    uid,
                    &mut entry,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut found,
                )
            },
            0
        );
        assert!(!found.is_null());
        assert_eq!(entry.pw_uid, uid);
        // Some NSS modules synthesize root/nobody even when the host databases
        // are denied. Those built-in defaults are not host account disclosure;
        // the private files above must still contain exactly the mapped entry.
        // The parent proved this other uid resolves to a real host account.
        assert!(![uid, 0, 65534].contains(&other));
        unsafe {
            libc::getpwuid_r(
                other,
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            );
        }
        assert!(
            found.is_null(),
            "the real host account must not be visible through NSS"
        );
        let gid = unsafe { libc::getegid() };
        let mut group: libc::group = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                libc::getgrgid_r(
                    gid,
                    &mut group,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut found,
                )
            },
            0
        );
        assert!(!found.is_null());
        assert_eq!(group.gr_gid, gid);
        unsafe {
            libc::getgrgid_r(
                if gid == 1 { 2 } else { 1 },
                &mut group,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            );
        }
        assert!(found.is_null(), "another primary group must remain hidden");
        println!("BUILD_PRIVATE_IDENTITY_CONFIRMED");
        return;
    }

    #[cfg(target_os = "linux")]
    if std::env::args().any(|arg| arg == "--toolchains-3327") {
        toolchains::run();
        return;
    }

    #[cfg(target_os = "linux")]
    if let Some(names) = std::env::args().find_map(|arg| {
        arg.strip_prefix("--build-scratch-probe=")
            .map(str::to_owned)
    }) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let private = std::net::TcpListener::bind(("127.0.0.1", 0)).is_ok();
        let names: Vec<_> = names.split(':').collect();
        assert_eq!(names.len(), 3);
        let caller_uid: u32 = names[2].parse().unwrap();
        // SAFETY: geteuid has no preconditions or memory effects.
        assert_eq!(unsafe { libc::geteuid() }, caller_uid);
        for (directory, name) in ["/tmp", "/dev/shm"].into_iter().zip(names) {
            let host = std::path::Path::new(directory).join(name);
            let child = host.with_extension("child");
            let write = std::fs::write(&child, b"private scratch");
            if private {
                let metadata = std::fs::metadata(directory).unwrap();
                assert_eq!(metadata.uid(), caller_uid, "{directory} owner");
                assert_eq!(metadata.mode() & 0o7777, 0o700, "{directory} mode");
                write.unwrap_or_else(|error| panic!("private scratch write {directory}: {error}"));
                assert_eq!(std::fs::read(&child).unwrap(), b"private scratch");
                // Grounds the mocked trust walk's mode checks: privacy never
                // exempts a subsequently writable object or ancestor.
                let trust =
                    newt_core::git_staging::TrustContext::bind(&newt_core::Scope::none()).unwrap();
                std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o600)).unwrap();
                newt_core::git_staging::trust_check(&child, &trust).unwrap();
                std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o660)).unwrap();
                assert!(newt_core::git_staging::trust_check(&child, &trust).is_err());
                std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o600)).unwrap();
                for mode in [0o1777, 0o770] {
                    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode))
                        .unwrap();
                    assert!(newt_core::git_staging::trust_check(&child, &trust).is_err());
                }
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                newt_core::git_staging::trust_check(&child, &trust).unwrap();
                assert!(!host.exists(), "host scratch sentinel must be hidden");
                std::fs::write(&host, b"private counterpart").unwrap();
                assert_eq!(std::fs::read(&host).unwrap(), b"private counterpart");
                let expected_capacity = if directory == "/tmp" {
                    2 * 1024 * 1024 * 1024
                } else {
                    256 * 1024 * 1024
                };
                let directory = std::ffi::CString::new(directory).unwrap();
                // SAFETY: live C path and writable statfs storage.
                let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { libc::statfs(directory.as_ptr(), &mut stats) }, 0);
                assert_eq!(stats.f_type, libc::TMPFS_MAGIC);
                let capacity = stats.f_blocks * stats.f_bsize as u64;
                assert_eq!(capacity, expected_capacity);
                // Grounds the mount-topology floor after the filesystem fence.
                assert_eq!(
                    unsafe {
                        libc::mount(
                            c"tmpfs".as_ptr(),
                            directory.as_ptr(),
                            c"tmpfs".as_ptr(),
                            0,
                            std::ptr::null(),
                        )
                    },
                    -1
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EPERM)
                );
            } else {
                assert_eq!(
                    write.unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
            }
        }
        // Grounds the shared toolchain environment contract: these values stay
        // unchanged and their directories exist inside the private mounts too.
        for name in ["HOME", "TMPDIR", "TMP", "TEMP"] {
            let directory = std::env::var_os(name).unwrap();
            let path = std::path::Path::new(&directory).join("runtime-scratch");
            std::fs::write(&path, b"runtime scratch")
                .unwrap_or_else(|error| panic!("runtime scratch {name}: {error}"));
            assert_eq!(std::fs::read(&path).unwrap(), b"runtime scratch");
        }
        println!("BUILD_RUNTIME_DIRECTORIES_CONFIRMED");
        println!(
            "{}",
            if private {
                "BUILD_PRIVATE_SCRATCH_CONFIRMED"
            } else {
                "BUILD_SCRATCH_DENIED_FALLBACK"
            }
        );
        return;
    }
    #[cfg(target_os = "linux")]
    if let Some(mode) =
        std::env::args().find_map(|arg| arg.strip_prefix("--git-alias-ground=").map(str::to_owned))
    {
        git_alias_bind::ground_child(&mode);
    }
    #[cfg(target_os = "linux")]
    if std::env::args().any(|arg| arg == "--build-git-alias-probe") {
        return git_alias_bind::route_child();
    }
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
            // Enumerate via libc's netlink API; the build fence intentionally
            // does not grant procfs network files. The allocation ends with a
            // zero-index/null-name sentinel and must be freed even on mismatch.
            let interfaces = unsafe { libc::if_nameindex() };
            assert!(!interfaces.is_null(), "{}", std::io::Error::last_os_error());
            // SAFETY: libc returned a sentinel-terminated array with C-string
            // names. Short-circuiting checks the first entry before advancing.
            let only_loopback = unsafe {
                (*interfaces).if_index != 0
                    && !(*interfaces).if_name.is_null()
                    && std::ffi::CStr::from_ptr((*interfaces).if_name) == c"lo"
                    && (*interfaces.add(1)).if_index == 0
                    && (*interfaces.add(1)).if_name.is_null()
            };
            unsafe { libc::if_freenameindex(interfaces) };
            assert!(only_loopback, "private namespace must expose only loopback");
            println!("BUILD_PRIVATE_INTERFACE_CONFIRMED: lo");
        }
        for host in ["127.0.0.1", "127.23.45.67", "::1"] {
            let listener = TcpListener::bind((host, 0))
                .unwrap_or_else(|error| panic!("loopback bind {host}: {error}"));
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            client
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            client.write_all(b"ping").unwrap();
            let (mut server, _) = listener.accept().unwrap();
            server
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut payload = [0; 4];
            server.read_exact(&mut payload).unwrap();
            assert_eq!(&payload, b"ping");
            server.write_all(b"pong").unwrap();
            client.read_exact(&mut payload).unwrap();
            assert_eq!(&payload, b"pong");
            println!("BUILD_LOOPBACK_ADDRESS_CONFIRMED: {host}");
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
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        assert_eq!(input, "private master input\n");
        println!("BUILD_PTY_ROUNDTRIP_CONFIRMED");
        return;
    }
    #[cfg(unix)]
    if let Some(probe) = std::env::args()
        .find(|arg| arg == "--build-pty-probe" || arg.starts_with("--build-pty-probe="))
    {
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(target_os = "linux")]
        if std::net::TcpListener::bind(("127.0.0.1", 0)).is_err() {
            // A failed setup must retain the original host-terminal refusal.
            assert_eq!(
                unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            println!("BUILD_PTY_DENIED_FALLBACK");
            return;
        }
        let pty = tests_pty::Pty::open();
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
            use std::os::unix::fs::MetadataExt;
            let host: Vec<_> = probe
                .strip_prefix("--build-pty-probe=")
                .unwrap()
                .split(':')
                .collect();
            let host_dev: u64 = host[0].parse().unwrap();
            assert_ne!(std::fs::metadata("/dev/pts").unwrap().dev(), host_dev);
            assert_eq!(
                unsafe { libc::fcntl(host[2].parse::<i32>().unwrap(), libc::F_GETFD) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
            if let Ok(metadata) = std::fs::metadata(host[1]) {
                // The same numeric slave name can denote a different instance.
                assert_ne!(metadata.dev(), host_dev);
            }
            for path in ["/dev/tty", "/dev/console"] {
                let error = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap_err();
                assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                ));
            }
            let slaves: Vec<_> = std::fs::read_dir("/dev/pts")
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| entry.file_name() != "ptmx")
                .collect();
            assert_eq!(slaves.len(), 1, "only this build's slave may be visible");
            let slave = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(slaves[0].path())
                .unwrap();
            let fd = slave.as_raw_fd();
            let mut termios: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut termios) }, 0);
            assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) }, 0);
            let mut size = libc::winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) }, 0);
            size.ws_row = 0;
            assert_eq!(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) }, 0);
            assert_eq!((size.ws_row, size.ws_col), (24, 80));
            for request in [libc::TIOCSTI, libc::TIOCLINUX] {
                let byte = 0u8;
                assert_eq!(unsafe { libc::ioctl(fd, request, &byte) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EACCES)
                );
            }
            // One pair is already held by Pty. Ground the per-instance max,
            // not a timing assumption about the global host PTY allocation.
            let mut masters = Vec::new();
            for _ in 1..64 {
                let fd =
                    unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
                assert!(
                    fd >= 0,
                    "private PTY bound: {}",
                    std::io::Error::last_os_error()
                );
                masters.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            assert_eq!(
                unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOSPC)
            );
        }
        #[cfg(not(target_os = "linux"))]
        let _ = probe;
        pty.type_in("private master input\n");
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

#[cfg(target_os = "linux")]
#[path = "support/git_alias_bind.rs"]
mod git_alias_bind;

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
        if std::env::args().any(|arg| {
            matches!(
                arg.as_str(),
                "--deny-build-netns"
                    | "--deny-build-mounts"
                    | "--deny-build-devpts"
                    | "--deny-build-identity"
                    | "--deny-build-alias-bind"
                    | "--deny-build-pidns"
                    | "--deny-build-pidns-init"
            )
        }) {
            let deny_mounts = std::env::args().any(|arg| arg == "--deny-build-mounts");
            let deny_devpts = std::env::args().any(|arg| arg == "--deny-build-devpts");
            let deny_identity = std::env::args().any(|arg| arg == "--deny-build-identity");
            // `move_mount` is used only to attach the same-object Git alias, the
            // last mount setup step: scratch, terminals and account files exist by then.
            let deny_alias = std::env::args().any(|arg| arg == "--deny-build-alias-bind");
            // The PID step is its own unshare with exactly this argument, so
            // the combined user/network/mount unshare before it still succeeds.
            let deny_pidns = std::env::args().any(|arg| arg == "--deny-build-pidns");
            // A later failure: the namespace exists and its init cannot finish
            // hardening, so it never reports ready and the relay discards it.
            let deny_init = std::env::args().any(|arg| arg == "--deny-build-pidns-init");
            // The PTY allocator also uses MS_BIND. Target the account's
            // readonly remount so scratch and terminal setup complete first.
            let fault_flags = if deny_identity {
                libc::MS_BIND
                    | libc::MS_REMOUNT
                    | libc::MS_RDONLY
                    | libc::MS_NOSUID
                    | libc::MS_NODEV
                    | libc::MS_NOEXEC
            } else if deny_devpts {
                libc::MS_NOSUID | libc::MS_NOEXEC
            } else {
                0
            };
            let fault_syscall = if deny_alias {
                libc::SYS_move_mount
            } else if deny_mounts || deny_devpts || deny_identity {
                libc::SYS_mount
            } else if deny_init {
                libc::SYS_capset
            } else {
                libc::SYS_unshare
            };
            // The compared syscall argument: mount's flags, or unshare's.
            let (fault_argument, fault_value) = if deny_pidns {
                (0, libc::CLONE_NEWPID as libc::c_ulong)
            } else {
                (3, fault_flags)
            };
            assert!(
                !std::env::args().any(|arg| arg == "--require-build-netns"),
                "cannot both deny and require namespace setup"
            );
            // Test-only fault injection, before starting any runtime threads:
            // Descendants inherit EACCES for namespace or mount setup, even on
            // permissive hosts. A mount failure exercises partial setup after
            // the child has already acquired its private namespaces. Identity
            // injection denies the readonly account remount, after scratch and PTYs.
            // Alias injection denies the attach that follows all of them.
            // This is not a production namespace policy or an escape filter.
            let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
            let mut filters = vec![
                instruction((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0, 0, 0),
                instruction(
                    (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                    0,
                    1,
                    fault_syscall as u32,
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
            if deny_devpts || deny_identity || deny_pidns {
                filters[1].jf = 3;
                filters.splice(
                    2..2,
                    [
                        instruction(
                            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                            0,
                            0,
                            (std::mem::offset_of!(libc::seccomp_data, args)
                                + fault_argument * std::mem::size_of::<u64>())
                                as u32,
                        ),
                        instruction(
                            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                            0,
                            1,
                            fault_value as u32,
                        ),
                    ],
                );
            }
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
            // Neither unshare(0), mount, move_mount nor capset with null operands can
            // mutate the host, and the filter answers unshare(CLONE_NEWPID)
            // before the kernel acts on it. The control must observe the
            // injected errno, independently of host namespace or mount
            // permissions.
            assert_eq!(
                unsafe {
                    libc::syscall(
                        fault_syscall,
                        if deny_pidns { fault_value } else { 0 },
                        0,
                        0,
                        fault_flags,
                        0,
                    )
                },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EACCES)
            );
            eprintln!(
                "{}",
                if deny_devpts {
                    "BUILD_DEVPTS_SETUP_FAILURE_INJECTED"
                } else if deny_mounts {
                    "BUILD_MOUNT_SETUP_FAILURE_INJECTED"
                } else if deny_identity {
                    "BUILD_IDENTITY_SETUP_FAILURE_INJECTED"
                } else if deny_alias {
                    "BUILD_ALIAS_BIND_FAILURE_INJECTED"
                } else if deny_pidns {
                    "BUILD_PIDNS_SETUP_FAILURE_INJECTED"
                } else if deny_init {
                    "BUILD_PIDNS_INIT_SETUP_FAILURE_INJECTED"
                } else {
                    "BUILD_NETNS_SETUP_FAILURE_INJECTED"
                }
            );
        }
        #[cfg(target_os = "macos")]
        let native_fence = agent_bridle::seatbelt_is_supported();
        #[cfg(target_os = "linux")]
        let native_fence = newt_core::confined_exec::kernel_fs_fence_available();
        assert!(
            native_fence,
            "this explicitly selected real-resource test requires a native filesystem fence"
        );
        // The host fixture workspace must survive covering temporary roots.
        // Host scratch sentinels remain in /tmp and /dev/shm to prove isolation.
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target");
        std::fs::create_dir_all(&target).unwrap();
        let root = tempfile::tempdir_in(target).unwrap();
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
            #[cfg(target_os = "macos")]
            if selected("build_tests_can_use_a_private_pty") {
                build_tests_can_use_a_private_pty(&root).await;
            }
            if selected("build_tests_can_bind_a_unix_socket_in_a_tempdir") {
                build_tests_can_bind_a_unix_socket_in_a_tempdir(&root).await;
            }
            #[cfg(target_os = "linux")]
            if selected("build_tests_granted_temporary_root_remains_visible") {
                build_tests_granted_temporary_root_remains_visible(&root).await;
            }
            #[cfg(target_os = "linux")]
            if selected("build_tests_temporary_workspace_retains_fallback") {
                let covered = tempfile::tempdir_in("/tmp").unwrap();
                build_network_probe(
                    covered.path(),
                    "build_tests_temporary_workspace_retains_fallback",
                    "--build-scratch-probe",
                    "BUILD_PRIVATE_SCRATCH_CONFIRMED",
                )
                .await;
            }
            #[cfg(target_os = "linux")]
            if selected("build_tests_git_alias_is_the_granted_primary") {
                use super::git_alias_bind::{object, pair};
                let host = || pair().map(|(primary, alias)| (object(&primary), object(&alias)));
                let before = host();
                build_network_probe(
                    &root,
                    "build_tests_git_alias_is_the_granted_primary",
                    "--build-git-alias-probe",
                    "BUILD_GIT_ALIAS_IS_THE_GRANTED_PRIMARY",
                )
                .await;
                assert_eq!(before, host(), "the host alias must be untouched");
            }
            #[cfg(target_os = "linux")]
            for (name, probe, marker) in [
                (
                    "build_tests_can_use_a_private_pty",
                    "--build-pty-probe",
                    "BUILD_PTY_TEST_PASSED",
                ),
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
                (
                    "build_tests_private_scratch_capacity_mode_and_isolation",
                    "--build-scratch-probe",
                    "BUILD_PRIVATE_SCRATCH_CONFIRMED",
                ),
                (
                    "build_tests_private_identity_isolated",
                    "--build-identity-probe",
                    "BUILD_PRIVATE_IDENTITY_CONFIRMED",
                ),
            ] {
                if selected(name) {
                    build_network_probe(&root, name, probe, marker).await;
                }
            }
            #[cfg(target_os = "linux")]
            if selected("git_alias_file_bind_carries_a_nested_exec_rule") {
                super::git_alias_bind::ground(&root);
            }
            #[cfg(target_os = "linux")]
            if selected("build_tests_private_pid_namespace") {
                build_tests_private_pid_namespace(&root);
            }
            if selected("denied_build_runs_no_pipeline_stage_or_redirection") {
                denied_build_runs_no_pipeline_stage_or_redirection(&root).await;
            }
            if selected("cached_build_formats_with_native_cargo_subcommand") {
                cached_build_formats_with_native_cargo_subcommand(&root).await;
            }
            #[cfg(target_os = "linux")]
            if selected("worker_result_channel_is_not_reopenable") {
                worker_result_channel_is_not_reopenable(&root).await;
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
        for directory in ["bin", "registry", "git"] {
            std::fs::create_dir(cargo_home.join(directory)).unwrap();
        }
        for config in ["config", "config.toml"] {
            std::fs::write(cargo_home.join(config), "").unwrap();
        }
        let bin_home = home.join(".local/bin");
        std::fs::create_dir_all(&bin_home).unwrap();
        std::env::set_var("XDG_BIN_HOME", bin_home);
        std::env::set_var("HOME", &home);
        std::env::set_var("USER", "untrusted-build-user");
        std::env::set_var("LOGNAME", "untrusted-build-login");
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
        std::fs::create_dir(workspace.join("target")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let secret = outside.join("secret");
        let forbidden = outside.join("write");
        std::fs::write(&secret, "sibling-secret-sentinel").unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\n[package]\nname = \"brush-build-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
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

    /// Grounds the mocked covered-path fallback decision and Build extra-write
    /// grants and reported mode on the argv and Brush routes: successful writes must
    /// survive on the host, rather than disappear with a private tmpfs.
    #[cfg(target_os = "linux")]
    async fn build_tests_granted_temporary_root_remains_visible(root: &Path) {
        use newt_core::confined_exec::build_tool_caveats_with_writes;
        let (workspace, _) = project(root, "granted-temporary-root");
        let granted = tempfile::tempdir_in("/tmp").unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut caveats = build_tool_caveats_with_writes(
            &workspace,
            &[granted.path().to_str().unwrap().to_owned()],
        );
        if let Scope::Only(reads) = &mut caveats.fs_read {
            reads.insert(executable.to_str().unwrap().to_owned());
        }
        use agent_bridle::Tool;
        let policy = std::sync::Arc::new(agent_bridle::SandboxPolicy {
            child_network: agent_bridle::ChildNetworkPolicy::PrivateBuild,
            ..Default::default()
        });
        let brush = agent_bridle::BrushShellTool::new().with_sandbox_policy(policy.clone());
        let cx = agent_bridle::Gate::new(0)
            .authorize(&brush, &caveats)
            .unwrap();
        use std::os::unix::fs::MetadataExt;
        let mounts = [
            "/proc/self/ns/mnt",
            "/tmp",
            "/dev/shm",
            "/dev/pts",
            "/dev/ptmx",
            "/etc/passwd",
            "/etc/group",
        ]
        .map(|path| {
            let metadata = std::fs::metadata(path).unwrap();
            format!("{}:{}", metadata.dev(), metadata.ino())
        })
        .join(",");
        let mounts = format!("--host-mounts={mounts}");
        let mut visible = Vec::new();
        for route in ["argv", "Brush"] {
            let path = granted.path().join(route);
            let probe = format!("--build-granted-write-probe={}", path.display());
            let output = if route == "argv" {
                let output = agent_bridle::ConfinedCommand::new(executable.to_str().unwrap())
                    .args([probe.as_str(), mounts.as_str()])
                    .current_dir(&workspace)
                    .sandbox_policy(policy.clone())
                    .stdin(agent_bridle::ConfinedStdio::Null)
                    .stdout(agent_bridle::ConfinedStdio::Piped)
                    .stderr(agent_bridle::ConfinedStdio::Piped)
                    .spawn(&cx)
                    .unwrap();
                assert_eq!(
                    output.build_isolation,
                    Some(agent_bridle::BuildIsolation::DenyAllFallback),
                    "covered inputs select fallback before any retry"
                );
                let output = output.child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            } else {
                // run_command deliberately rebuilds narrower Build grants. Use
                // its existing Brush mechanism directly to retain this extra root.
                let source = format!(
                    "cargo --version && '{}' {probe} {mounts}",
                    executable.display()
                );
                let output = brush
                    .invoke(serde_json::json!({"cmd": source, "cwd": workspace, "env": {"HOME": workspace}}), &cx)
                    .await
                    .unwrap();
                assert_eq!(output["exit_code"], 0, "{output}");
                format!(
                    "{}\n{}",
                    output["stdout"].as_str().unwrap(),
                    output["stderr"].as_str().unwrap()
                )
            };
            assert!(output.contains("BUILD_GRANTED_WRITE_COMPLETED"), "{output}");
            let persisted = std::fs::read(&path).is_ok_and(|bytes| bytes == b"granted host write");
            let host_mounts = output.contains("BUILD_HOST_MOUNTS_CONFIRMED");
            eprintln!("build_tests_granted_temporary_root_remains_visible ({route}): host write visible={persisted}; host mounts={host_mounts}");
            visible.push(persisted && host_mounts && output.contains("deny-all fallback"));
        }
        assert!(
            visible.into_iter().all(|persisted| persisted),
            "both granted writes must survive on the host in fallback mode"
        );
    }

    #[cfg(target_os = "linux")]
    fn assert_build_mode(diagnostic: &str, name: &str, route: &str) {
        for (switch, mode) in [
            (
                "--require-build-netns",
                if name == "build_tests_temporary_workspace_retains_fallback" {
                    "deny-all fallback"
                } else {
                    "private loopback only"
                },
            ),
            ("--deny-build-netns", "deny-all fallback"),
            ("--deny-build-mounts", "deny-all fallback"),
            ("--deny-build-devpts", "deny-all fallback"),
            ("--deny-build-identity", "deny-all fallback"),
            ("--deny-build-alias-bind", "deny-all fallback"),
        ] {
            if std::env::args().any(|arg| arg == switch) {
                assert!(
                    diagnostic.contains(mode),
                    "{name} ({route}) requires {mode}: {diagnostic}"
                );
                eprintln!("{name} ({route}): required {mode} measured");
            }
        }
        if std::env::args().any(|arg| {
            matches!(
                arg.as_str(),
                "--deny-build-pidns" | "--deny-build-pidns-init"
            )
        }) {
            let mode = if pidns_argv_enabled()
                || name == "build_tests_temporary_workspace_retains_fallback"
            {
                "deny-all fallback"
            } else {
                "private loopback only"
            };
            assert!(diagnostic.contains(mode), "{name} ({route}): {diagnostic}");
            eprintln!("{name} ({route}): required {mode} measured");
        }
    }

    // Test-only opt-in: production request policy remains unchanged. The argv
    // funnel can exercise every existing probe before enabling PID isolation.
    #[cfg(target_os = "linux")]
    fn pidns_argv_enabled() -> bool {
        std::env::args().any(|arg| arg == "--test-private-pidns-argv")
    }

    #[cfg(target_os = "linux")]
    fn argv_build_probe(workspace: &Path, name: &str, probe: &str) -> (bool, Vec<u8>, Vec<u8>) {
        use newt_core::confined_exec::{build_tool_request, ConstrainedExecutor};
        let executable = std::env::current_exe().unwrap();
        let request = build_tool_request(
            workspace,
            workspace,
            executable.to_str().unwrap(),
            [probe, "--build-network-auto"],
            &Scope::none(),
        );
        if !pidns_argv_enabled() {
            let output = ConstrainedExecutor::run(&request).unwrap();
            return (output.success, output.stdout, output.stderr);
        }
        // Owned short-path scratch survives private /tmp, including fallback.
        // The existing probe still checks that its run directory is removed.
        let scratch = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut caveats = request.caveats().clone();
        // Match the existing PID probe's exact executable-image read grant;
        // ConstrainedExecutor normally adds this while resolving its guard.
        if let Scope::Only(reads) = &mut caveats.fs_read {
            reads.insert(executable.to_str().unwrap().to_owned());
            reads.insert(scratch.path().to_str().unwrap().to_owned());
        }
        if let Scope::Only(writes) = &mut caveats.fs_write {
            writes.insert(scratch.path().to_str().unwrap().to_owned());
        }
        let cx = agent_bridle::Gate::new(0)
            .authorize(&agent_bridle::ShellTool::default(), &caveats)
            .unwrap();
        let policy = agent_bridle::SandboxPolicy {
            child_network: agent_bridle::ChildNetworkPolicy::PrivateBuild,
            private_pid_namespace: true,
            ..Default::default()
        };
        let mut command = agent_bridle::ConfinedCommand::new(executable.to_str().unwrap())
            .args([probe, "--build-network-auto"])
            .current_dir(workspace)
            .sandbox_policy(std::sync::Arc::new(policy))
            .new_process_group()
            .stdin(agent_bridle::ConfinedStdio::Null)
            .stdout(agent_bridle::ConfinedStdio::Piped)
            .stderr(agent_bridle::ConfinedStdio::Piped);
        for (key, value) in request.env_grants() {
            command = command.env(key, value);
        }
        for name in ["HOME", "TMPDIR", "TMP", "TEMP"] {
            command = command.env(name, scratch.path());
        }
        let child = command.spawn(&cx).unwrap();
        let isolation = child
            .build_isolation
            .expect("private build reports its mode");
        if std::env::args().any(|arg| arg.starts_with("--deny-build-")) {
            assert_eq!(isolation, agent_bridle::BuildIsolation::DenyAllFallback);
        } else if std::env::args().any(|arg| arg == "--require-build-netns") {
            assert_eq!(
                isolation,
                if name == "build_tests_temporary_workspace_retains_fallback" {
                    agent_bridle::BuildIsolation::DenyAllFallback
                } else {
                    agent_bridle::BuildIsolation::Private
                }
            );
        }
        let output = child.child.wait_with_output().unwrap();
        eprintln!("PIDNS_ARGV_STAGING_ON: {probe}: {isolation:?}");
        (output.status.success(), output.stdout, output.stderr)
    }

    /// Grounds the mocked build-network scope contract in real socket calls
    /// through both build routes: a private namespace supports loopback with
    /// no external route; unavailable namespaces retain the socket-family deny.
    /// Also grounds the inherited-descriptor floor with a real host TCP socket.
    /// The scratch twin grounds the Build write-root contract: private tmpfs
    /// capacity, caller ownership and mode, plus the mocked staging trust walk's
    /// rejection of writable children/ancestors. Round trips cannot observe or
    /// replace host sentinels or change host modes; fallback denies writes.
    /// The PTY twin grounds terminal availability and isolation, including the
    /// real master/slave transport, ioctl contract and per-instance allocation bound.
    /// The identity twin grounds fdguard's mocked account-line validation in
    /// real NSS lookups, readonly object grants and unchanged host databases.
    /// `--require-build-netns` rejects fallback except for the deliberately
    /// covered workspace; `--deny-build-netns` forces setup to fail.
    /// `--deny-build-identity` forces a later failure at the account remount.
    /// The Git alias twin (`support/git_alias_bind.rs`) grounds the same-object
    /// binding; `--deny-build-alias-bind` fails its attach before the PID step,
    /// and the retried child must then report no binding at all.
    #[cfg(target_os = "linux")]
    async fn build_network_probe(root: &Path, name: &str, probe: &str, marker: &str) {
        use newt_core::confined_exec::build_tool_caveats;

        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::fs::MetadataExt;
        // Required-private probes need an uncovered scratch root. Keep its
        // spelling short enough for the Unix socket probe's sockaddr_un.
        let scratch_root = tempfile::Builder::new()
            .prefix("")
            .rand_bytes(3)
            .tempdir_in("/var/tmp")
            .unwrap();
        let previous_scratch = std::env::var_os("NEWT_BUILD_SCRATCH_DIR");
        std::env::set_var("NEWT_BUILD_SCRATCH_DIR", scratch_root.path());
        let host_pty = if probe == "--build-pty-probe" {
            // Allocate our own host canary; never use the operator's terminal.
            let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
            assert!(fd >= 0);
            let master = unsafe { OwnedFd::from_raw_fd(fd) };
            assert_eq!(unsafe { libc::grantpt(fd) }, 0);
            assert_eq!(unsafe { libc::unlockpt(fd) }, 0);
            let name = unsafe { libc::ptsname(fd) };
            assert!(!name.is_null());
            let name = unsafe { std::ffi::CStr::from_ptr(name) }
                .to_str()
                .unwrap()
                .to_owned();
            let fd = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_DUPFD, 201) };
            assert!(fd >= 201);
            Some((
                unsafe { OwnedFd::from_raw_fd(fd) },
                name,
                std::fs::metadata("/dev/pts").unwrap().dev(),
            ))
        } else {
            None
        };
        let scratch_canaries: Vec<_> = if probe == "--build-scratch-probe" {
            ["/tmp", "/dev/shm"]
                .into_iter()
                .map(|directory| {
                    let file = tempfile::NamedTempFile::new_in(directory).unwrap();
                    std::fs::write(file.path(), b"host scratch sentinel").unwrap();
                    assert!(!file.path().with_extension("child").exists());
                    file
                })
                .collect()
        } else {
            Vec::new()
        };
        let host_metadata: Vec<_> = scratch_canaries
            .iter()
            .map(|file| std::fs::metadata(file.path().parent().unwrap()).unwrap())
            .collect();
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
        } else if let Some((fd, path, device)) = &host_pty {
            format!("{probe}={device}:{path}:{}", fd.as_raw_fd())
        } else if !scratch_canaries.is_empty() {
            format!(
                "{probe}={}:{}:{}",
                scratch_canaries[0]
                    .path()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap(),
                scratch_canaries[1]
                    .path()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap(),
                // SAFETY: geteuid has no preconditions or memory effects.
                unsafe { libc::geteuid() }
            )
        } else {
            probe.to_owned()
        };
        let (workspace, outside) = project(root, name);
        let identity_paths = ["/etc/passwd", "/etc/group"];
        let host_identity = (probe == "--build-identity-probe")
            .then(|| identity_paths.map(|path| std::fs::read(path).unwrap()));
        if probe == "--build-identity-probe" {
            // Resolve before confinement, with HOME already poisoned by isolate().
            // Compare without printing account names, home paths, or shell paths.
            let mut buffer = vec![0; 65536];
            let mut user: libc::passwd = unsafe { std::mem::zeroed() };
            let mut group: libc::group = unsafe { std::mem::zeroed() };
            let mut user_ptr = std::ptr::null_mut();
            let mut group_ptr = std::ptr::null_mut();
            let uid = unsafe { libc::geteuid() };
            let gid = unsafe { libc::getegid() };
            assert_eq!(
                unsafe {
                    libc::getpwuid_r(
                        uid,
                        &mut user,
                        buffer.as_mut_ptr(),
                        buffer.len(),
                        &mut user_ptr,
                    )
                },
                0
            );
            assert!(!user_ptr.is_null());
            let passwd = unsafe {
                vec![
                    std::ffi::CStr::from_ptr(user.pw_name).to_bytes().to_vec(),
                    b"x".to_vec(),
                    uid.to_string().into_bytes(),
                    gid.to_string().into_bytes(),
                    Vec::new(),
                    std::ffi::CStr::from_ptr(user.pw_dir).to_bytes().to_vec(),
                    std::ffi::CStr::from_ptr(user.pw_shell).to_bytes().to_vec(),
                ]
            };
            assert_eq!(
                unsafe {
                    libc::getgrgid_r(
                        gid,
                        &mut group,
                        buffer.as_mut_ptr(),
                        buffer.len(),
                        &mut group_ptr,
                    )
                },
                0
            );
            assert!(!group_ptr.is_null());
            let group = unsafe {
                vec![
                    std::ffi::CStr::from_ptr(group.gr_name).to_bytes().to_vec(),
                    b"x".to_vec(),
                    gid.to_string().into_bytes(),
                    Vec::new(),
                ]
            };
            // Pick from the host file, excluding the caller and NSS constants,
            // and prove that the same uid lookup succeeds before confinement.
            // Only its uid enters the fixture, never its account fields.
            let other = host_identity.as_ref().unwrap()[0]
                .split(|byte| *byte == b'\n')
                .find_map(|line| {
                    let candidate = std::str::from_utf8(line.split(|byte| *byte == b':').nth(2)?)
                        .ok()?
                        .parse::<libc::uid_t>()
                        .ok()?;
                    if [uid, 0, 65534].contains(&candidate) {
                        return None;
                    }
                    let rc = unsafe {
                        libc::getpwuid_r(
                            candidate,
                            &mut user,
                            buffer.as_mut_ptr(),
                            buffer.len(),
                            &mut user_ptr,
                        )
                    };
                    (rc == 0 && !user_ptr.is_null() && user.pw_uid == candidate)
                        .then_some(candidate)
                })
                .expect("fixture requires another real host account outside NSS built-in uids");
            eprintln!("HOST_ACCOUNT_LOOKUP_CONTROL_CONFIRMED");
            std::fs::write(
                workspace.join("identity-expected.json"),
                serde_json::to_vec(&([passwd, group], other)).unwrap(),
            )
            .unwrap();
        }
        let executable = std::env::current_exe().unwrap();
        let (argv_success, argv_stdout, argv_stderr) = argv_build_probe(&workspace, name, &probe);
        let source = format!(
            "cargo build --lib --offline && '{}' {probe} --build-network-auto",
            executable.display()
        );
        let brush = if pidns_argv_enabled() {
            None
        } else {
            Some(
                execute_tool(
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
                .await,
            )
        };
        let stdout = String::from_utf8_lossy(&argv_stdout);
        let stderr = String::from_utf8_lossy(&argv_stderr);
        let routes = std::iter::once(("argv", stdout.as_ref(), stderr.as_ref())).chain(
            brush
                .as_ref()
                .map(|text| ("Brush", text.as_str(), text.as_str())),
        );
        for (route, output, diagnostic) in routes {
            let unbound = output.contains("BUILD_GIT_ALIAS_UNBOUND_FALLBACK");
            let fallback = output.contains("BUILD_LOOPBACK_DENIED_FALLBACK")
                || output.contains("BUILD_SCRATCH_DENIED_FALLBACK")
                || output.contains("BUILD_PTY_DENIED_FALLBACK")
                || output.contains("BUILD_IDENTITY_DENIED_FALLBACK")
                || unbound;
            assert!(
                output.contains(marker)
                    || ((name == "build_tests_loopback_roundtrip"
                        || !scratch_canaries.is_empty()
                        || host_pty.is_some()
                        || name == "build_tests_private_identity_isolated"
                        || unbound)
                        && fallback),
                "{name} ({route}): {output}\n{diagnostic}"
            );
            assert!(
                diagnostic.contains("Build isolation:"),
                "missing effective network mode ({route}): {diagnostic}"
            );
            assert_build_mode(diagnostic, name, route);
            if fallback {
                assert!(diagnostic.contains("deny-all fallback"), "{diagnostic}");
            } else if name == "build_tests_loopback_roundtrip" {
                assert!(diagnostic.contains("private loopback only"), "{diagnostic}");
            }
            if name == "build_tests_temporary_workspace_retains_fallback" {
                assert!(
                    fallback && diagnostic.contains("deny-all fallback"),
                    "{diagnostic}"
                );
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
        assert!(argv_success, "{stderr}");
        if probe == "--build-unix-socket-probe" {
            let scratch = PathBuf::from(
                std::fs::read_to_string(workspace.join("socket-scratch-path")).unwrap(),
            );
            assert!(!scratch.starts_with(&workspace));
            assert!(!scratch.exists(), "normal completion must remove scratch");
        }

        if let Some(before) = host_identity {
            for (path, contents) in identity_paths.into_iter().zip(before) {
                assert!(
                    std::fs::read(path).unwrap() == contents,
                    "host account database must be unchanged"
                );
            }
        }
        if let Some((host, path, device)) = host_pty {
            // The production routes use ABI 3. Exercise the configurable ABI 5
            // mask too: private terminal ioctls need IOCTL_DEV there. An older
            // kernel must fall back when the raw post-mount rule is unsupported.
            let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, 0, 0, 1) };
            assert!(abi >= 3);
            let mut caveats = build_tool_caveats(&workspace);
            // The raw argv check below owns no executor scratch lease.
            std::fs::create_dir_all(newt_core::confined_exec::build_scratch_root(&workspace))
                .unwrap();
            if let Scope::Only(reads) = &mut caveats.fs_read {
                reads.insert(executable.to_str().unwrap().to_owned());
            }
            let cx = agent_bridle::Gate::new(0)
                .authorize(&agent_bridle::ShellTool::default(), &caveats)
                .unwrap();
            for terminal_stdio in [None, Some(0), Some(1), Some(2)] {
                let policy = agent_bridle::SandboxPolicy {
                    child_network: agent_bridle::ChildNetworkPolicy::PrivateBuild,
                    private_pid_namespace: pidns_argv_enabled(),
                    landlock_abi_floor: if terminal_stdio.is_some() { 3 } else { 5 },
                    ..Default::default()
                };
                let mut command = agent_bridle::ConfinedCommand::new(executable.to_str().unwrap())
                    .args([probe.as_str(), "--build-network-auto"])
                    .current_dir(&workspace)
                    .sandbox_policy(std::sync::Arc::new(policy))
                    .stdin(agent_bridle::ConfinedStdio::Null)
                    .stdout(agent_bridle::ConfinedStdio::Piped)
                    .stderr(agent_bridle::ConfinedStdio::Piped);
                if let Some(fd) = terminal_stdio {
                    let terminal = std::process::Stdio::from(host.try_clone().unwrap());
                    command = match fd {
                        0 => command.stdin(terminal),
                        1 => command.stdout(terminal),
                        _ => command.stderr(terminal),
                    };
                }
                let child = command.spawn(&cx);
                if let Some(fd) = terminal_stdio {
                    let error = child.expect_err("host terminal stdio must refuse before exec");
                    assert!(error.to_string().contains("Permission denied"), "{error}");
                    eprintln!("private build refuses host terminal on stdio {fd}");
                } else {
                    let output = child.unwrap().child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let output = String::from_utf8_lossy(&output.stdout);
                    assert!(
                        output.contains(marker) || output.contains("BUILD_PTY_DENIED_FALLBACK"),
                        "{output}"
                    );
                    if abi < 5 {
                        assert!(output.contains("BUILD_PTY_DENIED_FALLBACK"), "{output}");
                    } else if std::env::args().any(|arg| arg == "--require-build-netns") {
                        assert!(
                            output.contains(marker),
                            "ABI 5 must not fall back: {output}"
                        );
                    }
                    eprintln!(
                        "private PTY ABI 5 request on kernel ABI {abi}: {}",
                        output.trim()
                    );
                }
            }
            assert_eq!(
                std::fs::metadata(path).unwrap().dev(),
                device,
                "host PTY changed"
            );
        }
        for (file, before) in scratch_canaries.into_iter().zip(host_metadata) {
            let after = std::fs::metadata(file.path().parent().unwrap()).unwrap();
            assert_eq!(
                (after.uid(), after.gid(), after.mode()),
                (before.uid(), before.gid(), before.mode()),
                "private scratch must not change host ownership or mode"
            );
            assert_eq!(
                std::fs::read(file.path()).unwrap(),
                b"host scratch sentinel"
            );
            assert!(
                !file.path().with_extension("child").exists(),
                "private child file leaked to the host"
            );
        }
        assert!(!outside.join("write").exists());
        match previous_scratch {
            Some(value) => std::env::set_var("NEWT_BUILD_SCRATCH_DIR", value),
            None => std::env::remove_var("NEWT_BUILD_SCRATCH_DIR"),
        }
        eprintln!(
            "test {name} ... ok ({})",
            if pidns_argv_enabled() {
                "argv; PID staging on"
            } else {
                "argv and Brush"
            }
        );
    }

    /// Grounds fdguard's mocked relay decision and `clone_args` layout, and
    /// core's mocked fallback-flag mode, in real processes on both spawn funnels
    /// with the staging switch on (`docs/design/private-pid-namespace.md`).
    /// The payload is PID 2 with no visible parent and leads its own group;
    /// the harness is not nameable; `spawn` returns while the payload is still
    /// blocked on a pipe this test holds; killing only the spawned child ends
    /// every member, including one in its own session. The Brush worker starts
    /// under the same funnel and its bootstrap is refused on the reported PID
    /// until authentication is namespace-aware. `--deny-build-pidns` fails the
    /// PID step alone and `--deny-build-pidns-init` fails init's hardening
    /// inside the new namespace; each must select the deny-all fallback on
    /// both funnels.
    #[cfg(target_os = "linux")]
    fn build_tests_private_pid_namespace(root: &Path) {
        use agent_bridle::{BuildIsolation, ConfinedCommand, ConfinedStdio, SandboxedWorker};
        use std::io::{BufRead, Read};

        let name = "build_tests_private_pid_namespace";
        // Like the other private-mode probes, use an uncovered scratch root:
        // main's covered-input decision must remain in force with PID staging on.
        let previous_scratch = std::env::var_os("NEWT_BUILD_SCRATCH_DIR");
        std::env::set_var("NEWT_BUILD_SCRATCH_DIR", root.join("private-pid-scratch"));
        let (workspace, _) = project(root, name);
        let executable = std::env::current_exe().unwrap();
        let mut caveats = newt_core::confined_exec::build_tool_caveats(&workspace);
        if let Scope::Only(reads) = &mut caveats.fs_read {
            reads.insert(executable.to_str().unwrap().to_owned());
        }
        let cx = agent_bridle::Gate::new(0)
            .authorize(&agent_bridle::ShellTool::default(), &caveats)
            .unwrap();
        let policy = std::sync::Arc::new(agent_bridle::SandboxPolicy {
            child_network: agent_bridle::ChildNetworkPolicy::PrivateBuild,
            private_pid_namespace: true,
            ..Default::default()
        });
        let switch = |switch: &str| std::env::args().any(|arg| arg == switch);
        // The trusted record must agree with what the child measured and with
        // the mode line. Any injected setup failure means fallback; otherwise
        // `--require-build-netns` rejects one.
        let expect = |isolation: Option<BuildIsolation>, stdout: &str, stderr: &str| {
            let isolation = isolation.expect("a private build reports its mode");
            if std::env::args().any(|arg| arg.starts_with("--deny-build-")) {
                assert_eq!(isolation, BuildIsolation::DenyAllFallback);
            } else if switch("--require-build-netns") {
                assert_eq!(isolation, BuildIsolation::Private, "{stderr}");
            }
            let (marker, mode) = match isolation {
                BuildIsolation::Private => {
                    ("BUILD_PRIVATE_PIDNS_CONFIRMED", "private loopback only")
                }
                BuildIsolation::DenyAllFallback => {
                    ("BUILD_PIDNS_DENIED_FALLBACK", "deny-all fallback")
                }
            };
            assert!(stdout.contains(marker), "{isolation:?}: {stdout}\n{stderr}");
            assert!(stderr.contains(mode), "{isolation:?}: {stderr}");
            isolation
        };
        let command = |arguments: &[String]| {
            ConfinedCommand::new(executable.to_str().unwrap())
                .args(arguments)
                .current_dir(&workspace)
                .sandbox_policy(policy.clone())
                .stdin(ConfinedStdio::Null)
                .stdout(ConfinedStdio::Piped)
                .stderr(ConfinedStdio::Piped)
                .new_process_group()
        };
        let probe = format!("--build-pid-probe={}", std::process::id());

        // argv funnel, normal exit: the relay mirrors the payload's status.
        let confined = command(std::slice::from_ref(&probe)).spawn(&cx).unwrap();
        let output = confined.child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        let isolation = expect(confined.build_isolation, &stdout, &stderr);
        eprintln!("{name} (argv): {isolation:?}: {}", stdout.trim());

        // The private mechanisms installed before the PID step still hold
        // for a payload that is PID 2.
        for (existing, marker) in [
            (
                "--build-loopback-probe",
                "BUILD_LOOPBACK_ROUNDTRIP_CONFIRMED",
            ),
            (
                "--build-external-connect-probe",
                "BUILD_EXTERNAL_CONNECT_DENIED",
            ),
        ] {
            let confined = command(&[existing.to_owned(), "--build-network-auto".to_owned()])
                .spawn(&cx)
                .unwrap();
            let output = confined.child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success()
                    && (stdout.contains(marker)
                        || stdout.contains("BUILD_LOOPBACK_DENIED_FALLBACK")),
                "{existing}: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(confined.build_isolation, Some(isolation));
            eprintln!("{name} (argv): {existing} under {isolation:?} ... ok");
        }

        // `spawn` returns at the payload's exec, not at its exit: this test
        // holds the only write end of the payload's stdin and has not closed
        // it. A relay that kept std's exec-status pipe open would hang here.
        let (gate, held_open) = std::io::pipe().unwrap();
        let mut confined = command(&[format!("{probe}:hold")])
            .stdin(std::process::Stdio::from(gate))
            .spawn(&cx)
            .unwrap();
        let mut stdout = std::io::BufReader::new(confined.child.stdout.take().unwrap());
        let mut reported = String::new();
        let lines = if confined.build_isolation == Some(BuildIsolation::Private) {
            2
        } else {
            1
        };
        for _ in 0..lines {
            stdout.read_line(&mut reported).unwrap();
        }
        assert!(
            confined.child.try_wait().unwrap().is_none(),
            "the payload is blocked on a pipe this test still holds"
        );
        eprintln!("{name} (argv): spawn returned while the payload is blocked");
        // Signal only the spawned child. In a namespace that is the relay; the
        // payload and its detached descendant hold stdout and are still
        // blocked, so end-of-file can only come from the namespace ending.
        confined.child.kill().unwrap();
        let status = confined.child.wait().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGKILL)
        );
        stdout.read_to_string(&mut reported).unwrap();
        let mut stderr = String::new();
        confined
            .child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert_eq!(
            reported.contains("BUILD_PIDNS_DESCENDANT_DETACHED"),
            expect(confined.build_isolation, &reported, &stderr) == BuildIsolation::Private,
            "{reported}"
        );
        drop(held_open);
        eprintln!("{name} (argv): killing the spawned child ended every member");

        // Brush launch, same funnel. First: the bootstrap.
        let mut worker = SandboxedWorker::brush()
            .sandbox_policy(policy.clone())
            .spawn(&cx, "pid-namespace-probe", &workspace)
            .unwrap();
        let refusal = worker
            .send_payload(&serde_json::json!({}), Duration::from_secs(30))
            .expect_err("an empty payload is never a Brush request")
            .to_string();
        let _ = worker.child.kill();
        let mut stderr = String::new();
        worker
            .child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        worker.child.wait().unwrap();
        let isolation = worker.build_isolation.expect("worker mode");
        // The worker reports the PID it sees: 2 inside the namespace, while
        // the spawned child is the relay. PR 4 of the design replaces this
        // refusal with kernel-credential authentication.
        let mismatch = format!(
            "worker hello PID mismatch: spawned {}, reported 2",
            worker.child.id()
        );
        assert_eq!(
            (
                refusal.contains(&mismatch),
                refusal.contains("PID mismatch")
            ),
            (
                isolation == BuildIsolation::Private,
                isolation == BuildIsolation::Private
            ),
            "{isolation:?}: {refusal}"
        );
        let marker = match isolation {
            BuildIsolation::Private => "BUILD_PRIVATE_PIDNS_CONFIRMED",
            BuildIsolation::DenyAllFallback => "BUILD_PIDNS_DENIED_FALLBACK",
        };
        expect(Some(isolation), marker, &stderr);
        eprintln!("{name} (Brush): {isolation:?}: {refusal}");

        // Second: a worker still waiting for its bootstrap. This test keeps
        // the control channel open, so the worker cannot end on its own.
        let mut worker = SandboxedWorker::brush()
            .sandbox_policy(policy.clone())
            .spawn(&cx, "pid-namespace-probe", &workspace)
            .unwrap();
        let mut stdout = worker.child.stdout.take().unwrap();
        worker.child.kill().unwrap();
        let status = worker.child.wait().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGKILL)
        );
        stdout.read_to_end(&mut Vec::new()).unwrap();
        assert_eq!(worker.build_isolation, Some(isolation));
        drop(worker);
        eprintln!("{name} (Brush): killing the spawned child ended the worker");
        eprintln!("test {name} ... ok (argv and Brush funnels)");
        match previous_scratch {
            Some(value) => std::env::set_var("NEWT_BUILD_SCRATCH_DIR", value),
            None => std::env::remove_var("NEWT_BUILD_SCRATCH_DIR"),
        }
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
                // The fixture creates `target` up front; denial leaves it empty.
                assert!(
                    std::fs::read_dir(workspace.join("target"))
                        .unwrap()
                        .next()
                        .is_none(),
                    "{output}"
                );
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
        #[cfg(target_os = "linux")]
        {
            // Keep the unrestricted-network permission case below intact and
            // separately exercise private/fallback mode on both build routes.
            build_network_probe(
                root,
                "build_tests_can_bind_a_unix_socket_in_a_tempdir",
                "--build-unix-socket-probe",
                "BUILD_UNIX_SOCKET_ROUNDTRIP_CONFIRMED",
            )
            .await;
            if pidns_argv_enabled() {
                return;
            }
        }
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
    #[cfg(target_os = "macos")]
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

    /// Real kernel grounding for the framing reader's one authoritative result:
    /// both output descriptors refuse pathname reopens, while the framed result
    /// and bounded diagnostic mode copy still arrive in private and fallback mode.
    #[cfg(target_os = "linux")]
    async fn worker_result_channel_is_not_reopenable(root: &Path) {
        use newt_core::confined_exec::build_tool_caveats;
        let (workspace, _) = project(root, "worker-result-channel");
        // Keep required-private inputs outside the roots replaced by private
        // mounts; the default managed scratch correctly selects fallback.
        let previous_scratch = std::env::var_os("NEWT_BUILD_SCRATCH_DIR");
        std::env::set_var("NEWT_BUILD_SCRATCH_DIR", root.join("worker-result-scratch"));
        let executable = std::env::current_exe().unwrap();
        let source = format!(
            "cargo build --lib --offline && '{}' --worker-result-channel-probe",
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
        match previous_scratch {
            Some(value) => std::env::set_var("NEWT_BUILD_SCRATCH_DIR", value),
            None => std::env::remove_var("NEWT_BUILD_SCRATCH_DIR"),
        }
        assert!(brush.contains("WORKER_RESULT_STDOUT_INTACT"), "{brush}");
        assert!(brush.contains("WORKER_RESULT_STDERR_INTACT"), "{brush}");
        let private = brush.contains("Build isolation: private loopback only");
        let fallback = brush.contains("Build isolation: deny-all fallback");
        assert!(private || fallback, "missing build mode: {brush}");
        // PID-only denials leave the production worker's default-off PID
        // policy unchanged; use the shared per-switch mode expectations.
        assert_build_mode(&brush, "worker_result_channel_is_not_reopenable", "Brush");
        let mode = if private {
            "private loopback only"
        } else {
            "deny-all fallback"
        };
        eprintln!("worker result and mode intact: {mode}");
        for fd in [1, 2] {
            assert!(
                brush.contains(&format!("WORKER_RESULT_FD{fd}_REOPEN=denied")),
                "{brush}"
            );
        }
        eprintln!("test worker_result_channel_is_not_reopenable ... ok");
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

#[cfg(target_os = "linux")]
#[path = "common/toolchains.rs"]
mod toolchains;

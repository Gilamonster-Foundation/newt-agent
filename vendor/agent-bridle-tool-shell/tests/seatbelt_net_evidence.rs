//! macOS Seatbelt restricted-network fail-closed evidence.
//!
//! Direct `sandbox-exec` mechanism probes live below admission in core and remain
//! useful evidence about the child's own sockets. These tests exercise the
//! end-to-end `ShellTool` path instead: because ambient Mach/XPC authority is not
//! faithfully bounded, every restricted Seatbelt net shape reports Advisory and
//! is refused before the probe process can spawn.
//!
//! Parent-side listeners make an accidental spawn observable; structured denial
//! envelopes must name `net`, report `advisory`, and leave probe stdout empty.
#![cfg(all(target_os = "macos", feature = "macos-seatbelt", feature = "shell"))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_bridle_core::{seatbelt_is_supported, Caveats, Gate, Scope, Tool, ToolContext};
use agent_bridle_tool_shell::ShellTool;

fn ctx(granted: Caveats) -> ToolContext {
    Gate::new(0)
        .authorize(&ShellTool::new(), &granted)
        .expect("authorize")
}

fn unique_temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ab-net-{}-{}-{}",
        tag,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A `python3` probe written to a temp file (read ambiently by the confined
/// child; net-only caveats leave fs/exec unrestricted so the interpreter loads —
/// isolating "does the sandbox deny this socket?" from "can python start?"). It
/// prints `OK` on a successful open/connect and `DENIED …` on a sandbox refusal.
const PROBE: &str = r#"
import socket, sys
kind, target = sys.argv[1], sys.argv[2]
try:
    if kind == "tcp":
        host, port = target.rsplit(":", 1)
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM); s.settimeout(4)
        s.connect((host, int(port)))
    elif kind == "udp":
        host, port = target.rsplit(":", 1)
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.sendto(b"x", (host, int(port)))
    elif kind == "unix":
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(4)
        s.connect(target)
    print("OK")
except (PermissionError, OSError) as e:
    print("DENIED", type(e).__name__, getattr(e, "errno", ""))
"#;

fn write_probe() -> PathBuf {
    let p = unique_temp("probe.py");
    std::fs::write(&p, PROBE).expect("write probe");
    p
}

/// A parent-side loopback TCP listener that accepts one connection and writes a
/// byte. Returns the bound port and a join handle; the OS assigns the port so
/// nothing collides under parallel runs.
fn spawn_loopback_tcp() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        // Accept exactly one connection (the confined attempt or the parent's
        // unblock), write a byte, and exit.
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.write_all(b"y");
        }
    });
    (port, handle)
}

fn require_prerequisites() {
    assert!(
        seatbelt_is_supported(),
        "macOS Seatbelt evidence requires /usr/bin/sandbox-exec"
    );
    assert!(
        std::path::Path::new("/usr/bin/python3").exists(),
        "macOS Seatbelt evidence requires /usr/bin/python3"
    );
}

/// Attempt the probe through the end-to-end ShellTool path. Restricted Seatbelt
/// net must be refused by admission before this child command starts.
async fn run_probe(
    caveats: Caveats,
    probe: &std::path::Path,
    kind: &str,
    target: &str,
) -> serde_json::Value {
    ShellTool::new()
        .invoke(
            serde_json::json!({
                "cmd": format!("python3 {} {} {}", probe.display(), kind, target)
            }),
            &ctx(caveats),
        )
        .await
        .expect("invoke")
}

fn net_none() -> Caveats {
    Caveats {
        net: Scope::none(),
        ..Caveats::top()
    }
}

// ── net:none — every socket family kernel-denied ─────────────────────────────

#[tokio::test]
async fn seatbelt_net_none_denies_udp() {
    require_prerequisites();
    let probe = write_probe();
    let out = run_probe(net_none(), &probe, "udp", "1.1.1.1:53").await;
    assert_eq!(
        out["denied"], true,
        "net:none must refuse before spawn: {out}"
    );
    assert_eq!(out["denials"][0]["kind"], "net", "{out}");
    assert_eq!(out["enforcement"]["net"], "advisory", "{out}");
    assert_eq!(out["stdout"].as_str().unwrap_or_default(), "", "{out}");
    let _ = std::fs::remove_file(&probe);
}

#[tokio::test]
async fn seatbelt_net_none_denies_loopback_tcp_against_a_live_listener() {
    require_prerequisites();
    let probe = write_probe();
    // A REAL listener is accepting on this port: a refusal is therefore the
    // sandbox, not a missing server (the positive control is the live socket).
    let (port, handle) = spawn_loopback_tcp();
    let out = run_probe(net_none(), &probe, "tcp", &format!("127.0.0.1:{port}")).await;
    assert_eq!(
        out["denied"], true,
        "net:none must refuse before spawn: {out}"
    );
    assert_eq!(out["denials"][0]["kind"], "net", "{out}");
    assert_eq!(out["enforcement"]["net"], "advisory", "{out}");
    // Unblock the listener (nothing connected) by connecting from the parent.
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    let _ = handle.join();
    let _ = std::fs::remove_file(&probe);
}

#[tokio::test]
async fn seatbelt_net_none_denies_pathname_af_unix_deputy() {
    require_prerequisites();
    let probe = write_probe();
    // A host deputy on a PATHNAME AF_UNIX socket — the Linux residual, repeated.
    // On macOS `(deny network*)` governs AF_UNIX connect itself (stronger than
    // Linux, where only the fs fence bounds it). The socket path is fs-ambient
    // here (net-only caveats), so a denial is purely the NET axis.
    let sock = unique_temp("deputy.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind unix");
    let sock_for_thread = sock.clone();
    let handle = std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.write_all(b"relayed");
        }
        let _ = sock_for_thread;
    });
    let out = run_probe(net_none(), &probe, "unix", &sock.to_string_lossy()).await;
    assert_eq!(
        out["denied"], true,
        "net:none must refuse before spawn: {out}"
    );
    assert_eq!(out["denials"][0]["kind"], "net", "{out}");
    assert_eq!(out["enforcement"]["net"], "advisory", "{out}");
    // Unblock the deputy thread.
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(&sock) {
        let mut buf = [0u8; 8];
        let _ = s.read(&mut buf);
    }
    let _ = handle.join();
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&probe);
}

// ── loopback-only — loopback REACHABLE, off-box DENIED (same sandbox) ─────────

#[tokio::test]
async fn seatbelt_loopback_only_allows_loopback_but_denies_offbox() {
    require_prerequisites();
    let probe = write_probe();
    // Even the full loopback interface remains unsupported at admission while
    // ambient deputy authority is unbounded.
    let loopback_only = Caveats {
        net: Scope::only(["localhost".to_string()]),
        ..Caveats::top()
    };

    // A single-address loopback grant is likewise Advisory and refused.
    let single = Caveats {
        net: Scope::only(["127.0.0.1".to_string()]),
        ..Caveats::top()
    };
    let single_out = run_probe(single, &probe, "tcp", "1.1.1.1:80").await;
    assert_eq!(single_out["denied"], true, "{single_out}");
    assert_eq!(single_out["denials"][0]["kind"], "net", "{single_out}");
    assert_eq!(
        single_out["enforcement"]["net"], "advisory",
        "every restricted Seatbelt net shape is Advisory: {single_out}"
    );

    // A live loopback listener must not be reached because admission refuses
    // before the probe child starts.
    let (port, handle) = spawn_loopback_tcp();
    let ok = run_probe(
        loopback_only.clone(),
        &probe,
        "tcp",
        &format!("127.0.0.1:{port}"),
    )
    .await;
    assert_eq!(
        ok["denied"], true,
        "full loopback must refuse before spawn: {ok}"
    );
    assert_eq!(ok["denials"][0]["kind"], "net", "{ok}");
    assert_eq!(ok["enforcement"]["net"], "advisory", "{ok}");
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    let _ = handle.join();

    // Off-box use of the same restricted grant is refused at the same boundary.
    let denied = run_probe(loopback_only, &probe, "tcp", "1.1.1.1:80").await;
    assert_eq!(denied["denied"], true, "{denied}");
    assert_eq!(denied["denials"][0]["kind"], "net", "{denied}");
    assert_eq!(denied["enforcement"]["net"], "advisory", "{denied}");
    let _ = std::fs::remove_file(&probe);
}

// ── remote allowlist — honestly below Kernel (SBPL cannot name a host) ────────

#[tokio::test]
async fn seatbelt_general_remote_allowlist_is_not_kernel() {
    require_prerequisites();
    // A general remote host is inexpressible in SBPL, so Bridle must NOT claim a
    // Kernel net witness for it — CONFINED refuses before spawn rather than
    // running under an over-claimed fence.
    let probe = write_probe();
    let remote = Caveats {
        net: Scope::only(["example.com".to_string()]),
        ..Caveats::top()
    };
    let out = run_probe(remote, &probe, "udp", "1.1.1.1:53").await;
    assert_eq!(
        out["denied"], true,
        "remote net must refuse before spawn: {out}"
    );
    assert_eq!(out["denials"][0]["kind"], "net", "{out}");
    assert_eq!(out["enforcement"]["net"], "advisory", "{out}");
    let _ = std::fs::remove_file(&probe);
}

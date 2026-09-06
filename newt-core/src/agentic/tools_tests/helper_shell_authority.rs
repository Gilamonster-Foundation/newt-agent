use super::*;

/// b1 slice 2 — the LIVE attacker-exec path proof: `run_command` →
/// `dispatch_bridled_shell` → agent-bridle `ShellTool` (child_network =
/// `DenyDirect`, 0.7.15) → spawn. Under `net: none` the seccomp egress floor
/// denies the child's AF_INET socket, so a hostile `run_command` has NO
/// off-box socket of any protocol — the shell path finally inheriting the
/// same complete egress floor as the `ConstrainedExecutor` callers. Skips
/// where Landlock / python3 are unavailable (there the confined spawn fails
/// closed — nothing runs unconfined). Real-resource; grounds the mocked
/// bridle-policy wiring in `bridle_registry`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn run_command_child_under_net_none_cannot_open_a_socket_b1() {
    if !crate::confined_exec::kernel_fs_fence_available()
        || !std::path::Path::new("/usr/bin/python3").exists()
    {
        return;
    }
    let caveats = crate::caveats::Caveats {
        exec: crate::caveats::Scope::only(["python3".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };
    let envelope = super::shell::dispatch_bridled_shell(
            serde_json::json!({
                "cmd": r#"python3 -c "import socket; socket.socket(socket.AF_INET, socket.SOCK_DGRAM)""#,
                "cwd": "."
            }),
            &caveats,
            None,
        )
        .await
        .expect("dispatch");
    // The confined path must actually have been taken (else the floor never ran).
    assert_eq!(
        envelope["sandbox_kind"], "landlock",
        "run_command child must be kernel-confined: {envelope}"
    );
    // AF_INET socket creation is seccomp-denied → PermissionError → exit 1.
    // (A net-GRANTED run_command leaves DenyDirect inert; this proves the
    // net:none case is fully fenced — no direct egress of any protocol.)
    assert_ne!(
        envelope["exit_code"], 0,
        "run_command under net:none must deny the child's AF_INET socket (b1): {envelope}"
    );
}

/// Closure-proof: the run_command route allows AF_UNIX (deliberately) and
/// does NOT fence an abstract-namespace `connect()`, so a confined child CAN
/// reach a host abstract-namespace unix-domain deputy — the local-deputy
/// egress path the direct-socket seccomp floor does not close. Pinned so a
/// future fence (netns) that closes it forces the register + public claim to
/// be revisited. (Grounds the honest narrowing: "direct AF_INET/INET6/PACKET
/// denied", NOT "no network egress".)
#[cfg(target_os = "linux")]
#[tokio::test]
async fn run_command_child_can_reach_an_af_unix_abstract_deputy() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    if !crate::confined_exec::kernel_fs_fence_available()
        || !std::path::Path::new("/usr/bin/python3").exists()
    {
        return;
    }
    let name = format!("newt-rc-afunix-{}", std::process::id());
    let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let _listener = UnixListener::bind_addr(&addr).unwrap();
    let caveats = crate::caveats::Caveats {
        exec: crate::caveats::Scope::only(["python3".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };
    let cmd = format!(
        r#"python3 -c "import socket; s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.connect('\0{name}'); print('DEPUTY-REACHED'); s.close()""#
    );
    let envelope = super::shell::dispatch_bridled_shell(
        serde_json::json!({"cmd": cmd, "cwd": "."}),
        &caveats,
        None,
    )
    .await
    .expect("dispatch");
    assert_eq!(
        envelope["sandbox_kind"], "landlock",
        "child must be kernel-confined: {envelope}"
    );
    assert!(
        envelope["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("DEPUTY-REACHED"),
        "run_command child reached an AF_UNIX abstract deputy — the ambient-deputy egress \
             residual is REAL on the run_command route (register it; narrow the claim): {envelope}"
    );
}

/// Closure-proof: the run_command route's FD hygiene is CLOEXEC-based (std's
/// default + agent-bridle `set_cloexec`), NOT the explicit `close_range(3,~0)`
/// the `NetGrant::DenyAll` `newt-net-guard` route performs. This pins that a
/// deliberately-NON-CLOEXEC descriptor IS inherited by the run_command child
/// — so the guarantee "a pre-opened network descriptor cannot bypass the
/// socket() filter" holds ONLY because newt opens its real fds via std
/// (CLOEXEC); a non-CLOEXEC fd would cross. Documents the asymmetry honestly.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn run_command_route_fd_hygiene_is_cloexec_based_not_explicit_close() {
    use std::os::fd::AsRawFd;
    if !crate::confined_exec::kernel_fs_fence_available() {
        return;
    }
    // A marker fd with CLOEXEC deliberately CLEARED (the case std never
    // produces, but a raw-libc caller could).
    let marker = std::fs::File::open("/dev/null").expect("open /dev/null");
    let fd = marker.as_raw_fd();
    // SAFETY: fcntl on a valid owned fd; clears the close-on-exec flag.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
    }
    if !std::path::Path::new("/usr/bin/python3").exists() {
        return;
    }
    let caveats = crate::caveats::Caveats {
        exec: crate::caveats::Scope::only(["python3".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };
    let cmd = format!(
        r#"python3 -c "import os; print('FD-INHERITED' if os.path.exists('/proc/self/fd/{fd}') else 'fd-closed')""#
    );
    let envelope = super::shell::dispatch_bridled_shell(
        serde_json::json!({"cmd": cmd, "cwd": "."}),
        &caveats,
        None,
    )
    .await
    .expect("dispatch");
    drop(marker);
    let stdout = envelope["stdout"].as_str().unwrap_or_default().to_string();
    // Ground truth: a non-CLOEXEC fd crosses into the run_command child (the
    // route does not explicitly close fds ≥ 3). If this ever flips to
    // fd-closed, the route gained explicit fd-closing — a strict improvement;
    // update the doc/register. Skipped-outcome tolerated where the confined
    // spawn could not run.
    if envelope["sandbox_kind"] == "landlock" {
        assert!(
            stdout.contains("FD-INHERITED"),
            "expected a non-CLOEXEC fd to be inherited (run_command FD hygiene is CLOEXEC-based, \
                 not explicit close). If now fd-closed, the route added explicit closing — update \
                 docs + register: {envelope}"
        );
    }
}

/// step-7.1a / invariant 9: the host-shell child must NOT inherit the two
/// authority switches. An ambient `NEWT_DISABLE_OCAP=1` / `NEWT_FULL_ACCESS=1`
/// (from a wrapper/pod, or this process's own Yolo lane) would otherwise flow
/// into the child and let it re-assert authority the session did not grant.
/// `env_remove` marks the var absent in the child's env plan (`get_envs` →
/// `(key, None)`); this asserts both are so marked. Fails on any code path
/// that builds the child without the `env_remove` calls.
#[cfg(not(windows))]
#[test]
fn host_shell_command_strips_authority_env() {
    let c = host_shell_command("bash", "true", "/tmp");
    let removed: Vec<String> = c
        .as_std()
        .get_envs()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    // #8: newt's WHOLE control plane is excised — every authority switch and
    // every newt-owned secret, not just the two OCAP flags. A regression that
    // drops any one of them re-opens the authority-survives-one-hop /
    // secret-leak hole.
    for key in CHILD_STRIPPED_AUTHORITY_ENV {
        assert!(
            removed.iter().any(|k| k == key),
            "{key} not stripped from the host-shell child; env plan: {removed:?}"
        );
    }
    // The specific credential-grade + Yolo-deriving switches, named so the
    // intent is legible in the test, not just the loop.
    for critical in [
        "NEWT_UNSAFE_HOST_EXEC",
        "NEWT_AGENT_KEY",
        "NEWT_OPERATOR_KEY",
        "NEWT_TOKEN_PASSPHRASE",
    ] {
        assert!(
            removed.iter().any(|k| k == critical),
            "{critical} must never reach a host-shell child"
        );
    }
}

#[test]
fn envelope_denied_reads_structured_flag_only() {
    assert!(envelope_denied(&serde_json::json!({"denied": true})));
    assert!(!envelope_denied(&serde_json::json!({"denied": false})));
    assert!(!envelope_denied(&serde_json::json!({})));
    // A non-bool `denied` is treated as not-denied, never a panic.
    assert!(!envelope_denied(&serde_json::json!({"denied": "yes"})));
}

#[test]
fn envelope_denial_reason_joins_or_falls_back() {
    let multi = serde_json::json!({
        "denials": [
            {"kind": "exec", "target": "rm", "reason": "exec rm denied"},
            {"kind": "open", "target": "/etc/shadow", "reason": "open denied"}
        ]
    });
    assert_eq!(
        envelope_denial_reason(&multi),
        "exec rm denied; open denied"
    );
    // Missing or empty denials → the generic message, never a panic.
    let generic = "denied: the capability leash refused an operation";
    assert_eq!(envelope_denial_reason(&serde_json::json!({})), generic);
    assert_eq!(
        envelope_denial_reason(&serde_json::json!({"denials": []})),
        generic
    );
    // Entries without a string `reason` are skipped.
    assert_eq!(
        envelope_denial_reason(&serde_json::json!({"denials": [{"kind": "exec"}]})),
        generic
    );
}

#[test]
fn exec_allowlist_name_takes_basename() {
    assert_eq!(exec_allowlist_name("env"), "env");
    assert_eq!(exec_allowlist_name("/usr/bin/env"), "env");
    assert_eq!(exec_allowlist_name("/usr/bin/"), "bin");
    assert_eq!(exec_allowlist_name("C:\\tools\\env.exe"), "env.exe");
}

/// #775 (§2.5): denial recovery uses the BARE command target(s), never the
/// reason sentence. Stuffing the full reason into the former notice's
/// `'{target}'` field produced the
/// field-report garble `capability denied: exec does not permit '<whole
/// reason sentence>'`.
#[test]
fn exec_denial_target_label_is_the_bare_command_not_the_reason() {
    let one = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "export",
            "reason": "exec of \"export\" is not within the granted authority"
        }]
    });
    let label = exec_denial_target_label(&one);
    assert_eq!(label, "export");
    // It is the bare command — NEVER the reason sentence (which, in the
    // `'{target}'` slot, was the nested garble).
    assert!(!label.contains("is not within the granted authority"));
    // Multiple targets join cleanly; an envelope with no target falls back
    // to a generic label so the notice still prints one clean line.
    let multi = serde_json::json!({
        "denials": [
            {"kind": "exec", "target": "export", "reason": "r"},
            {"kind": "exec", "target": "set", "reason": "r"}
        ]
    });
    assert_eq!(exec_denial_target_label(&multi), "export, set");
    assert_eq!(
        exec_denial_target_label(&serde_json::json!({})),
        "a command"
    );
}

#[test]
fn host_of_url_extracts_hosts_conservatively() {
    assert_eq!(host_of_url("https://docs.rs/serde"), Some("docs.rs".into()));
    assert_eq!(host_of_url("http://Docs.RS"), Some("docs.rs".into()));
    assert_eq!(
        host_of_url("https://user:pw@example.com:8443/p?q#f"),
        Some("example.com".into())
    );
    assert_eq!(host_of_url("https://[::1]:8080/x"), Some("::1".into()));
    // Unparseable / non-http inputs skip the pre-check (None) rather
    // than guessing — enforcement stays with the leash either way.
    assert_eq!(host_of_url("not a url"), None);
    assert_eq!(host_of_url("ftp://example.com"), None);
    assert_eq!(host_of_url("https:///path-only"), None);
}

#[test]
fn exec_denial_requests_lifts_only_pure_exec_envelopes() {
    // The promptable case: every entry is an exec denial with a target;
    // the request target is the allowlist basename (the grantable name).
    let exec_only = serde_json::json!({
        "denied": true,
        "denials": [
            {"kind": "exec", "target": "/usr/bin/npm", "reason": "exec npm denied"},
            {"kind": "exec", "target": "node", "reason": "exec node denied"}
        ]
    });
    let reqs = exec_denial_requests(&exec_only).expect("promptable");
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].tool, "run_command");
    assert_eq!(reqs[0].kind, DenialKind::Exec);
    assert_eq!(
        reqs[0].target, "npm",
        "basename, same rule as the config hint"
    );
    assert_eq!(reqs[0].reason, "exec npm denied");
    assert_eq!(reqs[1].target, "node");

    // A non-exec entry anywhere keeps the standard denial: mapping an
    // opaque `open` onto an fs axis would over-grant.
    let mixed = serde_json::json!({
        "denials": [
            {"kind": "exec", "target": "npm", "reason": "r"},
            {"kind": "open", "target": "/etc/shadow", "reason": "r"}
        ]
    });
    assert!(exec_denial_requests(&mixed).is_none());

    // Missing/empty pieces are never promptable.
    assert!(exec_denial_requests(&serde_json::json!({})).is_none());
    assert!(exec_denial_requests(&serde_json::json!({"denials": []})).is_none());
    let empty_target = serde_json::json!({
        "denials": [{"kind": "exec", "target": "", "reason": "r"}]
    });
    assert!(exec_denial_requests(&empty_target).is_none());
    let no_target = serde_json::json!({
        "denials": [{"kind": "exec", "reason": "r"}]
    });
    assert!(exec_denial_requests(&no_target).is_none());

    // #1150: a STRUCTURAL refusal must NOT be promptable — offering a grant
    // for `$(` (which the engine cannot interpret) is a grant->denial
    // contradiction. The exact reason strings are agent-bridle's
    // Refusal::Display output (verified against parse.rs).
    let dynamic = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "command/arithmetic substitution `$(`",
            "reason": "refused by design: command/arithmetic substitution `$(` is a \
                       dynamic construct the confined shell does not interpret (use the \
                       embedder's unbridled/--yolo path for a full shell)"
        }]
    });
    assert!(
        exec_denial_requests(&dynamic).is_none(),
        "structural refusal must not offer a grant menu (#1150)"
    );
    let unsupported = serde_json::json!({
        "denials": [{
            "kind": "exec",
            "target": "heredoc/herestring `<<`",
            "reason": "not yet supported by the confined shell engine: \
                       heredoc/herestring `<<` (tracked on agent-bridle#34)"
        }]
    });
    assert!(exec_denial_requests(&unsupported).is_none());
    // A genuine authority denial (not structural) STAYS promptable.
    let authority = serde_json::json!({
        "denials": [{"kind": "exec", "target": "cargo",
                     "reason": "exec of \"cargo\" is not within the granted authority"}]
    });
    assert!(exec_denial_requests(&authority).is_some());
}

/// #905: a NET denial envelope (agent-bridle #196 shape) lifts to a per-host
/// net PermissionRequest; the target is the CONNECT host verbatim (no
/// basename mangling). Non-net / mixed / empty batches stay flat.
#[test]
fn net_denial_requests_lifts_only_pure_net_envelopes() {
    let net_only = serde_json::json!({
        "denied": true,
        "denials": [
            {"kind": "net", "target": "github.com", "reason": "net does not permit 'github.com'"},
            {"kind": "net", "target": "api.github.com", "reason": "net does not permit 'api.github.com'"}
        ]
    });
    let reqs = net_denial_requests(&net_only).expect("promptable");
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].tool, "run_command");
    assert_eq!(reqs[0].kind, DenialKind::Net);
    assert_eq!(
        reqs[0].target, "github.com",
        "host verbatim, not a basename"
    );
    assert_eq!(reqs[0].reason, "net does not permit 'github.com'");
    assert_eq!(reqs[1].target, "api.github.com");

    // A non-net entry anywhere → not net-promptable (exec lifter handles exec).
    let mixed = serde_json::json!({
        "denials": [
            {"kind": "net", "target": "github.com", "reason": "r"},
            {"kind": "exec", "target": "npm", "reason": "r"}
        ]
    });
    assert!(net_denial_requests(&mixed).is_none());
    // Exec-only is not net-promptable; empty/missing targets never are.
    let exec_only = serde_json::json!({"denials": [{"kind": "exec", "target": "npm"}]});
    assert!(net_denial_requests(&exec_only).is_none());
    assert!(net_denial_requests(&serde_json::json!({"denials": []})).is_none());
    let empty_target = serde_json::json!({"denials": [{"kind": "net", "target": ""}]});
    assert!(net_denial_requests(&empty_target).is_none());
}

/// #905: the human denial NOTICE labels a pure-net refusal `net` (not `exec`),
/// so it never reads "exec does not permit '<host>'". Exec / mixed stay `exec`.
#[test]
fn denials_name_the_exact_recovery_call() {
    // #1160: the model shouldn't infer parameters the harness holds — a
    // denial carries the copy-pasteable request_permissions(...) call.
    let fs = denied_fs_result("fs_write", "/etc/hosts");
    assert!(
        fs.contains(r#"request_permissions(capability="fs_write", target="/etc/hosts""#),
        "{fs}"
    );
    let hint = denial_recovery_hint("exec", "cargo");
    assert!(
        hint.contains(r#"capability="exec""#) && hint.contains(r#"target="cargo""#),
        "{hint}"
    );
}

#[test]
fn denial_axis_label_is_net_only_for_pure_net() {
    let net = serde_json::json!({"denials": [{"kind": "net", "target": "github.com"}]});
    assert_eq!(denial_axis_label(&net), "net");
    let exec = serde_json::json!({"denials": [{"kind": "exec", "target": "rm"}]});
    assert_eq!(denial_axis_label(&exec), "exec");
    let mixed = serde_json::json!({
        "denials": [{"kind": "net", "target": "h"}, {"kind": "exec", "target": "rm"}]
    });
    assert_eq!(denial_axis_label(&mixed), "exec", "mixed defaults to exec");
    assert_eq!(denial_axis_label(&serde_json::json!({})), "exec");
}

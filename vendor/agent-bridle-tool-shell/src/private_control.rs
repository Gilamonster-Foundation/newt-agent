//! Kernel-authenticated private control channels for Brush worker transitions.
//!
//! A fresh challenge binds every frame, but the challenge is not treated as
//! authentication. On Linux the authority frame must carry `SCM_CREDENTIALS`
//! from the worker's real parent. On macOS a pre/post snapshot of
//! `LOCAL_PEERPID` + the full `LOCAL_PEERTOKEN` + the live process image must
//! remain stable across the challenge exchange. Both paths also require the
//! peer image to be this exact executable. Other targets fail closed.

#[cfg(feature = "carried-coreutils")]
use std::ffi::{OsStr, OsString};
#[cfg(feature = "carried-coreutils")]
use std::io::BufReader;
use std::io::{Read, Write};
use std::os::fd::AsFd;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(feature = "carried-coreutils")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;

use agent_bridle_core::{
    decode_trusted_worker_frame_header, decode_trusted_worker_request, encode_trusted_worker_hello,
    trusted_worker_frame_digest, TrustedWorkerRequest, TRUSTED_WORKER_ACK,
    TRUSTED_WORKER_BOOTSTRAP, TRUSTED_WORKER_FRAME_HEADER_LEN,
};
#[cfg(feature = "carried-coreutils")]
use agent_bridle_core::{encode_trusted_worker_frame_header, TRUSTED_WORKER_HELLO_LEN};
use serde::de::DeserializeOwned;

#[cfg(feature = "carried-coreutils")]
const CARRIED_ACK: [u8; 8] = *b"ABCU-A1\0";
#[cfg(feature = "carried-coreutils")]
const CARRIED_IDENTITY_DOMAIN: &[u8] = b"agent-bridle/carried-coreutil/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageIdentity {
    #[cfg(target_os = "linux")]
    dev: u64,
    #[cfg(target_os = "linux")]
    ino: u64,
    #[cfg(target_os = "macos")]
    path: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParentSnapshot {
    ppid: i32,
    peer_pid: i32,
    image: ImageIdentityState,
    #[cfg(target_os = "macos")]
    token: nix::sys::socket::audit_token_t,
}

#[derive(Debug, Clone, Copy)]
struct FrameCredentials {
    #[cfg(target_os = "linux")]
    pid: i32,
    #[cfg(target_os = "linux")]
    uid: u32,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum ImageIdentityState {
    Known(ImageIdentity),
    Unknown,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn same_image(left: &ImageIdentityState, right: &ImageIdentityState) -> bool {
    match (left, right) {
        (ImageIdentityState::Known(a), ImageIdentityState::Known(b)) => a == b,
        _ => true,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn inspect_image_with_fallback(pid: i32) -> Result<ImageIdentityState, String> {
    match process_image(pid) {
        Ok(image) => Ok(ImageIdentityState::Known(image)),
        Err(error) if error.raw_os_error() == Some(13) => Ok(ImageIdentityState::Unknown),
        Err(error) => Err(format!("inspect process {pid} image: {error}")),
    }
}

/// Receive and authenticate the one core-framed Brush worker request on fd 0.
pub(crate) fn receive_worker_request_with_endpoint<P: DeserializeOwned>(
    needs_endpoint: impl FnOnce(&P) -> bool,
) -> Result<(TrustedWorkerRequest<P>, Option<UnixStream>), String> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        return Err(
            "authenticated private worker control is unavailable on this platform".to_string(),
        );
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let stdin = std::io::stdin();
        let owned = stdin
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| format!("duplicate worker control socket: {error}"))?;
        let mut stream = UnixStream::from(owned);

        let mut challenge = [0_u8; 32];
        getrandom::getrandom(&mut challenge)
            .map_err(|error| format!("create worker control challenge: {error}"))?;

        // This diagnostic copy is intentionally emitted before authentication.
        // It lets the regression exercise an interactive read/response attacker;
        // no authority is accepted until the kernel channel checks below pass.
        eprintln!(
            "agent-bridle-private-control-v1 challenge {}",
            hex(&challenge)
        );
        prepare_receiver(&stream)?;
        receive_bootstrap(&mut stream, "worker")?;
        let pre = parent_snapshot(&stream);
        stream
            .write_all(&encode_trusted_worker_hello(std::process::id(), challenge))
            .and_then(|()| stream.flush())
            .map_err(|error| format!("write worker control hello: {error}"))?;

        let mut header = [0_u8; TRUSTED_WORKER_FRAME_HEADER_LEN];
        let credentials = recv_exact_frame(&stream, &mut header)?;
        let post = parent_snapshot(&stream);
        verify_parent(pre, post, credentials)?;

        let (body_len, echoed_challenge, expected_digest) =
            decode_trusted_worker_frame_header(&header)?;
        if echoed_challenge != challenge {
            return Err("worker control challenge mismatch".to_string());
        }
        let mut body = vec![0_u8; body_len];
        stream
            .read_exact(&mut body)
            .map_err(|error| format!("read worker authority body: {error}"))?;
        if trusted_worker_frame_digest(&challenge, &body) != expected_digest {
            return Err("worker authority frame digest mismatch".to_string());
        }
        let request: TrustedWorkerRequest<P> = decode_trusted_worker_request(&body)?;
        let endpoint = if needs_endpoint(request.payload()) {
            Some(
                agent_bridle_core::receive_control_endpoint(&stream).map_err(|error| {
                    format!("receive delegated worker control endpoint: {error}")
                })?,
            )
        } else {
            None
        };
        stream
            .write_all(&TRUSTED_WORKER_ACK)
            .and_then(|()| stream.flush())
            .map_err(|error| format!("acknowledge worker authority frame: {error}"))?;
        retire_worker_stdin()?;
        Ok((request, endpoint))
    }
}

/// Encode the exact carried utility identity (`name` + raw argv bytes).
#[cfg(feature = "carried-coreutils")]
pub(crate) fn carried_identity(name: &OsStr, args: &[OsString]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(CARRIED_IDENTITY_DOMAIN);
    push_os(&mut bytes, name);
    bytes.extend_from_slice(&(args.len() as u64).to_le_bytes());
    for arg in args {
        push_os(&mut bytes, arg);
    }
    bytes
}

/// Prepare a parent endpoint before spawning a carried utility.
#[cfg(feature = "carried-coreutils")]
pub(crate) fn prepare_carried_parent(stream: &UnixStream) -> Result<(), String> {
    prepare_receiver(stream)
}

/// Parent half of carried-utility authentication.
///
/// Returns a buffered reader positioned immediately after the binary ACK; all
/// subsequent bytes are the utility's ordinary stderr.
#[cfg(feature = "carried-coreutils")]
pub(crate) fn authorize_carried_child(
    mut stream: UnixStream,
    child_pid: u32,
    expected_identity: &[u8],
) -> Result<BufReader<UnixStream>, String> {
    stream
        .write_all(&TRUSTED_WORKER_BOOTSTRAP)
        .and_then(|()| stream.flush())
        .map_err(|error| format!("write carried control bootstrap: {error}"))?;

    // Linux authenticates the actual sender of each frame, so it can also
    // capture an image snapshot immediately before the hello. On macOS,
    // LOCAL_PEERPID/LOCAL_PEERTOKEN identify the most recent peer writer; the
    // child must write its hello before that snapshot can name the re-execed
    // child rather than the socketpair's creating parent.
    #[cfg(target_os = "linux")]
    let pre = child_snapshot(&stream, child_pid);
    let mut hello = [0_u8; TRUSTED_WORKER_HELLO_LEN];
    let hello_credentials = recv_exact_frame(&stream, &mut hello)?;
    let after_hello = child_snapshot(&stream, child_pid);
    #[cfg(target_os = "linux")]
    verify_child(pre, after_hello.clone(), hello_credentials, child_pid)?;
    // macOS: the hello-time kernel identity is MANDATORY. The child blocks on the
    // challenge read after writing its hello, so it is alive here in the normal
    // handshake; a `None` (peer already gone) is a fail-closed refusal, never an
    // authentication — we will not proceed without the intended kernel-identity
    // proof. (A false refusal is preferable to authenticating without it.)
    #[cfg(target_os = "macos")]
    let hello_identity = {
        let _ = hello_credentials;
        let snapshot = after_hello?.ok_or_else(|| {
            "carried-child peer disconnected before its authenticated hello — refusing \
             (fail-closed): no kernel-attested identity was captured"
                .to_string()
        })?;
        verify_child_macos(&snapshot, None, child_pid)?;
        snapshot
    };

    let (reported_pid, challenge) = agent_bridle_core::decode_trusted_worker_hello(&hello)?;
    if reported_pid != child_pid {
        return Err(format!(
            "carried child hello PID mismatch: spawned {child_pid}, reported {reported_pid}"
        ));
    }
    let digest = trusted_worker_frame_digest(&challenge, expected_identity);
    let header = encode_trusted_worker_frame_header(challenge, digest, expected_identity.len())?;
    stream
        .write_all(&header)
        .and_then(|()| stream.write_all(expected_identity))
        .and_then(|()| stream.flush())
        .map_err(|error| format!("write carried authorization: {error}"))?;
    let mut ack = [0_u8; CARRIED_ACK.len()];
    let ack_credentials = recv_exact_frame(&stream, &mut ack)?;
    let after_ack = child_snapshot(&stream, child_pid);
    #[cfg(target_os = "linux")]
    verify_child_after_ack(after_hello, after_ack, ack_credentials, child_pid)?;
    // macOS: the ACK completed the handshake, so the (already kernel-identified)
    // child may now legitimately exit — a pipeline stage whose downstream `head`
    // closed the pipe. A gone peer at re-snapshot time — ENOTCONN (`Ok(None)`)
    // OR a peer pid the process table can no longer image (exited/zombie:
    // `proc_pidpath` ESRCH — an `Err` here) — is therefore accepted: a gone
    // peer cannot be a live impersonator, and the mandatory kernel identity was
    // captured at hello. A still-CONNECTED peer must match the hello identity.
    // (This re-snapshot is defense-in-depth; the authority boundary is the
    // private socketpair + the completed hello/challenge/ACK handshake.)
    #[cfg(target_os = "macos")]
    {
        let _ = ack_credentials;
        let reverify = tolerate_gone_reverify(after_ack);
        verify_child_macos(&hello_identity, reverify.as_ref(), child_pid)?;
    }
    if ack != CARRIED_ACK {
        return Err("carried child returned an invalid authentication ACK".to_string());
    }
    // Keep the child alive through the final image check. EOF alone cannot
    // release dispatch: a refused parent also closes its socket on error.
    stream
        .write_all(&CARRIED_ACK)
        .and_then(|()| stream.flush())
        .map_err(|error| format!("confirm carried authentication: {error}"))?;
    // A confirmed fast utility may already have exited; the helper tolerates
    // only that on the courtesy half-close.
    close_authorization_direction(&stream)?;
    Ok(BufReader::new(stream))
}

/// Map the post-ACK re-snapshot result for re-verification: an `Err` (the peer
/// pid is exited/zombie and can no longer be imaged — `proc_pidpath` ESRCH) is
/// the same benign terminal state as the `Ok(None)` disconnect, so both become
/// `None`. Used ONLY on the post-ACK site; hello-time snapshot errors stay hard.
#[cfg(target_os = "macos")]
#[cfg(feature = "carried-coreutils")]
fn tolerate_gone_reverify(
    after_ack: Result<Option<ParentSnapshot>, String>,
) -> Option<ParentSnapshot> {
    after_ack.unwrap_or(None)
}

/// Half-close the parent→child authorization direction after a completed
/// handshake. A [`std::io::ErrorKind::NotConnected`] failure is accepted: the
/// authenticated child may already have exited and closed its end (a pipeline
/// stage whose work is done), in which case the write direction is already
/// down and there is nothing left to close.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(feature = "carried-coreutils")]
fn close_authorization_direction(stream: &UnixStream) -> Result<(), String> {
    match stream.shutdown(std::net::Shutdown::Write) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotConnected => Ok(()),
        Err(error) => Err(format!("close carried authorization direction: {error}")),
    }
}

/// Child half of carried-utility authentication on its duplex fd 2.
#[cfg(feature = "carried-coreutils")]
pub(crate) fn authenticate_carried_dispatch(name: &OsStr, args: &[OsString]) -> Result<(), String> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (name, args);
        return Err("authenticated carried dispatch is unavailable on this platform".to_string());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let stderr = std::io::stderr();
        let owned = stderr
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| format!("duplicate carried control socket: {error}"))?;
        let mut stream = UnixStream::from(owned);
        prepare_receiver(&stream)?;
        receive_bootstrap(&mut stream, "carried")?;
        let mut challenge = [0_u8; 32];
        getrandom::getrandom(&mut challenge)
            .map_err(|error| format!("create carried control challenge: {error}"))?;
        let pre = parent_snapshot(&stream)?;
        stream
            .write_all(&encode_trusted_worker_hello(std::process::id(), challenge))
            .and_then(|()| stream.flush())
            .map_err(|error| format!("write carried control hello: {error}"))?;

        let mut header = [0_u8; TRUSTED_WORKER_FRAME_HEADER_LEN];
        let credentials = recv_exact_frame(&stream, &mut header)?;
        let post = parent_snapshot(&stream)?;
        verify_parent(Ok(pre), Ok(post), credentials)?;

        let (body_len, echoed_challenge, expected_digest) =
            decode_trusted_worker_frame_header(&header)?;
        if echoed_challenge != challenge {
            return Err("carried control challenge mismatch".to_string());
        }
        let mut body = vec![0_u8; body_len];
        stream
            .read_exact(&mut body)
            .map_err(|error| format!("read carried identity body: {error}"))?;
        let actual_identity = carried_identity(name, args);
        if body != actual_identity
            || trusted_worker_frame_digest(&challenge, &body) != expected_digest
        {
            return Err("carried authorization does not match the exact utility argv".to_string());
        }

        stream
            .write_all(&CARRIED_ACK)
            .and_then(|()| stream.flush())
            .map_err(|error| format!("write carried authentication ACK: {error}"))?;
        let mut confirmation = [0_u8; CARRIED_ACK.len()];
        stream
            .read_exact(&mut confirmation)
            .map_err(|error| format!("read carried authentication confirmation: {error}"))?;
        if confirmation != CARRIED_ACK {
            return Err("carried parent returned an invalid authentication confirmation".into());
        }
        set_cloexec(&stderr)?;
        Ok(())
    }
}

#[cfg(feature = "carried-coreutils")]
fn push_os(bytes: &mut Vec<u8>, value: &OsStr) {
    let value = value.as_bytes();
    bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
    bytes.extend_from_slice(value);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Receive the fixed non-authority prelude with ordinary stream I/O.
///
/// The parent may queue this bootstrap immediately after spawning, before the
/// child enables Linux `SO_PASSCRED`. Requiring credentials on that already
/// queued prelude creates a startup race without authenticating any authority.
/// Every authority-bearing frame follows the child's hello and is still read
/// through `recv_exact_frame`, where credentials are mandatory.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn receive_bootstrap(stream: &mut UnixStream, channel: &str) -> Result<(), String> {
    let mut bootstrap = [0_u8; TRUSTED_WORKER_BOOTSTRAP.len()];
    stream
        .read_exact(&mut bootstrap)
        .map_err(|error| format!("read {channel} control bootstrap: {error}"))?;
    if bootstrap != TRUSTED_WORKER_BOOTSTRAP {
        return Err(format!("{channel} control bootstrap mismatch"));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn prepare_receiver(stream: &UnixStream) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    nix::sys::socket::setsockopt(stream, nix::sys::socket::sockopt::PassCred, &true)
        .map_err(|error| format!("enable control-frame credentials: {error}"))?;
    #[cfg(target_os = "macos")]
    let _ = stream;
    Ok(())
}

/// Enable per-message credentials before the peer writes its handshake.
pub(crate) fn prepare_broker_receiver(stream: &UnixStream) -> agent_bridle_core::ToolResult<()> {
    prepare_receiver(stream).map_err(agent_bridle_core::ToolError::denied)
}

/// Read the handshake then inspect the actual writer, not the socketpair creator.
pub(crate) fn read_broker_peer(
    stream: &UnixStream,
    bytes: &mut [u8],
) -> agent_bridle_core::ToolResult<u32> {
    use agent_bridle_core::ToolError;
    let credentials = recv_exact_frame(stream, bytes).map_err(ToolError::denied)?;
    #[cfg(target_os = "linux")]
    let pid = {
        if credentials.uid != nix::unistd::getuid().as_raw() {
            return Err(ToolError::denied(
                "broker writer has a different user identity",
            ));
        }
        credentials.pid
    };
    #[cfg(target_os = "macos")]
    let pid = {
        let _ = credentials;
        let pid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerPid)
            .map_err(|error| ToolError::denied(error.to_string()))?;
        let token = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerToken)
            .map_err(|error| ToolError::denied(error.to_string()))?;
        // audit_token: auid,euid,egid,ruid,rgid,pid,asid,pidversion.
        if token.val[1] != nix::unistd::geteuid().as_raw() || token.val[5] != pid as u32 {
            return Err(ToolError::denied("broker writer audit identity mismatch"));
        }
        pid
    };
    u32::try_from(pid).map_err(|_| ToolError::denied("invalid broker writer PID"))
}

#[cfg(target_os = "linux")]
fn recv_exact_frame(stream: &UnixStream, bytes: &mut [u8]) -> Result<FrameCredentials, String> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags, UnixCredentials};
    use std::io::IoSliceMut;

    let expected_len = bytes.len();
    let (received_len, credentials) = {
        let mut iov = [IoSliceMut::new(bytes)];
        let mut cmsgspace = nix::cmsg_space!(UnixCredentials);
        let message = recvmsg::<()>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut cmsgspace),
            MsgFlags::MSG_WAITALL,
        )
        .map_err(|error| format!("receive authenticated control frame: {error}"))?;
        let mut credentials = None;
        for control in message
            .cmsgs()
            .map_err(|error| format!("decode control-frame credentials: {error}"))?
        {
            if let ControlMessageOwned::ScmCredentials(seen) = control {
                if credentials.replace(seen).is_some() {
                    return Err("control frame carried duplicate credentials".to_string());
                }
            }
        }
        (message.bytes, credentials)
    };
    if received_len != expected_len {
        return Err(format!(
            "authenticated control frame was truncated ({} of {} bytes)",
            received_len, expected_len
        ));
    }
    let credentials =
        credentials.ok_or_else(|| "control frame carried no SCM_CREDENTIALS".to_string())?;
    Ok(FrameCredentials {
        pid: credentials.pid(),
        uid: credentials.uid(),
    })
}

#[cfg(target_os = "macos")]
fn recv_exact_frame(stream: &UnixStream, bytes: &mut [u8]) -> Result<FrameCredentials, String> {
    (&*stream)
        .read_exact(bytes)
        .map_err(|error| format!("receive authenticated control frame: {error}"))?;
    Ok(FrameCredentials {})
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn parent_snapshot(stream: &UnixStream) -> Result<ParentSnapshot, String> {
    let ppid = nix::unistd::getppid().as_raw();
    #[cfg(target_os = "linux")]
    let peer_pid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
        .map_err(|error| format!("read worker-control peer credentials: {error}"))?
        .pid();
    #[cfg(target_os = "macos")]
    let peer_pid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerPid)
        .map_err(|error| format!("read worker-control peer PID: {error}"))?;
    let image = inspect_image_with_fallback(peer_pid)?;
    Ok(ParentSnapshot {
        ppid,
        peer_pid,
        image,
        #[cfg(target_os = "macos")]
        token: nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerToken)
            .map_err(|error| format!("read worker-control peer audit token: {error}"))?,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verify_parent(
    pre: Result<ParentSnapshot, String>,
    post: Result<ParentSnapshot, String>,
    frame: FrameCredentials,
) -> Result<(), String> {
    let pre = pre?;
    let post = post?;
    if pre.ppid != post.ppid
        || pre.peer_pid != post.peer_pid
        || !same_image(&pre.image, &post.image)
    {
        return Err("worker-control peer identity changed during authentication".to_string());
    }
    if pre.peer_pid != pre.ppid {
        return Err(format!(
            "worker-control peer PID {} is not parent PID {}",
            pre.peer_pid, pre.ppid
        ));
    }
    if let ImageIdentityState::Known(peer_image) = pre.image {
        if current_image()? != peer_image {
            return Err("worker-control parent image is not this executable".to_string());
        }
    }
    #[cfg(target_os = "linux")]
    {
        if frame.pid != pre.peer_pid || frame.uid != nix::unistd::getuid().as_raw() {
            return Err("worker authority frame sender credentials do not match its parent".into());
        }
    }
    #[cfg(target_os = "macos")]
    let _ = frame;
    Ok(())
}

#[cfg(target_os = "linux")]
#[cfg(feature = "carried-coreutils")]
fn child_snapshot(_stream: &UnixStream, child_pid: u32) -> Result<ParentSnapshot, String> {
    let child_pid = i32::try_from(child_pid).map_err(|_| "child PID does not fit i32")?;
    let image = inspect_image_with_fallback(child_pid)?;
    Ok(ParentSnapshot {
        ppid: std::process::id() as i32,
        peer_pid: child_pid,
        image,
    })
}

/// Snapshot the carried child's kernel-attested identity on macOS.
///
/// Returns `Ok(None)` when the peer has already **disconnected** (`ENOTCONN`):
/// `getsockopt(LOCAL_PEERPID)` reports the most-recent peer *writer* and, once
/// every fd to the socketpair's child end is closed, fails with `ENOTCONN`
/// (verified on-device: pre-write it names the socketpair creator; after the
/// child's hello it names the child; after the child exits it is `ENOTCONN`).
///
/// `None` is a *terminal* state the caller may accept ONLY on the post-ACK
/// re-snapshot — where an authenticated carried utility in a pipeline
/// legitimately exits the instant a downstream stage (`head -1`) closes the pipe.
/// The **hello-time** identity is mandatory (see [`authorize_carried_child`]): a
/// `None` there is a fail-closed refusal, never a pass. Any errno other than
/// `ENOTCONN` remains a hard failure.
#[cfg(target_os = "macos")]
#[cfg(feature = "carried-coreutils")]
fn child_snapshot(stream: &UnixStream, _child_pid: u32) -> Result<Option<ParentSnapshot>, String> {
    let peer_pid =
        match nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerPid) {
            Ok(pid) => pid,
            Err(nix::errno::Errno::ENOTCONN) => return Ok(None),
            Err(error) => return Err(format!("read carried-child peer PID: {error}")),
        };
    let image = inspect_image_with_fallback(peer_pid)?;
    let token =
        match nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerToken) {
            Ok(token) => token,
            Err(nix::errno::Errno::ENOTCONN) => return Ok(None),
            Err(error) => return Err(format!("read carried-child peer audit token: {error}")),
        };
    Ok(Some(ParentSnapshot {
        ppid: std::process::id() as i32,
        peer_pid,
        image,
        token,
    }))
}

/// macOS carried-child verification.
///
/// The `hello` snapshot is the **kernel-attested identity captured immediately
/// after the child's authenticated hello, and it is mandatory** — a missing
/// hello-time identity is never treated as success. `reverify` is the OPTIONAL
/// post-ACK re-snapshot: `None` means the (already authenticated) child exited
/// after completing the handshake, which is benign (a disconnected peer cannot be
/// a live impersonator — no process holds the child end to substitute an
/// identity). A *connected* `reverify` must still match the hello-time identity.
///
/// The authority boundary is possession of the private carried socketpair PLUS
/// the completed hello/challenge/ACK handshake; the kernel PID/image check here
/// is the defense-in-depth cross-check that the *writer* was the spawned child
/// (not a grandchild that inherited the fd, and not a user-space-forged reported
/// PID — the kernel value is authoritative).
#[cfg(target_os = "macos")]
#[cfg(feature = "carried-coreutils")]
fn verify_child_macos(
    hello: &ParentSnapshot,
    reverify: Option<&ParentSnapshot>,
    child_pid: u32,
) -> Result<(), String> {
    let child_pid = i32::try_from(child_pid).map_err(|_| "child PID does not fit i32")?;
    if hello.peer_pid != child_pid {
        return Err(format!(
            "carried-child kernel peer PID {} is not the spawned child {child_pid}",
            hello.peer_pid
        ));
    }
    if let ImageIdentityState::Known(peer_image) = &hello.image {
        if &current_image()? != peer_image {
            return Err("carried-child image is not this executable".to_string());
        }
    }
    if let Some(reverify) = reverify {
        if hello.peer_pid != reverify.peer_pid
            || hello.ppid != reverify.ppid
            || !same_image(&hello.image, &reverify.image)
        {
            return Err("carried-child identity changed during authentication".to_string());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[cfg(feature = "carried-coreutils")]
fn verify_child(
    pre: Result<ParentSnapshot, String>,
    post: Result<ParentSnapshot, String>,
    frame: FrameCredentials,
    child_pid: u32,
) -> Result<(), String> {
    let pre = pre?;
    let post = post?;
    let child_pid = i32::try_from(child_pid).map_err(|_| "child PID does not fit i32")?;
    if pre.peer_pid != post.peer_pid
        || pre.peer_pid != child_pid
        || pre.ppid != post.ppid
        || !same_image(&pre.image, &post.image)
    {
        return Err("carried-child identity changed during authentication".to_string());
    }
    if let ImageIdentityState::Known(peer_image) = pre.image {
        if current_image()? != peer_image {
            return Err("carried-child image is not this executable".to_string());
        }
    }
    #[cfg(target_os = "linux")]
    {
        if frame.pid != child_pid || frame.uid != nix::unistd::getuid().as_raw() {
            return Err("carried-child frame credentials do not match the spawned child".into());
        }
    }
    #[cfg(target_os = "macos")]
    let _ = frame;
    Ok(())
}

/// The post-ACK variant of [`verify_child`]. The ACK frame's kernel
/// `SCM_CREDENTIALS` (captured at receive time) remain the load-bearing check
/// and are always enforced. The re-snapshot, however, is defense-in-depth
/// against a live impersonator — and a child whose `/proc/<pid>/exe` can no
/// longer be inspected (exited or zombie: ENOENT) is not a live anything: a
/// fast pipeline stage (`head`) legitimately exits the instant its downstream
/// closes, often before this re-snapshot runs. So a failed post-ACK snapshot is
/// accepted; a SUCCESSFUL snapshot must still match the hello-time identity.
#[cfg(target_os = "linux")]
#[cfg(feature = "carried-coreutils")]
fn verify_child_after_ack(
    after_hello: Result<ParentSnapshot, String>,
    after_ack: Result<ParentSnapshot, String>,
    frame: FrameCredentials,
    child_pid: u32,
) -> Result<(), String> {
    let hello = after_hello?;
    let child_pid_i32 = i32::try_from(child_pid).map_err(|_| "child PID does not fit i32")?;
    if frame.pid != child_pid_i32 || frame.uid != nix::unistd::getuid().as_raw() {
        return Err("carried-child frame credentials do not match the spawned child".into());
    }
    // An `Err` snapshot is benign here: the kernel-credentialed ACK completed
    // the handshake and the child has already exited (see the doc above).
    if let Ok(post) = after_ack {
        if hello.peer_pid != post.peer_pid
            || hello.ppid != post.ppid
            || !same_image(&hello.image, &post.image)
        {
            return Err("carried-child identity changed during authentication".to_string());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn process_image(pid: i32) -> Result<ImageIdentity, std::io::Error> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(format!("/proc/{pid}/exe"))?;
    Ok(ImageIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

#[cfg(target_os = "macos")]
fn process_image(pid: i32) -> Result<ImageIdentity, std::io::Error> {
    let path = libproc::libproc::proc_pid::pidpath(pid).map_err(std::io::Error::other)?;
    let path = std::fs::canonicalize(path).map_err(std::io::Error::other)?;
    Ok(ImageIdentity { path })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn current_image() -> Result<ImageIdentity, String> {
    let pid = i32::try_from(std::process::id()).map_err(|_| "current PID does not fit i32")?;
    process_image(pid).map_err(|error| format!("inspect process {pid} image: {error}"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn retire_worker_stdin() -> Result<(), String> {
    let null = std::fs::File::open("/dev/null")
        .map_err(|error| format!("open null stdin after worker authentication: {error}"))?;
    nix::unistd::dup2_stdin(&null)
        .map_err(|error| format!("retire worker control descriptor: {error}"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(feature = "carried-coreutils")]
fn set_cloexec(fd: &impl AsFd) -> Result<(), String> {
    nix::fcntl::fcntl(
        fd,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .map(drop)
    .map_err(|error| format!("mark private control descriptor close-on-exec: {error}"))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    #[cfg(any(target_os = "linux", feature = "carried-coreutils"))]
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn queued_bootstrap_precedes_credentialed_authority_frame_without_racing() {
        let (mut sender, mut receiver) = UnixStream::pair().expect("private socketpair");

        // Model the fast parent: the non-authority bootstrap is already queued
        // before the child has enabled credential reception.
        sender
            .write_all(&TRUSTED_WORKER_BOOTSTRAP)
            .expect("queue bootstrap");
        prepare_receiver(&receiver).expect("prepare authenticated receiver");
        receive_bootstrap(&mut receiver, "test").expect("receive queued bootstrap");

        // Authority is sent only after receiver preparation and must still
        // carry the sender's kernel credentials.
        let authority = *b"AUTH-V1\0";
        sender
            .write_all(&authority)
            .expect("write authority-shaped frame");
        let mut received = [0_u8; 8];
        let credentials =
            recv_exact_frame(&receiver, &mut received).expect("receive credentialed frame");
        assert_eq!(received, authority);
        assert_eq!(credentials.pid, std::process::id() as i32);
        assert_eq!(credentials.uid, nix::unistd::getuid().as_raw());
    }

    #[cfg(feature = "carried-coreutils")]
    mod carried_confirmation {
        use super::*;
        use std::os::fd::OwnedFd;
        use std::process::{Child, Command, ExitStatus, Stdio};
        use std::time::{Duration, Instant};

        const MARKER_ENV: &str = "AB_TEST_CARRIED_CONFIRMATION_MARKER";
        const BOUND: Duration = Duration::from_secs(5);

        struct ReapChild(Child);

        impl Drop for ReapChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        impl ReapChild {
            fn wait(&mut self) -> ExitStatus {
                let deadline = Instant::now() + BOUND;
                loop {
                    if let Some(status) = self.0.try_wait().expect("poll owned child") {
                        return status;
                    }
                    assert!(Instant::now() < deadline, "carried child exceeded deadline");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }

        /// Same-image real child grounds the carried handshake's requirement
        /// that dispatch follow the parent's final identity verification.
        #[test]
        #[ignore = "private child fixture, launched by the confirmation tests"]
        fn child() {
            let marker = std::env::var_os(MARKER_ENV).expect("private marker path");
            let code = match authenticate_carried_dispatch(OsStr::new("echo"), &[]) {
                Ok(()) => {
                    std::fs::write(marker, b"DISPATCHED").expect("write dispatch marker");
                    0
                }
                Err(_) => 64,
            };
            std::process::exit(code);
        }

        fn check_confirmation(label: &str, confirmation: Option<&[u8]>, succeeds: bool) {
            let dir = std::env::temp_dir().join(format!(
                "ab-carried-confirmation-{}-{label}",
                std::process::id()
            ));
            std::fs::create_dir(&dir).expect("create unique fixture directory");
            let marker = dir.join("dispatched");
            let (mut parent, child) = UnixStream::pair().expect("private socketpair");
            parent.set_read_timeout(Some(BOUND)).unwrap();
            parent.set_write_timeout(Some(BOUND)).unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--ignored",
                    "--exact",
                    "private_control::tests::carried_confirmation::child",
                    "--nocapture",
                ])
                .env_clear()
                .env(MARKER_ENV, &marker)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(OwnedFd::from(child)));
            let mut child = ReapChild(command.spawn().expect("spawn same-image child"));
            drop(command);

            parent.write_all(&TRUSTED_WORKER_BOOTSTRAP).unwrap();
            let mut hello = [0; TRUSTED_WORKER_HELLO_LEN];
            parent.read_exact(&mut hello).expect("read child hello");
            let (pid, challenge) = agent_bridle_core::decode_trusted_worker_hello(&hello).unwrap();
            assert_eq!(pid, child.0.id());
            let identity = carried_identity(OsStr::new("echo"), &[]);
            let header = encode_trusted_worker_frame_header(
                challenge,
                trusted_worker_frame_digest(&challenge, &identity),
                identity.len(),
            )
            .unwrap();
            parent.write_all(&header).unwrap();
            parent.write_all(&identity).unwrap();
            let mut ack = [0; CARRIED_ACK.len()];
            parent.read_exact(&mut ack).expect("read authenticated ACK");
            assert_eq!(ack, CARRIED_ACK);
            if let Some(confirmation) = confirmation {
                parent
                    .write_all(confirmation)
                    .expect("send final confirmation");
            } else {
                parent.shutdown(std::net::Shutdown::Write).unwrap();
            }

            let status = child.wait();
            assert_eq!(status.code(), Some(if succeeds { 0 } else { 64 }));
            assert_eq!(marker.exists(), succeeds, "dispatch requires confirmation");
            if succeeds {
                assert_eq!(std::fs::read(&marker).unwrap(), b"DISPATCHED");
            }
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn eof_after_ack_does_not_release_carried_dispatch() {
            check_confirmation("eof", None, false);
        }

        #[test]
        fn invalid_confirmation_does_not_release_carried_dispatch() {
            check_confirmation("invalid", Some(b"INVALID!"), false);
        }

        #[test]
        fn verified_confirmation_releases_carried_dispatch() {
            check_confirmation("valid", Some(&CARRIED_ACK), true);
        }
    }
}

#[cfg(all(test, target_os = "macos", feature = "carried-coreutils"))]
mod macos_auth_tests {
    use super::*;

    /// A real, connected hello-time snapshot for this process (both socketpair
    /// ends live here, so the kernel peer PID is our own pid).
    fn live_snapshot() -> (UnixStream, UnixStream, ParentSnapshot) {
        let (parent, child) = UnixStream::pair().expect("socketpair");
        // The child end must WRITE first so LOCAL_PEERPID names it.
        (&child).write_all(b"x").expect("child write");
        let mut buf = [0_u8; 1];
        (&parent).read_exact(&mut buf).expect("parent read");
        let snap = child_snapshot(&parent, std::process::id())
            .expect("snapshot")
            .expect("connected peer must snapshot Some");
        (parent, child, snap)
    }

    /// Happy path: a connected hello identity that equals the spawned child, with a
    /// matching connected re-verify, authenticates.
    #[test]
    fn direct_child_authenticates() {
        let (_p, _c, hello) = live_snapshot();
        verify_child_macos(&hello, Some(&hello), std::process::id()).expect("direct child ok");
    }

    /// A kernel peer PID that is not the spawned child is rejected — the kernel
    /// value is authoritative, so a user-space-forged reported PID cannot stand in
    /// for it (verify_child_macos never consults the self-declared hello bytes).
    #[test]
    fn wrong_kernel_pid_is_rejected_even_against_a_forged_report() {
        let (_p, _c, hello) = live_snapshot();
        let forged_report = std::process::id() + 4242; // pretend the hello claimed this
        assert!(
            verify_child_macos(&hello, None, forged_report).is_err(),
            "kernel peer PID must be checked against the spawned child, not a report"
        );
    }

    /// An identity substitution while the peer is still CONNECTED (a different live
    /// writer mid-handshake) is rejected.
    #[test]
    fn connected_identity_substitution_is_rejected() {
        let (_p, _c, hello) = live_snapshot();
        let mut substituted = hello.clone();
        substituted.peer_pid = hello.peer_pid + 9999;
        assert!(
            verify_child_macos(&hello, Some(&substituted), std::process::id()).is_err(),
            "a connected peer that changed identity must be rejected"
        );
    }

    /// A mismatched peer image (where inspectable) is rejected.
    #[test]
    fn wrong_image_is_rejected() {
        let (_p, _c, hello) = live_snapshot();
        let mut wrong = hello.clone();
        wrong.image = ImageIdentityState::Known(ImageIdentity {
            path: std::path::PathBuf::from("/definitely/not/this/executable"),
        });
        assert!(
            verify_child_macos(&wrong, None, std::process::id()).is_err(),
            "a peer whose image is not this executable must be rejected"
        );
    }

    /// Disconnect AFTER a completed handshake: the post-ACK re-snapshot is `None`
    /// (ENOTCONN — the authenticated child exited), which is accepted.
    #[test]
    fn disconnect_after_ack_succeeds() {
        let (_p, _c, hello) = live_snapshot();
        verify_child_macos(&hello, None, std::process::id())
            .expect("a peer that exits after the handshake is tolerated on re-verify");
    }

    /// Disconnect BEFORE the authenticated hello: the hello-time snapshot is `None`
    /// (ENOTCONN), which the caller turns into a fail-closed refusal. No kernel
    /// identity is captured, so authentication must NOT proceed.
    #[test]
    fn disconnect_before_hello_snapshots_none_and_refuses() {
        let (parent, child) = UnixStream::pair().expect("socketpair");
        (&child).write_all(b"x").expect("child write");
        let mut buf = [0_u8; 1];
        (&parent).read_exact(&mut buf).expect("parent read");
        drop(child); // peer gone before the hello-time snapshot
        let snap = child_snapshot(&parent, std::process::id()).expect("snapshot call");
        assert!(
            snap.is_none(),
            "a disconnected peer must snapshot None (ENOTCONN)"
        );
        // authorize_carried_child converts a None hello into a hard refusal:
        let refused = snap.ok_or_else(|| "no kernel-attested identity".to_string());
        assert!(refused.is_err(), "a None hello identity must fail closed");
    }

    /// #331: closing the authorization direction after the authenticated child
    /// already exited (its end closed → `ENOTCONN`) is a benign no-op, not an
    /// authentication failure. Any other shutdown error still refuses.
    #[test]
    fn close_authorization_direction_tolerates_a_gone_peer() {
        let (parent, child) = UnixStream::pair().expect("socketpair");
        // Live peer: the half-close succeeds normally.
        close_authorization_direction(&parent).expect("live-peer half-close");
        drop(child);
        // Gone peer: macOS reports NotConnected on shutdown of a socket whose
        // peer closed; the wrapper accepts it (the direction is already down).
        close_authorization_direction(&parent)
            .expect("a gone peer must not turn the courtesy half-close into a failure");
    }

    /// #331: a post-ACK re-snapshot whose image inspection FAILED (the pid is
    /// exited/zombie — `proc_pidpath` ESRCH) is treated like the disconnected
    /// case: tolerated on re-verify, because the mandatory identity was captured
    /// at hello and a gone process cannot be a live impersonator.
    #[test]
    fn errored_post_ack_resnapshot_is_tolerated_like_a_disconnect() {
        let (_p, _c, hello) = live_snapshot();
        let after_ack: Result<Option<ParentSnapshot>, String> =
            Err("inspect process 9943 image: No such process".to_string());
        let reverify = tolerate_gone_reverify(after_ack); // the authorize_carried_child mapping
        verify_child_macos(&hello, reverify.as_ref(), std::process::id())
            .expect("an uninspectable (exited) post-ACK peer is benign");
    }
}

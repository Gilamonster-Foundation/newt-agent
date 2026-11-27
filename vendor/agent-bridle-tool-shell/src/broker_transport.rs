//! One bounded, authenticated, content-addressed RPC per fresh socket.
//!
//! An inherited endpoint only accepts delegated reply sockets. Helper frames
//! therefore cannot interleave with another helper's request or response. The
//! server challenge is a freshness locator; MerkleNode content IDs identify the
//! actual request/reply, and replies commit to the exact request ID.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use agent_bridle_core::{receive_control_endpoint, send_control_endpoint, ToolError, ToolResult};
use content_addressable::{canonical, ContentId, MerkleNode};
use serde::{Deserialize, Serialize};

use crate::command_broker::{BrokerControl, BrokerPeer};

const MAX_FRAME: usize = 1024 * 1024;
const HELLO: &[u8; 8] = b"ABBR-H1\0";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    version: u8,
    #[serde(with = "serde_bytes")]
    challenge: Vec<u8>,
    #[serde(with = "serde_bytes")]
    payload: Vec<u8>,
    error: Option<String>,
    endpoint: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    id: ContentId,
    node: MerkleNode<Body>,
}

pub(crate) struct RpcReply {
    pub payload: Vec<u8>,
    pub endpoint: Option<UnixStream>,
}

/// A client for one explicitly delegated, invocation-scoped helper endpoint.
///
/// The descriptor is duplicated safely and is never authority by its number or
/// by a message kind. The host authenticates the helper and independently checks
/// the registered native parent and domain policy before answering a request.
pub struct BrokerClient {
    endpoint: UnixStream,
}

impl BrokerClient {
    /// Duplicate an explicitly delegated endpoint without opening `/dev/fd` or
    /// taking ownership of the caller's descriptor. The duplicate is CLOEXEC.
    /// A helper spawning other commands must apply the ordinary ambient-fd guard
    /// so its original inherited endpoint is not delegated onward accidentally.
    pub fn from_delegated_fd(fd: RawFd) -> ToolResult<Self> {
        Ok(Self {
            endpoint: agent_bridle_fdguard::duplicate_control_socket(fd)?,
        })
    }

    pub(crate) fn new(endpoint: UnixStream) -> Self {
        Self { endpoint }
    }

    /// Send bounded opaque request bytes and verify the exact linked response.
    /// Each call uses a fresh socket and challenge; no successful reply can be
    /// replayed into a later request. This does not request additional grants.
    pub fn request(&mut self, payload: &[u8]) -> ToolResult<Vec<u8>> {
        let reply = self.request_inner(payload, Duration::from_secs(30))?;
        if reply.endpoint.is_some() {
            return Err(ToolError::denied(
                "helper reply unexpectedly delegated a descriptor",
            ));
        }
        Ok(reply.payload)
    }

    pub(crate) fn request_inner(&self, payload: &[u8], timeout: Duration) -> ToolResult<RpcReply> {
        if payload.len() > MAX_FRAME / 2 {
            return Err(ToolError::denied(
                "broker request exceeds its bounded payload budget",
            ));
        }
        let (mut local, remote) =
            UnixStream::pair().map_err(|error| broker_io("create pair", error))?;
        local
            .set_read_timeout(Some(timeout))
            .map_err(|error| broker_io("set read timeout", error))?;
        local
            .set_write_timeout(Some(timeout))
            .map_err(|error| broker_io("set write timeout", error))?;
        crate::private_control::prepare_broker_receiver(&local)?;
        send_control_endpoint(&self.endpoint, &remote).map_err(|error| {
            ToolError::denied(format!(
                "broker delegation socket send RPC endpoint: {error}"
            ))
        })?;
        let challenge = read_server_hello(remote, |hello| {
            crate::private_control::read_broker_peer(&local, hello)
        })?;
        local
            .write_all(&std::process::id().to_le_bytes())
            .map_err(|error| broker_io("write client PID", error))?;
        let request = make_frame(
            payload.to_vec(),
            challenge.clone(),
            BTreeSet::new(),
            None,
            false,
        )?;
        write_frame(&mut local, &request)?;
        let response = read_frame(&mut local)?;
        verify_frame(&response, &challenge, [request.id].into_iter().collect())?;
        if let Some(error) = response.node.payload().error.as_ref() {
            return Err(ToolError::denied(error.clone()));
        }
        let endpoint = if response.node.payload().endpoint {
            let endpoint = receive_control_endpoint(&local).map_err(|error| {
                ToolError::denied(format!("broker RPC socket receive reply endpoint: {error}"))
            })?;
            local
                .write_all(&[1])
                .map_err(|error| broker_io("acknowledge reply endpoint", error))?;
            Some(endpoint)
        } else {
            None
        };
        Ok(RpcReply {
            payload: response.node.payload().payload.clone(),
            endpoint,
        })
    }
}

fn read_server_hello(
    remote: impl Sized,
    read_peer: impl FnOnce(&mut [u8; 44]) -> ToolResult<u32>,
) -> ToolResult<Vec<u8>> {
    // The server names itself in a kernel-attested message. Endpoint
    // delegation, not this claimed PID, selected the server capability.
    let mut hello = [0; 44];
    let server_pid = read_peer(&mut hello)
        .map_err(|error| ToolError::denied(format!("broker server hello: {error}")))?;
    if &hello[..8] != HELLO || hello[8..12] != server_pid.to_le_bytes() {
        return Err(ToolError::denied("broker server hello identity mismatch"));
    }
    // macOS can lose peer identity if the transferred reference is closed
    // before receipt. Keep it through the hello and BOTH kernel lookups, with
    // no I/O on this copy. A missing hello uses the finite read timeout; RAII
    // also closes the copy on every authentication or I/O failure.
    drop(remote);
    Ok(hello[12..].to_vec())
}

fn make_frame(
    payload: Vec<u8>,
    challenge: Vec<u8>,
    parents: BTreeSet<ContentId>,
    error: Option<String>,
    endpoint: bool,
) -> ToolResult<Frame> {
    let node = MerkleNode::new(
        Body {
            version: 1,
            challenge,
            payload,
            error,
            endpoint,
        },
        parents,
    );
    let id = node
        .id()
        .map_err(|error| ToolError::denied(error.to_string()))?;
    Ok(Frame { id, node })
}

fn verify_frame(frame: &Frame, challenge: &[u8], parents: BTreeSet<ContentId>) -> ToolResult<()> {
    if frame
        .node
        .id()
        .map_err(|error| ToolError::denied(error.to_string()))?
        != frame.id
        || frame.node.payload().version != 1
        || frame.node.payload().challenge != challenge
        || frame.node.parents() != &parents
    {
        return Err(ToolError::denied(
            "broker frame identity, freshness, or causal link mismatch",
        ));
    }
    Ok(())
}

/// Keep the channel and syscall phase without erasing the I/O error kind.
pub(crate) fn broker_io(operation: &str, error: std::io::Error) -> ToolError {
    ToolError::Exec(std::io::Error::new(
        error.kind(),
        crate::private_control::io_diagnostic(&format!("broker RPC socket {operation}"), error),
    ))
}

fn write_frame(stream: &mut impl Write, frame: &Frame) -> ToolResult<()> {
    let bytes = canonical::to_canonical_dagcbor(frame)
        .map_err(|error| ToolError::denied(error.to_string()))?;
    if bytes.len() > MAX_FRAME {
        return Err(ToolError::denied("broker frame exceeds its bound"));
    }
    stream
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .map_err(|error| broker_io("write frame length", error))?;
    stream
        .write_all(&bytes)
        .map_err(|error| broker_io("write frame body", error))?;
    stream
        .flush()
        .map_err(|error| broker_io("flush frame", error))?;
    Ok(())
}

fn read_frame(stream: &mut impl Read) -> ToolResult<Frame> {
    let mut length = [0; 4];
    stream
        .read_exact(&mut length)
        .map_err(|error| broker_io("read frame length", error))?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(ToolError::denied(
            "broker frame length is outside its bound",
        ));
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .map_err(|error| broker_io("read frame body", error))?;
    // Decoder errors can quote peer-supplied field names or values. This error
    // also reaches the server diagnostic stream, so retain only the phase.
    canonical::from_canonical_dagcbor_checked(&bytes)
        .map_err(|_| ToolError::denied("invalid canonical broker frame"))
}

pub(crate) fn serve_request(
    stream: UnixStream,
    control: &BrokerControl,
    expected_peer: Option<u32>,
    callback: impl FnOnce(&BrokerPeer, &[u8]) -> ToolResult<RpcReply>,
) -> ToolResult<()> {
    with_request_timeout(
        control
            .deadline()
            .saturating_duration_since(std::time::Instant::now()),
        |timeout| serve_request_inner(stream, control, expected_peer, callback, timeout),
    )
}

// The remaining budget is injected so expiry between the control check and
// socket setup can be exercised without a real clock or socket.
fn with_request_timeout(
    remaining: Duration,
    request: impl FnOnce(Duration) -> ToolResult<()>,
) -> ToolResult<()> {
    let timeout = remaining.min(Duration::from_secs(30));
    let result = if timeout.is_zero() {
        Err(ToolError::denied(
            "command broker invocation budget exhausted before hello",
        ))
    } else {
        request(timeout)
    };
    result.map_err(|error| {
        ToolError::denied(format!(
            "broker server request failed (timeout_ms={}): {error}",
            timeout.as_secs_f64() * 1000.0,
        ))
    })
}

fn serve_request_inner(
    mut stream: UnixStream,
    control: &BrokerControl,
    expected_peer: Option<u32>,
    callback: impl FnOnce(&BrokerPeer, &[u8]) -> ToolResult<RpcReply>,
    timeout: Duration,
) -> ToolResult<()> {
    control.check()?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| broker_io("set read timeout", error))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| broker_io("set write timeout", error))?;
    crate::private_control::prepare_broker_receiver(&stream)?;
    let mut challenge = [0; 32];
    getrandom::getrandom(&mut challenge).map_err(|error| ToolError::denied(error.to_string()))?;
    let mut hello = Vec::from(*HELLO);
    hello.extend_from_slice(&std::process::id().to_le_bytes());
    hello.extend_from_slice(&challenge);
    // Setup can consume the remaining budget; never announce an expired request.
    control.check()?;
    stream
        .write_all(&hello)
        .map_err(|error| broker_io("write server hello", error))?;
    let mut declared_pid = [0; 4];
    let peer_pid = crate::private_control::read_broker_peer(&stream, &mut declared_pid)
        .map_err(|error| ToolError::denied(format!("broker client identity: {error}")))?;
    if peer_pid != u32::from_le_bytes(declared_pid)
        || expected_peer.is_some_and(|pid| pid != peer_pid)
    {
        return Err(ToolError::denied(
            "broker request peer is not the registered process",
        ));
    }
    let peer = peer_identity(peer_pid)?;
    let request = read_frame(&mut stream)?;
    verify_frame(&request, &challenge, BTreeSet::new())?;
    if request.node.payload().error.is_some() || request.node.payload().endpoint {
        return Err(ToolError::denied("invalid broker request role"));
    }
    if peer_identity(peer_pid)? != peer {
        return Err(ToolError::denied("broker peer changed during request"));
    }
    control.check()?;
    let result = callback(&peer, &request.node.payload().payload);
    control.check()?;
    let (payload, error, endpoint) = match result {
        Ok(reply) => (reply.payload, None, reply.endpoint),
        Err(error) => (Vec::new(), Some(error.to_string()), None),
    };
    let response = make_frame(
        payload,
        challenge.to_vec(),
        [request.id].into_iter().collect(),
        error,
        endpoint.is_some(),
    )?;
    write_frame(&mut stream, &response)?;
    if let Some(endpoint) = endpoint {
        send_control_endpoint(&stream, &endpoint).map_err(|error| {
            ToolError::denied(format!("broker RPC socket send reply endpoint: {error}"))
        })?;
        read_endpoint_ack(endpoint, &mut stream)?;
    }
    Ok(())
}

fn read_endpoint_ack(endpoint: impl Sized, stream: &mut impl Read) -> ToolResult<()> {
    // The bounded RPC socket carries receipt; never use the retained copy for
    // I/O. Failed, missing or invalid acknowledgements release it by ownership.
    let mut ack = [0];
    stream
        .read_exact(&mut ack)
        .map_err(|error| broker_io("read reply endpoint acknowledgment", error))?;
    if ack != [1] {
        return Err(ToolError::denied("invalid reply endpoint acknowledgment"));
    }
    drop(endpoint);
    Ok(())
}

pub(crate) fn peer_identity(pid: u32) -> ToolResult<BrokerPeer> {
    let (parent, process_birth) = process_parent_and_birth(pid)?;
    let (_, parent_birth) = process_parent_and_birth(parent)?;
    let executable = executable_path(pid)?;
    let own = std::fs::metadata(std::env::current_exe()?)?;
    let other = std::fs::metadata(&executable)?;
    if own.dev() != other.dev() || own.ino() != other.ino() {
        return Err(ToolError::denied(
            "broker helper is not the harness executable image",
        ));
    }
    Ok(BrokerPeer {
        process_id: pid,
        process_birth,
        parent_process_id: parent,
        executable,
        parent_executable: executable_path(parent)?,
        parent_birth,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn executable_path(pid: u32) -> ToolResult<PathBuf> {
    let pid = i32::try_from(pid).map_err(|_| ToolError::denied("invalid process ID"))?;
    let path = libproc::libproc::proc_pid::pidpath(pid).map_err(ToolError::denied)?;
    std::fs::canonicalize(path).map_err(ToolError::from)
}

#[cfg(target_os = "linux")]
pub(crate) fn executable_path(pid: u32) -> ToolResult<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).map_err(ToolError::from)
}

#[cfg(target_os = "macos")]
pub(crate) fn process_parent_and_birth(pid: u32) -> ToolResult<(u32, u64)> {
    let info = libproc::libproc::proc_pid::pidinfo::<libproc::libproc::bsd_info::BSDInfo>(
        i32::try_from(pid).map_err(|_| ToolError::denied("invalid process ID"))?,
        0,
    )
    .map_err(ToolError::denied)?;
    Ok((
        info.pbi_ppid,
        info.pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec),
    ))
}

#[cfg(target_os = "linux")]
pub(crate) fn process_parent_and_birth(pid: u32) -> ToolResult<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let tail = stat
        .rsplit_once(')')
        .ok_or_else(|| ToolError::denied("invalid process identity"))?
        .1;
    let fields: Vec<_> = tail.split_whitespace().collect();
    let parent = fields
        .get(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ToolError::denied("invalid process parent"))?;
    let birth = fields
        .get(19)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ToolError::denied("invalid process birth identity"))?;
    Ok((parent, birth))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Retained<'a>(&'a std::cell::Cell<bool>);

    impl Drop for Retained<'_> {
        fn drop(&mut self) {
            assert!(!self.0.replace(true), "endpoint closed twice");
        }
    }

    #[test]
    fn transferred_endpoint_outlives_hello_and_both_identity_lookups() {
        let closed = std::cell::Cell::new(false);
        let challenge = read_server_hello(Retained(&closed), |hello| {
            assert!(
                !closed.get(),
                "transferred endpoint closed before hello/identity lookup"
            );
            hello[..8].copy_from_slice(HELLO);
            hello[8..12].copy_from_slice(&42_u32.to_le_bytes());
            hello[12..].fill(7);
            // The production closure reads the hello, LOCAL_PEERPID and
            // LOCAL_PEERTOKEN before returning; the copy must outlive it all.
            assert!(
                !closed.get(),
                "transferred endpoint closed before token lookup"
            );
            Ok(42)
        })
        .unwrap();
        assert_eq!(challenge, vec![7; 32]);
        assert!(closed.get());
    }

    #[test]
    fn transferred_endpoint_closes_on_every_hello_failure() {
        for phase in [
            "read peer handshake",
            "LOCAL_PEERPID",
            "LOCAL_PEERTOKEN",
            "magic",
            "pid",
        ] {
            let closed = std::cell::Cell::new(false);
            let error = read_server_hello(Retained(&closed), |hello| {
                assert!(!closed.get(), "transferred endpoint closed before {phase}");
                hello[..8].copy_from_slice(HELLO);
                hello[8..12].copy_from_slice(&42_u32.to_le_bytes());
                match phase {
                    "magic" => {
                        hello[0] = 0;
                        Ok(42)
                    }
                    "pid" => Ok(43),
                    _ => Err(broker_io(phase, std::io::ErrorKind::TimedOut.into())),
                }
            })
            .unwrap_err();
            assert!(closed.get(), "endpoint leaked after {phase}");
            assert!(
                error
                    .to_string()
                    .contains(if phase == "magic" || phase == "pid" {
                        "identity mismatch"
                    } else {
                        phase
                    }),
                "{error}"
            );
        }
    }

    #[test]
    fn transferred_endpoint_reply_waits_for_receipt_and_closes_on_failure() {
        struct Receipt<'a> {
            closed: &'a std::cell::Cell<bool>,
            read: bool,
            result: std::io::Result<u8>,
        }
        impl Read for Receipt<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                assert!(!self.closed.get(), "reply endpoint closed before receipt");
                self.read = true;
                bytes[0] = std::mem::replace(&mut self.result, Ok(0))?;
                Ok(1)
            }
        }
        for result in [
            Ok(1),
            Ok(0),
            Err(std::io::ErrorKind::TimedOut.into()),
            Err(std::io::ErrorKind::UnexpectedEof.into()),
        ] {
            let success = matches!(&result, Ok(1));
            let closed = std::cell::Cell::new(false);
            let mut receipt = Receipt {
                closed: &closed,
                read: false,
                result,
            };
            let result = read_endpoint_ack(Retained(&closed), &mut receipt);
            assert!(receipt.read, "sender never waited for endpoint receipt");
            assert_eq!(result.is_ok(), success);
            assert!(closed.get(), "reply endpoint leaked");
        }
    }

    #[test]
    fn exhausted_broker_budget_never_starts_the_hello() {
        // Inject a clock at and past the invocation deadline after an earlier
        // successful control check. The old floor starts the abandoned hello.
        let deadline = Duration::from_secs(60);
        for now in [deadline, deadline + Duration::from_millis(1)] {
            let mut hello_written = false;
            let error = with_request_timeout(deadline.saturating_sub(now), |_| {
                hello_written = true;
                Err(broker_io(
                    "read client PID",
                    std::io::ErrorKind::TimedOut.into(),
                ))
            })
            .unwrap_err();
            assert!(
                !hello_written,
                "exhausted invocation wrote a hello then timed out: {error}"
            );
            assert!(error.to_string().contains("budget exhausted"), "{error}");
            assert!(error.to_string().contains("timeout_ms=0"), "{error}");
        }
    }

    #[test]
    fn broker_timeout_keeps_the_remaining_budget_and_cap() {
        for (remaining, expected) in [
            (Duration::from_micros(500), Duration::from_micros(500)),
            (Duration::from_secs(60), Duration::from_secs(30)),
        ] {
            with_request_timeout(remaining, |timeout| {
                assert_eq!(timeout, expected);
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn broker_server_diagnostic_names_timeout_phase_kind_and_errno() {
        let errno = nix::errno::Errno::ENOTCONN as i32;
        let error = with_request_timeout(Duration::from_millis(7), |_| {
            Err(broker_io(
                "read peer handshake",
                std::io::Error::from_raw_os_error(errno),
            ))
        })
        .unwrap_err();
        let message = error.to_string();
        for expected in [
            "timeout_ms=7".to_owned(),
            "broker RPC socket read peer handshake".to_owned(),
            "kind=NotConnected".to_owned(),
            format!("raw_os_error=Some({errno})"),
        ] {
            assert!(message.contains(&expected), "missing {expected}: {message}");
        }

        // Authenticated reads pass through a String-valued error before the
        // broker adds its context. Keep metadata even when there is no errno.
        let error = with_request_timeout(Duration::from_micros(500), |_| {
            Err(ToolError::denied(crate::private_control::io_diagnostic(
                "receive authenticated control frame",
                std::io::ErrorKind::TimedOut.into(),
            )))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "denied: broker server request failed (timeout_ms=0.5): denied: receive authenticated control frame: timed out (kind=TimedOut, raw_os_error=None)");
    }

    #[test]
    fn broker_server_diagnostic_does_not_echo_malformed_payload() {
        for bytes in [
            canonical::to_canonical_dagcbor(&"private-payload-marker").unwrap(),
            canonical::to_canonical_dagcbor(&std::collections::BTreeMap::from([(
                "private-payload-marker",
                1,
            )]))
            .unwrap(),
        ] {
            let mut wire = (bytes.len() as u32).to_le_bytes().to_vec();
            wire.extend_from_slice(&bytes);
            let error = with_request_timeout(Duration::from_secs(1), |_| {
                read_frame(&mut wire.as_slice()).map(|_| ())
            })
            .unwrap_err();
            let message = error.to_string();
            assert!(!message.contains("private-payload-marker"), "{message}");
            assert!(
                message.contains("invalid canonical broker frame"),
                "{message}"
            );
        }
    }

    /// Inject macOS's disconnected-socket errno on Linux without real I/O.
    struct Disconnected;

    impl Read for Disconnected {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from_raw_os_error(
                nix::errno::Errno::ENOTCONN as i32,
            ))
        }
    }

    impl Write for Disconnected {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from_raw_os_error(
                nix::errno::Errno::ENOTCONN as i32,
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            unreachable!("the write must fail first")
        }
    }

    #[test]
    fn enotconn_read_names_broker_socket_and_frame_phase() {
        for (prefix, phase) in [
            (Vec::new(), "read frame length"),
            (4_u32.to_le_bytes().to_vec(), "read frame body"),
        ] {
            let mut stream = std::io::Cursor::new(prefix).chain(Disconnected);
            let error = read_frame(&mut stream)
                .err()
                .expect("disconnect must refuse");
            assert!(
                matches!(&error, ToolError::Exec(e) if e.kind() == std::io::ErrorKind::NotConnected)
            );
            let message = error.to_string();
            assert!(message.contains("broker RPC socket"), "{message}");
            assert!(message.contains(phase), "{message}");
        }
    }

    #[test]
    fn enotconn_write_names_broker_socket_and_refuses() {
        let frame = make_frame(Vec::new(), vec![7; 32], BTreeSet::new(), None, false).unwrap();
        let error = write_frame(&mut Disconnected, &frame).expect_err("disconnect must refuse");
        assert!(
            matches!(&error, ToolError::Exec(e) if e.kind() == std::io::ErrorKind::NotConnected)
        );
        assert!(
            error
                .to_string()
                .contains("broker RPC socket write frame length"),
            "{error}"
        );
    }

    #[test]
    fn enotconn_regression_complete_frame_survives_eof_but_truncation_refuses() {
        let frame =
            make_frame(b"reply".to_vec(), vec![7; 32], BTreeSet::new(), None, false).unwrap();
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).unwrap();
        let received = read_frame(&mut bytes.as_slice()).unwrap();
        verify_frame(&received, &[7; 32], BTreeSet::new()).unwrap();
        assert_eq!(received.node.payload().payload, b"reply");
        for length in [0, 3, bytes.len() - 1] {
            let error = read_frame(&mut &bytes[..length])
                .err()
                .expect("truncated frame must refuse");
            assert!(
                matches!(&error, ToolError::Exec(e) if e.kind() == std::io::ErrorKind::UnexpectedEof)
            );
            assert!(error.to_string().contains("broker RPC socket"), "{error}");
        }
    }

    /// Grounds the message-credential check against a real child and a reply
    /// socket created in that child then transferred to its parent. In
    /// particular, the socketpair creator is not the server's identity.
    #[test]
    fn actual_writer_identity_survives_endpoint_transfer_and_rejects_forgery() {
        for forged in [false, true] {
            let (host, child) = UnixStream::pair().unwrap();
            host.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "broker_transport::tests::broker_peer_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("BRIDLE_BROKER_TEST_FORGED", if forged { "1" } else { "0" })
                .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(child)))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            agent_bridle_fdguard::deny_inherited_fds(&mut command);
            let mut child = command.spawn().unwrap();
            let pid = child.id();
            let socket = receive_control_endpoint(&host).unwrap();
            let control = BrokerControl {
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                deadline: std::time::Instant::now() + Duration::from_secs(10),
            };
            let called = std::cell::Cell::new(false);
            let served = serve_request(socket, &control, Some(pid), |peer, bytes| {
                called.set(true);
                assert_eq!(peer.process_id, pid);
                assert_eq!(peer.parent_process_id, std::process::id());
                assert_eq!(bytes, b"bound request");
                Ok(RpcReply {
                    payload: b"bound reply".to_vec(),
                    endpoint: None,
                })
            });
            if forged {
                assert!(served.is_err());
                assert!(!called.get());
            } else {
                assert!(served.is_ok(), "{served:?}");
                assert!(called.get());
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while child.try_wait().unwrap().is_none() {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    #[ignore = "self-exec fixture, run by the positive/forgery parent test"]
    fn broker_peer_child() {
        use std::os::fd::AsFd;
        let mode = std::env::var("BRIDLE_BROKER_TEST_FORGED").expect("parent fixture marker");
        let endpoint = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
        if mode == "0" {
            let mut client = BrokerClient::new(endpoint);
            assert_eq!(client.request(b"bound request").unwrap(), b"bound reply");
        } else {
            let (mut local, remote) = UnixStream::pair().unwrap();
            local
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            crate::private_control::prepare_broker_receiver(&local).unwrap();
            send_control_endpoint(&endpoint, &remote).unwrap();
            let mut hello = [0; 44];
            let server = crate::private_control::read_broker_peer(&local, &mut hello).unwrap();
            assert_eq!(server, nix::unistd::getppid().as_raw() as u32);
            drop(remote);
            // Claim the server's PID; kernel credentials still name this child.
            local.write_all(&server.to_le_bytes()).unwrap();
            let mut byte = [0];
            assert_eq!(local.read(&mut byte).unwrap(), 0);
        }
    }

    #[test]
    fn causal_content_address_refuses_substitution_and_replay() {
        let frame = make_frame(
            b"request".to_vec(),
            vec![7; 32],
            BTreeSet::new(),
            None,
            false,
        )
        .unwrap();
        assert!(verify_frame(&frame, &[7; 32], BTreeSet::new()).is_ok());
        assert!(verify_frame(&frame, &[8; 32], BTreeSet::new()).is_err());
        let mut replaced = make_frame(
            b"substituted".to_vec(),
            vec![7; 32],
            BTreeSet::new(),
            None,
            false,
        )
        .unwrap();
        replaced.id = frame.id;
        assert!(verify_frame(&replaced, &[7; 32], BTreeSet::new()).is_err());
        let reply = make_frame(
            b"reply".to_vec(),
            vec![7; 32],
            [frame.id].into_iter().collect(),
            None,
            false,
        )
        .unwrap();
        assert!(verify_frame(&reply, &[7; 32], [frame.id].into_iter().collect()).is_ok());
        assert!(verify_frame(&reply, &[7; 32], BTreeSet::new()).is_err());
    }
}

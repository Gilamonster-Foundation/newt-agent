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
        let (mut local, remote) = UnixStream::pair()?;
        local.set_read_timeout(Some(timeout))?;
        local.set_write_timeout(Some(timeout))?;
        crate::private_control::prepare_broker_receiver(&local)?;
        send_control_endpoint(&self.endpoint, &remote)?;
        drop(remote);
        // The server names itself in a kernel-attested message. Endpoint
        // delegation, not this claimed PID, selected the server capability.
        let mut hello = [0; 44];
        let server_pid = crate::private_control::read_broker_peer(&local, &mut hello)?;
        if &hello[..8] != HELLO || hello[8..12] != server_pid.to_le_bytes() {
            return Err(ToolError::denied("broker server hello identity mismatch"));
        }
        let challenge = hello[12..].to_vec();
        local.write_all(&std::process::id().to_le_bytes())?;
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
            Some(receive_control_endpoint(&local)?)
        } else {
            None
        };
        Ok(RpcReply {
            payload: response.node.payload().payload.clone(),
            endpoint,
        })
    }
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

fn write_frame(stream: &mut UnixStream, frame: &Frame) -> ToolResult<()> {
    let bytes = canonical::to_canonical_dagcbor(frame)
        .map_err(|error| ToolError::denied(error.to_string()))?;
    if bytes.len() > MAX_FRAME {
        return Err(ToolError::denied("broker frame exceeds its bound"));
    }
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> ToolResult<Frame> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(ToolError::denied(
            "broker frame length is outside its bound",
        ));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    canonical::from_canonical_dagcbor_checked(&bytes)
        .map_err(|error| ToolError::denied(format!("invalid canonical broker frame: {error}")))
}

pub(crate) fn serve_request(
    mut stream: UnixStream,
    control: &BrokerControl,
    expected_peer: Option<u32>,
    callback: impl FnOnce(&BrokerPeer, &[u8]) -> ToolResult<RpcReply>,
) -> ToolResult<()> {
    control.check()?;
    let timeout = control
        .deadline()
        .saturating_duration_since(std::time::Instant::now())
        .min(Duration::from_secs(30))
        .max(Duration::from_millis(1));
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    crate::private_control::prepare_broker_receiver(&stream)?;
    let mut challenge = [0; 32];
    getrandom::getrandom(&mut challenge).map_err(|error| ToolError::denied(error.to_string()))?;
    let mut hello = Vec::from(*HELLO);
    hello.extend_from_slice(&std::process::id().to_le_bytes());
    hello.extend_from_slice(&challenge);
    stream.write_all(&hello)?;
    let mut declared_pid = [0; 4];
    let peer_pid = crate::private_control::read_broker_peer(&stream, &mut declared_pid)?;
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
        send_control_endpoint(&stream, &endpoint)?;
    }
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
            drop(remote);
            let mut hello = [0; 44];
            let server = crate::private_control::read_broker_peer(&local, &mut hello).unwrap();
            assert_eq!(server, nix::unistd::getppid().as_raw() as u32);
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

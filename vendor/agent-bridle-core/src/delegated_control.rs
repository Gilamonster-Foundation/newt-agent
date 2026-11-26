//! Bounded transport for an explicitly delegated Unix control endpoint.
//!
//! This is a descriptor transport, not authentication or a grant. Callers must
//! authenticate the enclosing private channel before accepting an endpoint and
//! apply their own protocol policy to every request carried by it.

use std::io::Write;
use std::mem::MaybeUninit;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

use rustix::io::{IoSlice, IoSliceMut};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};

use crate::{ToolError, ToolResult};

const MARKER: [u8; 8] = *b"ABTC-F1\0";
// Receive the whole kernel-supported rights message before rejecting extra fds.
// Darwin externalizes *all* rights before copyout_control truncates the buffer;
// unseen fd numbers cannot be closed by OwnedFd. XNU bounds a control record by
// MCLBYTES (2048) in sockargs, and UIPC_MAX_CMSG_FD (512) covers even 32-bit
// senders. unp_internalize accepts one rights message per record; recvmsg stops
// at the next record's control data. Do not shrink this to the accepted count.
// See xnu-11215.1.10: bsd/kern/{uipc_syscalls,uipc_usrreq,uipc_socket}.c.
#[cfg(target_os = "macos")]
const RECEIVE_CONTROL_SPACE: usize = rustix::cmsg_space!(ScmRights(512));
// Linux SCM_MAX_FD, plus credentials requested by the authenticated bootstrap.
// cmsg_space! also includes the padding needed to align the byte buffer.
#[cfg(target_os = "linux")]
const RECEIVE_CONTROL_SPACE: usize = rustix::cmsg_space!(ScmRights(253), ScmCredentials(1));

/// Send exactly one separate Unix stream endpoint over an authenticated channel.
///
/// Ownership stays with the caller; the receiver obtains a new owned reference.
/// No filesystem path is opened and no filesystem/network grant is changed.
pub fn send_control_endpoint(channel: &UnixStream, endpoint: &UnixStream) -> ToolResult<()> {
    let descriptors = [endpoint.as_fd()];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    if !ancillary.push(SendAncillaryMessage::ScmRights(&descriptors)) {
        return Err(ToolError::denied(
            "control endpoint ancillary buffer is too small",
        ));
    }
    let sent = rustix::net::sendmsg(
        channel,
        &[IoSlice::new(&MARKER)],
        &mut ancillary,
        SendFlags::empty(),
    )
    .map_err(std::io::Error::from)?;
    if sent == 0 || sent > MARKER.len() {
        return Err(ToolError::denied(
            "control endpoint transfer made no progress",
        ));
    }
    (&*channel).write_all(&MARKER[sent..])?;
    Ok(())
}

/// Receive exactly one connected Unix stream endpoint, owned and close-on-exec.
///
/// Only call this after authenticating the channel peer. Unknown markers,
/// truncated control data, missing/extra descriptors, non-sockets and other
/// socket families are rejected. Rejected descriptors are closed by ownership.
pub fn receive_control_endpoint(channel: &UnixStream) -> ToolResult<UnixStream> {
    let mut marker = [0; MARKER.len()];
    let mut space = [MaybeUninit::uninit(); RECEIVE_CONTROL_SPACE];
    let mut descriptors = Vec::new();
    let mut offset = 0;
    while offset < marker.len() {
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        let received = rustix::net::recvmsg(
            channel,
            &mut [IoSliceMut::new(&mut marker[offset..])],
            &mut ancillary,
            // Linux makes the received descriptors CLOEXEC atomically, avoiding
            // a concurrent fork/exec inheritance window. macOS has no equivalent;
            // its confined spawn funnel additionally scrubs all ambient fds.
            #[cfg(target_os = "linux")]
            RecvFlags::CMSG_CLOEXEC,
            #[cfg(not(target_os = "linux"))]
            RecvFlags::empty(),
        )
        .map_err(std::io::Error::from)?;
        for item in ancillary.drain() {
            match item {
                RecvAncillaryMessage::ScmRights(fds) => descriptors.extend(fds),
                #[cfg(target_os = "linux")]
                RecvAncillaryMessage::ScmCredentials(_) => {}
                _ => {
                    return Err(ToolError::denied(
                        "unexpected control endpoint ancillary data",
                    ))
                }
            }
        }
        if received.bytes == 0
            || received.bytes > marker.len() - offset
            || received.flags.contains(ReturnFlags::CTRUNC)
            || received.flags.contains(ReturnFlags::TRUNC)
            || descriptors.len() != 1
        {
            return Err(ToolError::denied(
                "control transfer requires exactly one complete endpoint",
            ));
        }
        // Every fragment must use recvmsg: a plain read could discard an extra
        // rights message attached to the marker's tail without owning its fds.
        offset += received.bytes;
    }
    if marker != MARKER {
        return Err(ToolError::denied(
            "invalid control endpoint transfer marker",
        ));
    }
    let endpoint = descriptors.pop().expect("exactly one descriptor checked");
    rustix::io::fcntl_setfd(&endpoint, rustix::io::FdFlags::CLOEXEC)
        .map_err(std::io::Error::from)?;
    if rustix::net::sockopt::socket_type(&endpoint).map_err(std::io::Error::from)?
        != rustix::net::SocketType::STREAM
        || rustix::net::getsockname(&endpoint)
            .map_err(std::io::Error::from)?
            .address_family()
            != rustix::net::AddressFamily::UNIX
    {
        return Err(ToolError::denied(
            "delegated control endpoint must be a Unix stream socket",
        ));
    }
    let endpoint = UnixStream::from(endpoint);
    endpoint.peer_addr()?;
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn separate_endpoint_is_usable_and_close_on_exec() {
        let (parent, child) = UnixStream::pair().unwrap();
        let (endpoint, mut peer) = UnixStream::pair().unwrap();
        send_control_endpoint(&parent, &endpoint).unwrap();
        let mut received = receive_control_endpoint(&child).unwrap();
        assert!(rustix::io::fcntl_getfd(&received)
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC));
        drop(endpoint);
        received.write_all(b"private").unwrap();
        let mut bytes = [0; 7];
        peer.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"private");
        // The original bootstrap is an independent channel, not the broker.
        (&parent).write_all(b"a").unwrap();
        let mut byte = [0];
        (&child).read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"a");
    }

    #[test]
    fn missing_or_duplicate_endpoints_are_refused() {
        let (parent, child) = UnixStream::pair().unwrap();
        (&parent).write_all(&MARKER).unwrap();
        assert!(receive_control_endpoint(&child).is_err());
        let (one, _peer) = UnixStream::pair().unwrap();
        let (two, _peer) = UnixStream::pair().unwrap();
        let descriptors = [one.as_fd(), two.as_fd()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        assert!(ancillary.push(SendAncillaryMessage::ScmRights(&descriptors)));
        rustix::net::sendmsg(
            &parent,
            &[IoSlice::new(&MARKER)],
            &mut ancillary,
            SendFlags::empty(),
        )
        .unwrap();
        assert!(receive_control_endpoint(&child).is_err());
    }

    #[test]
    fn ordinary_files_cannot_be_transferred_as_control_sockets() {
        let (parent, child) = UnixStream::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let descriptors = [file.as_fd()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        assert!(ancillary.push(SendAncillaryMessage::ScmRights(&descriptors)));
        rustix::net::sendmsg(
            &parent,
            &[IoSlice::new(&MARKER)],
            &mut ancillary,
            SendFlags::empty(),
        )
        .unwrap();
        assert!(receive_control_endpoint(&child).is_err());
        assert!(file.metadata().is_ok());
    }

    fn send_raw(channel: &UnixStream, fds: &[std::os::fd::BorrowedFd<'_>], marker: &[u8]) {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_TRANSFER_FDS))];
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        assert!(ancillary.push(SendAncillaryMessage::ScmRights(fds)));
        assert_eq!(
            rustix::net::sendmsg(
                channel,
                &[IoSlice::new(marker)],
                &mut ancillary,
                SendFlags::empty()
            )
            .unwrap(),
            marker.len()
        );
    }

    // Linux SCM_MAX_FD; Darwin's LP64 sockargs limit is
    // (MCLBYTES - sizeof(cmsghdr)) / sizeof(fileglob *) = (2048 - 12) / 8.
    #[cfg(target_os = "linux")]
    const MAX_TRANSFER_FDS: usize = 253;
    #[cfg(target_os = "macos")]
    const MAX_TRANSFER_FDS: usize = 254;

    fn isolated(case: &str, check: impl FnOnce()) {
        const CHILD_CASE: &str = "BRIDLE_DELEGATED_CONTROL_TEST_CASE";
        if std::env::var(CHILD_CASE).as_deref() == Ok(case) {
            // macOS commonly starts with a soft limit of 256. Leave room for
            // the maximum batch plus the harness's own descriptors; only this
            // isolated child changes its limit, and never its hard limit.
            use rustix::process::{getrlimit, setrlimit, Resource};
            let mut limit = getrlimit(Resource::Nofile);
            if limit.current.is_some_and(|current| current < 512) {
                limit.current = Some(512);
                setrlimit(Resource::Nofile, limit).expect("test needs a 512-fd soft limit");
            }
            check();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("delegated_control::tests::{case}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_CASE, case)
            .output()
            .unwrap();
        // Emit each child's diagnostics together, even with parallel tests.
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success(),
            "isolated descriptor check failed: {case}\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    fn open_descriptors() -> std::collections::BTreeSet<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        #[cfg(target_os = "linux")]
        let path = "/proc/self/fd";
        #[cfg(target_os = "macos")]
        let path = "/dev/fd";
        let directory = rustix::fs::Dir::new(std::fs::File::open(path).unwrap()).unwrap();
        let directory_fd = directory.fd().unwrap().as_raw_fd();
        directory
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let fd = entry.file_name().to_str().unwrap().parse().ok()?;
                (fd != directory_fd).then_some(fd)
            })
            .collect()
    }

    fn rejected_transfer(case: &str, marker: &[u8], count: usize) {
        rejected_fragments(case, &[(marker, count)]);
    }

    fn rejected_fragments(case: &str, fragments: &[(&[u8], usize)]) {
        isolated(case, || {
            use std::os::fd::AsRawFd;
            let (parent, child) = UnixStream::pair().unwrap();
            let (endpoint, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let count: usize = fragments.iter().map(|(_, count)| count).sum();
            for &(marker, count) in fragments {
                send_raw(&parent, &vec![endpoint.as_fd(); count], marker);
            }
            parent.shutdown(std::net::Shutdown::Write).unwrap();
            drop(endpoint);
            let mut before = open_descriptors();
            let refused = receive_control_endpoint(&child).is_err();
            let after_refusal = open_descriptors();
            let extra_after_refusal: Vec<_> = after_refusal.difference(&before).copied().collect();
            let peer_before_drop = peer.read(&mut [0]);
            before.remove(&child.as_raw_fd());
            drop(child);
            let after_drop = open_descriptors();
            let extra_after_drop: Vec<_> = after_drop.difference(&before).copied().collect();
            let peer_after_drop = peer.read(&mut [0]);
            eprintln!(
                "{case}: sent={count}; refused={refused}; \
                 extra_fds_after_refusal={extra_after_refusal:?}; peer_before_drop={peer_before_drop:?}; \
                 extra_fds_after_drop={extra_after_drop:?}; peer_after_drop={peer_after_drop:?}"
            );
            assert!(refused, "{case}: malformed transfer accepted");
            assert!(
                extra_after_refusal.is_empty(),
                "{case}: descriptors installed and leaked"
            );
            assert_eq!(
                after_drop, before,
                "{case}: descriptors remain after socket drop"
            );
            assert!(
                matches!(peer_before_drop, Ok(0)),
                "{case}: rights remain before socket drop"
            );
            assert!(
                matches!(peer_after_drop, Ok(0)),
                "{case}: rights remain after socket drop"
            );
        });
    }

    #[test]
    fn malformed_wrong_marker() {
        rejected_transfer("malformed_wrong_marker", b"wrong!!!", 1);
    }

    #[test]
    fn malformed_short_marker() {
        rejected_transfer("malformed_short_marker", &MARKER[..3], 1);
    }

    #[test]
    fn malformed_two_descriptors() {
        rejected_transfer("malformed_two_descriptors", &MARKER, 2);
    }

    #[test]
    fn malformed_eighty_descriptors() {
        rejected_transfer("malformed_eighty_descriptors", &MARKER, 80);
    }

    #[test]
    fn malformed_maximum_descriptors() {
        rejected_transfer("malformed_maximum_descriptors", &MARKER, MAX_TRANSFER_FDS);
    }

    #[test]
    fn malformed_extra_descriptor_on_marker_tail() {
        rejected_fragments(
            "malformed_extra_descriptor_on_marker_tail",
            &[(&MARKER[..3], 1), (&MARKER[3..], 1)],
        );
    }

    #[test]
    fn maximum_rights_batch_is_received_without_truncation() {
        let case = "maximum_rights_batch_is_received_without_truncation";
        isolated(case, || {
            let (parent, child) = UnixStream::pair().unwrap();
            // The bootstrap may also request Linux credentials.
            #[cfg(target_os = "linux")]
            rustix::net::sockopt::set_socket_passcred(&child, true).unwrap();
            let (endpoint, _peer) = UnixStream::pair().unwrap();
            send_raw(&parent, &vec![endpoint.as_fd(); MAX_TRANSFER_FDS], &MARKER);
            let mut space = [MaybeUninit::uninit(); RECEIVE_CONTROL_SPACE];
            let mut ancillary = RecvAncillaryBuffer::new(&mut space);
            let mut marker = [0; MARKER.len()];
            let received = rustix::net::recvmsg(
                &child,
                &mut [IoSliceMut::new(&mut marker)],
                &mut ancillary,
                RecvFlags::empty(),
            )
            .unwrap();
            let mut count = 0;
            for item in ancillary.drain() {
                if let RecvAncillaryMessage::ScmRights(fds) = item {
                    count += fds.count(); // Each OwnedFd is dropped, including on failure.
                }
            }
            let truncated = received.flags.contains(ReturnFlags::CTRUNC);
            let complete = received.bytes == MARKER.len() && marker == MARKER;
            eprintln!(
                "{case}: sent={MAX_TRANSFER_FDS}; received={count}; \
                 ctrunc={truncated}; marker_complete={complete}"
            );
            assert!(!truncated, "receiver must see every fd");
            assert_eq!(count, MAX_TRANSFER_FDS);
            assert!(complete);
        });
    }

    #[test]
    fn split_marker_is_completed_without_losing_the_endpoint() {
        let (parent, child) = UnixStream::pair().unwrap();
        let (endpoint, mut peer) = UnixStream::pair().unwrap();
        send_raw(&parent, &[endpoint.as_fd()], &MARKER[..3]);
        (&parent).write_all(&MARKER[3..]).unwrap();
        let mut received = receive_control_endpoint(&child).unwrap();
        received.write_all(b"x").unwrap();
        let mut byte = [0];
        peer.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"x");
    }

    #[test]
    fn wrong_socket_type_family_and_disconnected_stream_are_refused() {
        use rustix::net::{socket, AddressFamily, SocketType};
        for (family, kind) in [
            (AddressFamily::UNIX, SocketType::DGRAM),
            (AddressFamily::INET, SocketType::STREAM),
            (AddressFamily::UNIX, SocketType::STREAM),
        ] {
            let (parent, child) = UnixStream::pair().unwrap();
            let endpoint = socket(family, kind, None).unwrap();
            rustix::io::fcntl_setfd(&endpoint, rustix::io::FdFlags::CLOEXEC).unwrap();
            send_raw(&parent, &[endpoint.as_fd()], &MARKER);
            assert!(receive_control_endpoint(&child).is_err());
            assert!(
                rustix::io::fcntl_getfd(&endpoint).is_ok(),
                "sender retains ownership"
            );
        }
    }
}

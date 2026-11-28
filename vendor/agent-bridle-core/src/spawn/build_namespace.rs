//! Private build startup: creator provenance plus kernel-owned process handles.

use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

/// Stable handles created atomically with init and payload. No PID is reopened.
/// A reaped handle is authenticated by the private report channel's credentials;
/// Linux discards its per-process metadata, but the handle remains pollable.
#[derive(Debug)]
pub struct BuildNamespace {
    init: OwnedFd,
    payload: OwnedFd,
    waited: std::sync::atomic::AtomicBool,
    /// Host coordinate from the trusted creator report, never a lookup operand.
    pub payload_pid: u32,
}

/// Separate bounded teardown allowance, including timeout and drop paths.
/// The syscall may remain uninterruptible beyond this bound; report that fact.
pub const BUILD_TEARDOWN_BOUND: Duration = Duration::from_millis(500);

impl Drop for BuildNamespace {
    fn drop(&mut self) {
        let bound = if self.waited.load(std::sync::atomic::Ordering::Relaxed) {
            Duration::ZERO
        } else {
            BUILD_TEARDOWN_BOUND
        };
        if let Err(error) = self.terminate(bound) {
            eprintln!("{error}");
        }
    }
}

impl BuildNamespace {
    pub(super) fn receive(channel: OwnedFd, relay: u32) -> io::Result<Self> {
        let receive = || {
            let mut bytes = [0u8; 17];
            // Own all Linux SCM_MAX_FD rights, including those we will reject.
            let mut space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(253), ScmCredentials(1))];
            let mut ancillary = RecvAncillaryBuffer::new(&mut space);
            let message = rustix::net::recvmsg(
                &channel,
                &mut [rustix::io::IoSliceMut::new(&mut bytes)],
                &mut ancillary,
                RecvFlags::CMSG_CLOEXEC | RecvFlags::DONTWAIT,
            )?;
            let mut descriptors = Vec::new();
            let mut credentials = Vec::new();
            let mut truncated = message
                .flags
                .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC);
            for item in ancillary.drain() {
                match item {
                    RecvAncillaryMessage::ScmRights(fds) => descriptors.extend(fds),
                    RecvAncillaryMessage::ScmCredentials(c) => credentials.push([
                        c.pid.as_raw_nonzero().get() as u32,
                        c.uid.as_raw(),
                        c.gid.as_raw(),
                    ]),
                    _ => truncated = true,
                }
            }
            Ok::<_, io::Error>((bytes, message.bytes, descriptors, credentials, truncated))
        };
        let (bytes, count, descriptors, credentials, truncated) = receive()?;
        // The creator closes its sole sender before closing std's exec-status
        // pipe. After spawn, a second message OR an open sender is a refusal.
        let (_, next_count, next_fds, next_credentials, next_truncated) = receive()?;
        let eof = next_count == 0
            && next_fds.is_empty()
            && next_credentials.is_empty()
            && !next_truncated;
        let info: Vec<_> = descriptors
            .iter()
            .map(|fd| std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())))
            .collect::<io::Result<_>>()?;
        let refs: Vec<_> = info.iter().map(String::as_str).collect();
        let pids = validate_report(
            &bytes[..count.min(bytes.len())],
            truncated,
            &credentials,
            [
                relay,
                rustix::process::getuid().as_raw(),
                rustix::process::getgid().as_raw(),
            ],
            &refs,
            eof,
        )?;
        let mut fds = descriptors.into_iter();
        Ok(Self {
            waited: std::sync::atomic::AtomicBool::new(false),
            init: fds.next().expect("two validated handles"),
            payload: fds.next().expect("two validated handles"),
            payload_pid: pids[1],
        })
    }

    /// Wait for the kernel to finish namespace teardown, bounded by the caller.
    /// Readability includes reaped handles. Expiry is an error, never a claim
    /// that descendants stuck in kernel I/O have stopped.
    pub fn wait_gone(&self, bound: Duration) -> io::Result<()> {
        self.waited
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let start = Instant::now();
        loop {
            let remaining = bound.saturating_sub(start.elapsed());
            let timeout = rustix::event::Timespec::try_from(remaining).map_err(|_| {
                io::Error::other("private PID namespace teardown bound is too large")
            })?;
            let mut poll = [rustix::event::PollFd::new(
                &self.init,
                rustix::event::PollFlags::IN,
            )];
            match rustix::event::poll(&mut poll, Some(&timeout)) {
                Ok(_) if poll[0].revents().intersects(rustix::event::PollFlags::IN | rustix::event::PollFlags::HUP) => return Ok(()),
                Err(rustix::io::Errno::INTR) if start.elapsed() < bound => continue,
                Err(error) => return Err(error.into()),
                _ => return Err(io::Error::new(io::ErrorKind::TimedOut,
                    "private PID namespace teardown did not complete within its budget; descendants may still be blocked in kernel I/O")),
            }
        }
    }

    /// Stop all namespace members through init's stable handle.
    pub fn terminate(&self, bound: Duration) -> io::Result<()> {
        match rustix::process::pidfd_send_signal(&self.init, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => self.wait_gone(bound),
            Err(error) => Err(error.into()),
        }
    }

    /// The payload handle is retained for namespace-aware worker authentication.
    pub fn payload_handle(&self) -> std::os::fd::BorrowedFd<'_> {
        self.payload.as_fd()
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid private PID namespace startup report",
    )
}

fn validate_report(
    bytes: &[u8],
    truncated: bool,
    credentials: &[[u32; 3]],
    expected: [u32; 3],
    fdinfo: &[&str],
    eof: bool,
) -> io::Result<[u32; 2]> {
    if truncated
        || !eof
        || credentials != [expected]
        || fdinfo.len() != 2
        || bytes.len() != 16
        || &bytes[..8] != b"ABPID1\0\0"
    {
        return Err(invalid());
    }
    let pids = [
        u32::from_ne_bytes(bytes[8..12].try_into().map_err(|_| invalid())?),
        u32::from_ne_bytes(bytes[12..16].try_into().map_err(|_| invalid())?),
    ];
    if pids[0] == pids[1]
        || pids
            .iter()
            .any(|pid| *pid == 0 || *pid > i32::MAX as u32 || *pid == expected[0])
    {
        return Err(invalid());
    }
    for (index, info) in fdinfo.iter().enumerate() {
        let field = |name: &str| -> io::Result<Option<&str>> {
            let mut matches = info.lines().filter_map(|line| line.strip_prefix(name));
            let value = matches.next();
            if matches.next().is_some() {
                return Err(invalid());
            }
            Ok(value)
        };
        let pid: i32 = field("Pid:")?
            .ok_or_else(invalid)?
            .trim()
            .parse()
            .map_err(|_| invalid())?;
        let nspids = field("NSpid:")?
            .map(|line| {
                line.split_whitespace()
                    .map(str::parse::<i32>)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|_| invalid())?;
        if pid == -1 {
            // Deliberate residual: identity after reap comes from the trusted
            // creator channel and SCM_CREDENTIALS, not discarded task metadata.
            if nspids.as_ref().is_some_and(|ids| ids != &[-1]) {
                return Err(invalid());
            }
        } else if pid <= 0
            || pid as u32 != pids[index]
            || nspids.as_ref().is_some_and(|ids| {
                ids.first() != Some(&pid) || ids.last() != Some(&(index as i32 + 1))
            })
        {
            return Err(invalid());
        }
    }
    Ok(pids)
}

#[cfg(test)]
mod tests {
    use super::*;
    const EXPECTED: [u32; 3] = [40, 1000, 1000];
    const LIVE: [&str; 2] = ["Pid:\t41\nNSpid:\t41\t1\n", "Pid:\t42\nNSpid:\t42\t2\n"];
    fn report() -> Vec<u8> {
        let mut bytes = b"ABPID1\0\0".to_vec();
        bytes.extend(41u32.to_ne_bytes());
        bytes.extend(42u32.to_ne_bytes());
        bytes
    }
    #[test]
    fn accepts_exact_credentialed_report_and_live_handles() {
        assert_eq!(
            validate_report(&report(), false, &[EXPECTED], EXPECTED, &LIVE, true).unwrap(),
            [41, 42]
        );
    }
    #[test]
    fn accepts_reaped_handles_on_creator_provenance() {
        assert_eq!(
            validate_report(
                &report(),
                false,
                &[EXPECTED],
                EXPECTED,
                &["Pid:\t-1\nNSpid:\t-1\n", LIVE[1]],
                true
            )
            .unwrap(),
            [41, 42]
        );
    }
    #[test]
    fn refuses_missing_short_extra_or_malformed_report() {
        let bytes = report();
        for end in 0..bytes.len() {
            assert!(
                validate_report(&bytes[..end], false, &[EXPECTED], EXPECTED, &LIVE, true).is_err()
            );
        }
        for invalid in [
            vec![0; 16],
            [bytes.clone(), vec![0]].concat(),
            [b"ABPID1\0\0".as_slice(), &[0; 8]].concat(),
        ] {
            assert!(validate_report(&invalid, false, &[EXPECTED], EXPECTED, &LIVE, true).is_err());
        }
    }
    #[test]
    fn refuses_truncation_or_second_message() {
        for (truncated, eof) in [(true, true), (false, false)] {
            assert!(
                validate_report(&report(), truncated, &[EXPECTED], EXPECTED, &LIVE, eof).is_err()
            );
        }
    }
    #[test]
    fn refuses_missing_extra_or_wrong_kernel_credentials() {
        for credentials in [
            vec![],
            vec![EXPECTED, EXPECTED],
            vec![[99, 1000, 1000]],
            vec![[40, 99, 1000]],
            vec![[40, 1000, 99]],
        ] {
            assert!(
                validate_report(&report(), false, &credentials, EXPECTED, &LIVE, true).is_err()
            );
        }
    }
    #[test]
    fn refuses_missing_extra_non_pidfd_or_malformed_handles() {
        for infos in [
            vec![],
            vec![LIVE[0]],
            vec![LIVE[0], LIVE[1], LIVE[1]],
            vec![LIVE[0], "pos:\t0\n"],
            vec![LIVE[0], "Pid:\tbad\n"],
            vec![LIVE[0], "Pid:\t0\nNSpid:\t0\n"],
            vec![LIVE[0], "Pid:\t42\nPid:\t42\nNSpid:\t42\t2\n"],
        ] {
            assert!(
                validate_report(&report(), false, &[EXPECTED], EXPECTED, &infos, true).is_err()
            );
        }
    }
    #[test]
    fn refuses_each_live_pid_or_namespace_role_mismatch() {
        for infos in [
            [LIVE[1], LIVE[0]],
            [LIVE[0], "Pid:\t43\nNSpid:\t43\t2\n"],
            [LIVE[0], "Pid:\t42\nNSpid:\t42\t1\n"],
            [LIVE[0], "Pid:\t42\nNSpid:\t99\t2\n"],
            ["Pid:\t41\nNSpid:\t41\t2\n", LIVE[1]],
        ] {
            assert!(
                validate_report(&report(), false, &[EXPECTED], EXPECTED, &infos, true).is_err()
            );
        }
    }
}

//! Bounded process-**tree** termination.
//!
//! A managed execution owns a *tree*, not a process. Signalling only the direct
//! child leaves grandchildren running and, worse, leaves them holding the
//! inherited stdout/stderr writers — which is precisely how a lifecycle ends up
//! reporting a terminal while output producers are still alive.
//!
//! Every execution's direct child is therefore spawned into a **fresh process
//! group** (`ConfinedCommand::new_process_group`), so the group id equals the
//! child's pid and the group contains that child and its descendants and
//! nothing else. Signalling the group is thus bounded ownership: it can never
//! reach a process this execution did not create.
//!
//! Core is `forbid(unsafe_code)`, so the Unix signal path goes through
//! `rustix`'s safe wrapper rather than a raw `libc` FFI call — the same
//! primitive `agent-bridle-tool-shell` already uses to reap a Brush worker tree.

/// Which termination a tree-wide signal requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreeSignal {
    /// Ask the tree to terminate (SIGTERM). May be ignored by the target.
    Graceful,
    /// Terminate the tree now (SIGKILL). Cannot be caught.
    Forced,
}

/// Whether the platform can prove tree-wide termination for a managed
/// execution, or only best-effort termination of the direct child.
// Exactly one variant is constructed per target, so the others are
// legitimately dead on any single platform. Keeping the whole enum compiled
// everywhere is what lets the public `LocalTreeContainment` mapping be
// exhaustive and total rather than itself cfg-shaped.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreeContainment {
    /// The whole descendant tree is signalled as one unit.
    ProcessGroup,
    /// Descendants are reached through the platform's tree-killer utility.
    PlatformTreeKill,
    /// No tree primitive is available; only the direct child can be signalled.
    DirectChildOnly,
}

/// The containment this platform actually provides.
#[must_use]
pub(crate) const fn containment() -> TreeContainment {
    #[cfg(unix)]
    {
        TreeContainment::ProcessGroup
    }
    #[cfg(all(not(unix), windows))]
    {
        TreeContainment::PlatformTreeKill
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        TreeContainment::DirectChildOnly
    }
}

/// Signal the whole process tree rooted at `pid`.
///
/// `pid` must be the leader of the execution's own process group (Unix) — that
/// is what `ConfinedCommand::new_process_group` arranges.
#[cfg(unix)]
pub(crate) fn signal_tree(pid: u32, signal: TreeSignal) {
    use rustix::process::{kill_process_group, Pid, Signal};
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    let Some(pid) = Pid::from_raw(raw) else {
        return;
    };
    let signal = match signal {
        TreeSignal::Graceful => Signal::TERM,
        TreeSignal::Forced => Signal::KILL,
    };
    // A failure here is almost always ESRCH — the group already went away,
    // which is the outcome the caller wanted.
    let _ = kill_process_group(pid, signal);
}

/// Signal the whole process tree rooted at `pid`.
///
/// Windows has no process groups with Unix semantics. `taskkill /T` walks the
/// parent/child relation and terminates the tree, which is a real tree kill and
/// needs no `unsafe` FFI — but it is *not* a job object, so it cannot contain a
/// descendant that has re-parented itself. [`containment`] reports this
/// honestly as [`TreeContainment::PlatformTreeKill`] rather than claiming
/// process-group equivalence.
#[cfg(all(not(unix), windows))]
pub(crate) fn signal_tree(pid: u32, signal: TreeSignal) {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("taskkill");
    cmd.arg("/PID").arg(pid.to_string()).arg("/T");
    if signal == TreeSignal::Forced {
        cmd.arg("/F");
    }
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// No tree primitive on this platform; the caller falls back to the direct
/// child, and [`containment`] says so.
#[cfg(all(not(unix), not(windows)))]
pub(crate) fn signal_tree(_pid: u32, _signal: TreeSignal) {}

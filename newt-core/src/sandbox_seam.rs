//! The sandbox data newt-core's own signatures take in place of agent-bridle's types, so the
//! code behind them can be built without a bridle crate (#3160, the `confine` feature).
//!
//! A launcher's sandbox grants a child more than its `Caveats` name: a read base, the
//! executable directories, the dynamic loaders and a few writable device sinks. Code that
//! decides what a grant can reach has to count those paths too. [`SandboxRoots`] is those four
//! lists. They are the only fields of `agent_bridle::SandboxPolicy` that the admission guard
//! (`workspace_protection`) and the smart-harness frame check (`config::smart_harness`) read,
//! so both read them from this type and neither names bridle.
//!
//! [`SandboxKind`] is the OS sandbox a confined run reports it ran under
//! (`confined_exec::ConfinedOutput`).
//!
//! The two `From` impls are the only conversions that know bridle.
//!
//! [`HeldReadRoot`] and [`CommandBroker`] are bridle's own types, named from here by the
//! executor's request and the build's Git reads (their held read roots) and by the confined
//! shell's entry point (its broker). Only bridle's fence reads a held root, and only bridle's
//! shell calls a broker.

/// A read root the caller holds as a descriptor, which the executor's Landlock fence anchors
/// on. Bridle's type: its one constructor checks the descriptor against the path.
pub use agent_bridle::HeldReadRoot;
/// What the confined shell asks before it runs a resolved command (the native Git commit
/// broker). Bridle's trait: its methods take bridle's command and context types.
pub use agent_bridle_tool_shell::CommandBroker;

/// The paths a launcher's sandbox adds to a child's explicit filesystem `Caveats`, each list as
/// the launcher's policy resolves it. The guard and the frame check count every list as
/// readable, and the device sinks as writable too.
pub struct SandboxRoots {
    /// The read base: loader and library trees and system data.
    pub(crate) base_read_paths: Vec<String>,
    /// Directories of executables.
    pub(crate) bin_read_paths: Vec<String>,
    /// The dynamic loader files.
    pub(crate) loader_paths: Vec<String>,
    /// Discard devices a child may always open for reading and writing.
    pub(crate) device_sink_paths: Vec<String>,
}

impl SandboxRoots {
    /// The roots of the policy newt-core's own executor installs, so the guard, the frame check
    /// and the executor read one policy.
    pub(crate) fn runtime() -> Self {
        Self::from(&crate::confined_exec::runtime_sandbox_policy())
    }
}

/// A launcher that builds its own policy converts the policy it launches with.
impl From<&agent_bridle::SandboxPolicy> for SandboxRoots {
    fn from(policy: &agent_bridle::SandboxPolicy) -> Self {
        Self {
            base_read_paths: policy.base_read_paths.resolve(),
            bin_read_paths: policy.bin_read_paths.resolve(),
            loader_paths: policy.loader_paths.resolve(),
            device_sink_paths: policy.device_sink_paths.resolve(),
        }
    }
}

/// The OS sandbox that actually confined a child: the kind its launcher reports, never the
/// kind that was asked for. `None` means no OS sandbox, so the leash was advisory only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxKind {
    /// A Landlock ruleset (Linux).
    Landlock,
    /// A Seatbelt profile (macOS).
    Seatbelt,
    /// An AppContainer token (Windows).
    AppContainer,
    /// A minimal-rootfs mount-namespace jail (Linux).
    MinimalRootfs,
    /// A micro-VM guest (Linux).
    MicroVm,
    /// No OS sandbox: advisory only.
    None,
}

/// The kind bridle's launcher reports, variant for variant. There is no wildcard arm, so a kind
/// bridle adds stops this compiling instead of reading as `None`.
impl From<agent_bridle::SandboxKind> for SandboxKind {
    fn from(kind: agent_bridle::SandboxKind) -> Self {
        match kind {
            agent_bridle::SandboxKind::Landlock => Self::Landlock,
            agent_bridle::SandboxKind::Seatbelt => Self::Seatbelt,
            agent_bridle::SandboxKind::AppContainer => Self::AppContainer,
            agent_bridle::SandboxKind::MinimalRootfs => Self::MinimalRootfs,
            agent_bridle::SandboxKind::MicroVm => Self::MicroVm,
            agent_bridle::SandboxKind::None => Self::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandBroker, HeldReadRoot, SandboxKind, SandboxRoots};
    use std::sync::Arc;

    /// `roots` holds each of `policy`'s four lists as bridle resolves it, which is how bridle
    /// reads them when it builds the sandbox.
    fn assert_resolved(roots: &SandboxRoots, policy: &agent_bridle::SandboxPolicy) {
        assert_eq!(roots.base_read_paths, policy.base_read_paths.resolve());
        assert_eq!(roots.bin_read_paths, policy.bin_read_paths.resolve());
        assert_eq!(roots.loader_paths, policy.loader_paths.resolve());
        assert_eq!(roots.device_sink_paths, policy.device_sink_paths.resolve());
    }

    /// The conversion reads each list into its own field, with the operator's additions: the
    /// defaults then the extras, or the extras alone under `replace`.
    #[test]
    fn the_roots_are_the_resolved_policy_lists() {
        let mut policy = agent_bridle::SandboxPolicy::default();
        assert_resolved(&SandboxRoots::from(&policy), &policy);

        policy.base_read_paths.extra.push("/seam/base".into());
        policy.bin_read_paths.extra.push("/seam/bin".into());
        policy.device_sink_paths.extra.push("/seam/device".into());
        policy.loader_paths.extra.push("/seam/loader".into());
        policy.loader_paths.replace = true;
        let roots = SandboxRoots::from(&policy);
        assert_resolved(&roots, &policy);
        // Literals, so the comparison above cannot pass over lists that lack the additions.
        assert_eq!(roots.base_read_paths.last().unwrap(), "/seam/base");
        assert_eq!(roots.bin_read_paths.last().unwrap(), "/seam/bin");
        assert_eq!(roots.device_sink_paths.last().unwrap(), "/seam/device");
        assert_eq!(roots.loader_paths, ["/seam/loader"]);
    }

    /// The guard and the frame check see the policy the executor installs, and that policy
    /// grants roots.
    #[test]
    fn the_runtime_roots_are_what_the_executor_installs() {
        let roots = SandboxRoots::runtime();
        assert_resolved(&roots, &crate::confined_exec::runtime_sandbox_policy());
        assert!(!roots.base_read_paths.is_empty());
    }

    /// Each kind bridle's launcher reports becomes the newt-core kind of the same name, which
    /// prints the same: the move check and the confined-git refusal print it.
    #[test]
    fn the_sandbox_kind_is_the_one_bridle_reports() {
        use agent_bridle::SandboxKind as Bridle;
        let mut names = std::collections::BTreeSet::new();
        for (theirs, ours) in [
            (Bridle::Landlock, SandboxKind::Landlock),
            (Bridle::Seatbelt, SandboxKind::Seatbelt),
            (Bridle::AppContainer, SandboxKind::AppContainer),
            (Bridle::MinimalRootfs, SandboxKind::MinimalRootfs),
            (Bridle::MicroVm, SandboxKind::MicroVm),
            (Bridle::None, SandboxKind::None),
        ] {
            let converted = SandboxKind::from(theirs);
            assert_eq!(converted, ours, "{theirs:?}");
            assert_eq!(format!("{converted:?}"), format!("{theirs:?}"));
            names.insert(format!("{converted:?}"));
        }
        // Anti-vacuous: six kinds with six different names.
        assert_eq!(names.len(), 6);
    }

    // The two names are bridle's own types, not look-alikes. Each coercion compiles only when
    // its two types are one: a newtype does not coerce, and in this direction neither does a
    // trait that extends bridle's.
    const _: fn(agent_bridle::HeldReadRoot) -> HeldReadRoot = |root| root;
    const _: fn(Arc<dyn agent_bridle_tool_shell::CommandBroker>) -> Arc<dyn CommandBroker> =
        |broker| broker;
}

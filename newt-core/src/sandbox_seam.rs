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
//! The `From<&agent_bridle::SandboxPolicy>` impl is the one conversion that knows bridle.

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

#[cfg(test)]
mod tests {
    use super::SandboxRoots;

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
}

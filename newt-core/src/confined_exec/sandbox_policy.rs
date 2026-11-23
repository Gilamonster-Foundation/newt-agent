//! The sandbox policy the executor installs. The confined shell's policy and the sandbox roots
//! are built from it too, so it is a child module of its own, and a cfg can gate the executor
//! without it (#3160). Moved from `confined_exec/executor.rs` unchanged.

#[cfg(target_os = "macos")]
use std::path::Path;

#[cfg(target_os = "macos")]
use super::selected_developer_directory;

/// System runtime reads shared by shell execution, build execution, and frame
/// admission. macOS's /usr/bin/git is an xcrun shim; its selected developer
/// directory can live outside Bridle's default /Library runtime roots.
pub(crate) fn runtime_sandbox_policy() -> agent_bridle::SandboxPolicy {
    // agent-bridle 0.8's L3 admission bound (`AdmittedFence::admit`) only
    // resolves a RESTRICTED `net` axis when `child_network == DenyDirect`;
    // under the default `LandlockOnly` the axis is `Unknown` and every
    // `ConstrainedExecutor` spawn fails closed with "not decidable against
    // the delegated grant ∪ declared runtime closure (L3 BOUND)". This
    // STRENGTHENS the fence (adds the seccomp `socket()` + io_uring deny on
    // top of Landlock's TCP-only rule) — it widens nothing. Matches the
    // floor `b1_run_command_sandbox_policy` already applies to the
    // run_command lane (see its doc comment).
    let policy = agent_bridle::SandboxPolicy {
        child_network: agent_bridle::ChildNetworkPolicy::DenyDirect,
        macos_private_ptys: cfg!(target_os = "macos"),
        ..agent_bridle::SandboxPolicy::default()
    };
    #[cfg(target_os = "macos")]
    {
        let mut policy = policy;
        if let Some(developer) = selected_developer_directory() {
            policy.base_read_paths.extra.push(developer.to_owned());
            // Xcode's launchers load libraries beside Contents/Developer.
            // Reading the selected tools without these sibling frameworks
            // admits the executable but leaves dyld unable to start it.
            if let Some(contents) = Path::new(developer).parent() {
                for name in ["Frameworks", "SharedFrameworks"] {
                    let frameworks = contents.join(name);
                    if frameworks.is_dir() {
                        policy
                            .base_read_paths
                            .extra
                            .push(frameworks.to_string_lossy().into_owned());
                    }
                }
            }
        }
        policy
    }
    #[cfg(not(target_os = "macos"))]
    policy
}

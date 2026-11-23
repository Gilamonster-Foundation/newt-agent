//! What `ConstrainedExecutor` is in a build without newt-core's `confine` feature: a refusal.
//!
//! The executor (`executor.rs`) is compiled only with `confine`. Without it these names
//! stay, so the code outside this module that imports them compiles unchanged. Every call,
//! whatever its origin, is refused as [`ExecRefused::ConfinementUnenforceable`] before a
//! scratch directory or a process exists, and the three probes report no fence. This file
//! names no agent-bridle crate, tests included: `tests/confine_boundary_ratchet.rs` holds
//! it to that (#3160).

use super::{off_refusal, ConfinedOutput, ExecRefused, ExecRequest};

/// The stand-in for the confined executor: it never runs a child.
pub struct ConstrainedExecutor;

impl ConstrainedExecutor {
    /// Refuse `req`: this build has no confinement backend, and the child is never run
    /// unconfined (#10). The request is not read.
    pub fn run(_req: &ExecRequest) -> Result<ConfinedOutput, ExecRefused> {
        Err(off_refusal())
    }

    /// The same refusal for the async callers.
    pub async fn run_async(_req: ExecRequest) -> Result<ConfinedOutput, ExecRefused> {
        Err(off_refusal())
    }
}

/// No executor is compiled to use a kernel fs fence, so none is available.
#[must_use]
pub fn kernel_fs_fence_available() -> bool {
    false
}

/// No executor is compiled to run a network guard, so none can be resolved.
#[must_use]
pub fn net_guard_available() -> bool {
    false
}

/// No executor is compiled to place a child in a cgroup subtree, so no subtree kill is available.
#[must_use]
pub fn cgroup_subtree_kill_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confined_exec::{
        workspace_confined_caveats, ExecOrigin, NetGrant, CONFINE_OFF_REASON,
    };
    use std::path::Path;

    /// A request for a program that is not there, under a workspace that is not there: an
    /// executor that tried to run it would answer `Spawn`, `Authorize` or a refusal with a
    /// reason of its own, never the one below.
    fn request_for(origin: ExecOrigin, net: NetGrant) -> ExecRequest {
        let workspace = Path::new("/newt-virtual-workspace");
        ExecRequest::new(
            origin,
            "/newt-virtual-workspace/no-such-program",
            ["unused"],
            workspace,
            workspace_confined_caveats(workspace),
        )
        .net_grant(net)
    }

    fn assert_typed_refusal(result: Result<ConfinedOutput, ExecRefused>, what: &str) {
        match result {
            Err(ExecRefused::ConfinementUnenforceable(why)) => {
                assert_eq!(why, CONFINE_OFF_REASON, "{what}");
            }
            other => panic!("{what}: expected the typed refusal, got {other:?}"),
        }
    }

    /// Every origin and network floor gets the one typed refusal, and it says it is
    /// fail-closed. None gets `Spawn`, which is what trying the program would have given.
    #[test]
    fn confine_off_executor_refuses_typed_without_spawning() {
        for origin in [
            ExecOrigin::AgentInfluenced,
            ExecOrigin::TrustedInfra,
            ExecOrigin::BrokerMediated,
        ] {
            for net in [NetGrant::Unrestricted, NetGrant::DenyAll] {
                let result = ConstrainedExecutor::run(&request_for(origin, net));
                if let Err(refusal) = &result {
                    assert!(refusal.to_string().contains("fail-closed"), "{refusal}");
                }
                assert_typed_refusal(result, &format!("{origin:?} {net:?}"));
            }
        }
    }

    /// The async entry refuses the same way, before it reaches the scratch directory it was
    /// asked for: an executor fails on that one with `Spawn(NotFound)`, because the
    /// directories above it are missing.
    #[tokio::test]
    async fn confine_off_run_async_refuses_before_scratch_or_spawn() {
        let request = request_for(ExecOrigin::AgentInfluenced, NetGrant::Unrestricted)
            .scratch_dir("/newt-virtual-workspace/missing/parent/scratch");
        assert_typed_refusal(ConstrainedExecutor::run_async(request).await, "run_async");
    }

    /// No probe reports a fence, so the verifiers and the callers that read them say there
    /// is none. On a host with Landlock the real probe would say yes: this is the test that
    /// fails then.
    #[test]
    fn confine_off_fence_probes_report_absent() {
        assert!(!kernel_fs_fence_available());
        assert!(!net_guard_available());
        assert!(!cgroup_subtree_kill_available());
    }
}

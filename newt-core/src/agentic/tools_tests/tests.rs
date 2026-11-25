use super::*;
use crate::agentic::NoMcp;

#[test]
fn delayed_operator_decision_records_the_actual_permission_tool_result() {
    use crate::agentic::attribution::{record_completed_tool_event, ToolTimer};
    use crate::markup::extension::Stopwatch;
    use crate::Caveats;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    struct Clock(AtomicU64);
    impl Stopwatch for Clock {
        fn read_nanos(&self) -> u128 {
            u128::from(self.0.load(Ordering::Relaxed)) * 1_000_000
        }
    }
    struct Gate {
        clock: Arc<Clock>,
        allow: bool,
    }
    impl PermissionGate for Gate {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            assert_eq!(requests[0].tool, "request_permissions");
            assert_eq!(requests[0].target, "stat");
            crate::lifecycle::emit(crate::lifecycle::LifecycleEvent::Blocked);
            self.clock.0.store(10_002, Ordering::Relaxed);
            crate::lifecycle::emit(crate::lifecycle::LifecycleEvent::Unblocked);
            if self.allow {
                PermissionDecision::Allow(Caveats::top())
            } else {
                PermissionDecision::Deny
            }
        }
        fn ask_dependency_fetch(
            &mut self,
            _: &Caveats,
            _: &PermissionRequest,
        ) -> PermissionDecision {
            panic!("this tool requests exec permission, not a dependency fetch")
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            panic!("this tool requests exec permission, not free text")
        }
    }
    for allow in [true, false] {
        let clock = Arc::new(Clock(AtomicU64::new(0)));
        let timer = ToolTimer::with_clock(clock.clone());
        clock.0.store(2, Ordering::Relaxed);
        let mut gate = Gate {
            clock: clock.clone(),
            allow,
        };
        let args = serde_json::json!({"capability":"exec", "target":"stat"});
        let (granted, replay, result) = execute_request_permissions(
            &Caveats::default(),
            &args,
            Some(&mut gate),
            false,
            20,
            "/workspace",
            None,
        );
        assert_eq!(granted.is_some(), allow, "{result}");
        assert!(replay.is_none());
        clock.0.store(10_007, Ordering::Relaxed);
        let mut events = Vec::new();
        record_completed_tool_event(
            &mut Some(&mut events),
            "request_permissions",
            &args,
            allow,
            None,
            timer,
        );
        assert_eq!(events[0].duration_ms, Some(7));
        assert_eq!(events[0].operator_wait_ms, Some(10_000));
        assert_eq!(events[0].ok, allow);
    }
}

#[derive(Default)]
pub(super) struct RecordingLiveOutput {
    pub(in crate::agentic::tools) events: std::sync::Mutex<Vec<String>>,
}

impl crate::agentic::LiveToolOutput for RecordingLiveOutput {
    fn start(&self, _generation: u64) {
        self.events.lock().unwrap().push("start".into());
    }

    fn write(&self, _generation: u64, stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
        self.events
            .lock()
            .unwrap()
            .push(format!("{stream:?}:{}", String::from_utf8_lossy(chunk)));
    }

    fn finish(&self, _generation: u64) {
        self.events.lock().unwrap().push("finish".into());
    }

    fn abandon(&self, _generation: u64) {
        self.events.lock().unwrap().push("abandon".into());
    }
}

#[cfg(test)]
#[path = "helper_validation.rs"]
mod validation;

#[cfg(test)]
#[path = "helper_find.rs"]
mod find;

#[cfg(test)]
#[path = "helper_live_output.rs"]
mod live_output;

#[cfg(all(test, windows, feature = "windows-appcontainer"))]
#[path = "helper_windows_appcontainer.rs"]
mod windows_appcontainer;

#[cfg(test)]
#[path = "helper_phantom_discovery.rs"]
mod phantom_discovery;

#[cfg(test)]
#[path = "helper_output_bounds.rs"]
mod output_bounds;

#[cfg(test)]
#[path = "helper_catalog.rs"]
mod catalog;

#[cfg(test)]
#[path = "helper_catalog_authority.rs"]
mod catalog_authority;

#[cfg(test)]
#[path = "helper_lifecycle.rs"]
mod lifecycle;

#[cfg(test)]
#[path = "helper_shell_routing.rs"]
mod shell_routing;

#[cfg(test)]
#[path = "helper_shell_authority.rs"]
mod shell_authority;

// Unix-only: the shell `git add` write grant (F32) targets the Linux kernel
// fence. On Windows, `own_gitdir_grants` resolves no write set, so the grant
// fails closed (no shell write on git metadata) and these real-git tests do
// not apply.
//
// `pub(in crate::agentic::tools)`, not private: #2681 round 3's
// `execute_tool_branch_tests::permissions` tests reuse [`git_shell_grant::
// hermetic_git`] for their own real-git fixture rather than open-coding a
// second one that inherits the ambient `GIT_DIR`/`HOME`/hooks.
#[cfg(all(test, unix))]
#[path = "helper_git_shell_grant.rs"]
pub(in crate::agentic::tools) mod git_shell_grant;

// #2274 part 1 — a binary the carried userland does not carry must produce a
// NAMED refusal, distinguishable from a grant denial and from a binary that is
// not installed at all.
#[cfg(test)]
#[path = "helper_absent_binary_refusal.rs"]
mod absent_binary_refusal;

#[cfg(test)]
#[path = "helper_file_provenance.rs"]
mod file_provenance;

#[cfg(test)]
#[path = "helper_collaborators.rs"]
mod collaborators;

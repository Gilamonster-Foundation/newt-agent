//! Shared stage/host process supervision. Output readers drain concurrently.
use agent_bridle_core::{ToolError, ToolResult};
use std::time::{Duration, Instant};

/// One invocation clock shared by all pipelines and the async timeout owner.
#[derive(Clone)]
pub(crate) struct Deadline {
    timeout: Duration,
    elapsed: std::sync::Arc<dyn Fn() -> Duration + Send + Sync>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Deadline {
    pub(crate) fn new(timeout: Duration) -> Self {
        let started = Instant::now();
        Self::with_clock(timeout, move || started.elapsed())
    }

    pub(crate) fn with_clock(
        timeout: Duration,
        elapsed: impl Fn() -> Duration + Send + Sync + 'static,
    ) -> Self {
        Self {
            timeout,
            elapsed: std::sync::Arc::new(elapsed),
            cancelled: Default::default(),
        }
    }

    pub(crate) fn remaining(&self) -> Duration {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            Duration::ZERO
        } else {
            self.timeout.saturating_sub((self.elapsed)())
        }
    }

    #[cfg(any(feature = "shell", test))]
    pub(crate) fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

trait ChildProcess {
    fn poll(&mut self) -> ToolResult<Option<i32>>;
    fn terminate(&mut self);
    fn reap(&mut self);
}

impl ChildProcess for std::process::Child {
    fn poll(&mut self) -> ToolResult<Option<i32>> {
        self.try_wait()
            .map(|status| status.map(|s| s.code().unwrap_or(-1)))
            .map_err(ToolError::Exec)
    }
    fn terminate(&mut self) {
        crate::kill_child_tree(self);
    }
    fn reap(&mut self) {
        let _ = self.wait();
    }
}

#[cfg(feature = "host-shell")]
pub(crate) fn supervise(
    children: &mut [std::process::Child],
    timeout: Duration,
) -> ToolResult<(i32, bool)> {
    supervise_until(children, &Deadline::new(timeout))
}

pub(crate) fn supervise_until(
    children: &mut [std::process::Child],
    deadline: &Deadline,
) -> ToolResult<(i32, bool)> {
    run_supervisor(
        children,
        || deadline.remaining().is_zero(),
        || std::thread::sleep(Duration::from_millis(15)),
    )
}

#[cfg(test)]
fn supervise_with(
    children: &mut [impl ChildProcess],
    timeout: Duration,
    mut elapsed: impl FnMut() -> Duration,
    pause: impl FnMut(),
) -> ToolResult<(i32, bool)> {
    run_supervisor(children, || elapsed() >= timeout, pause)
}

fn run_supervisor(
    children: &mut [impl ChildProcess],
    mut expired: impl FnMut() -> bool,
    mut pause: impl FnMut(),
) -> ToolResult<(i32, bool)> {
    let mut done = vec![false; children.len()];
    let mut exit_code = -1;
    loop {
        if expired() {
            stop(children);
            return Ok((124, true));
        }
        let mut all_done = true;
        for (i, child) in children.iter_mut().enumerate() {
            if done[i] {
                continue;
            }
            match child.poll() {
                Ok(Some(code)) => {
                    done[i] = true;
                    if i + 1 == done.len() {
                        exit_code = code;
                    }
                }
                Ok(None) => all_done = false,
                Err(error) => {
                    stop(children);
                    return Err(error);
                }
            }
        }
        if all_done {
            return Ok((exit_code, false));
        }
        pause();
    }
}

fn stop(children: &mut [impl ChildProcess]) {
    // Stop every process group before waiting for any one stage.
    for child in children.iter_mut() {
        child.terminate();
    }
    for child in children {
        child.reap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct FakeChild {
        code: Option<i32>,
        killed: bool,
        reaped: bool,
    }
    impl ChildProcess for FakeChild {
        fn poll(&mut self) -> ToolResult<Option<i32>> {
            Ok(self.code)
        }
        fn terminate(&mut self) {
            self.killed = true;
        }
        fn reap(&mut self) {
            assert!(self.killed);
            self.reaped = true;
        }
    }

    /// newt #2732: expiry kills/reaps every stage at the injected budget, not wall time.
    #[test]
    fn issue_2732_budget_expires_and_reaps_all_stages() {
        let mut children = [FakeChild::default(), FakeChild::default()];
        let mut ticks = [Duration::from_secs(59), Duration::from_secs(60)].into_iter();
        let mut pauses = 0;
        let result = supervise_with(
            &mut children,
            Duration::from_secs(60),
            || ticks.next().unwrap(),
            || pauses += 1,
        )
        .unwrap();
        assert_eq!(result, (124, true));
        assert_eq!(pauses, 1);
        assert!(children.iter().all(|c| c.killed && c.reaped));
    }

    /// newt #2732: successful commands keep their real status and are not timed out.
    #[test]
    fn issue_2732_completed_child_keeps_status() {
        let mut children = [FakeChild {
            code: Some(7),
            ..Default::default()
        }];
        assert_eq!(
            supervise_with(
                &mut children,
                Duration::from_secs(60),
                || Duration::ZERO,
                || panic!("completed")
            )
            .unwrap(),
            (7, false)
        );
        assert!(!children[0].killed);
    }
    /// #2732 round 2: a pipeline receives the invocation's remaining second,
    /// not a new 60-second budget. Expiry stops and reaps the active group.
    #[test]
    fn round2_supervisor_uses_remaining_invocation_budget() {
        use std::sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        };
        let ticks = Arc::new(AtomicU64::new(59));
        let clock = ticks.clone();
        let deadline = Deadline::with_clock(Duration::from_secs(60), move || {
            Duration::from_secs(clock.load(Ordering::SeqCst))
        });
        let mut children = [FakeChild::default()];
        let mut pauses = 0;
        assert_eq!(
            run_supervisor(
                &mut children,
                || deadline.remaining().is_zero(),
                || {
                    pauses += 1;
                    ticks.fetch_add(1, Ordering::SeqCst);
                }
            )
            .unwrap(),
            (124, true)
        );
        assert_eq!(pauses, 1);
        assert!(children[0].killed && children[0].reaped);
    }

    /// #2732 round 2: outer expiry cancels the same deadline and termination
    /// completes before the supervisor can return its timeout status.
    #[test]
    fn round2_outer_cancellation_reaps_active_group() {
        let deadline = Deadline::with_clock(Duration::from_secs(60), || Duration::ZERO);
        let worker = deadline.clone();
        deadline.cancel();
        let mut children = [FakeChild::default()];
        assert_eq!(
            run_supervisor(
                &mut children,
                || worker.remaining().is_zero(),
                || panic!("cancelled")
            )
            .unwrap(),
            (124, true)
        );
        assert!(children[0].killed && children[0].reaped);
    }
    /// #2732 round 2: an injected 61-second elapsed time permits build work
    /// to complete but terminates an ordinary probe/search invocation.
    #[test]
    fn round2_build_survives_past_ordinary_deadline() {
        for (budget, expected) in [(1800, (7, false)), (60, (124, true))] {
            let deadline =
                Deadline::with_clock(Duration::from_secs(budget), || Duration::from_secs(61));
            let mut children = [FakeChild {
                code: Some(7),
                ..Default::default()
            }];
            assert_eq!(
                run_supervisor(
                    &mut children,
                    || deadline.remaining().is_zero(),
                    || panic!("no wait")
                )
                .unwrap(),
                expected
            );
            assert_eq!(children[0].killed, expected.1);
        }
    }
}

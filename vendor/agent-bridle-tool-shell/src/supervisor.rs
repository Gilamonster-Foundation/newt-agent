//! Shared stage/host process supervision. Output readers drain concurrently.
use agent_bridle_core::{ToolError, ToolResult};
use std::time::{Duration, Instant};

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

pub(crate) fn supervise(
    children: &mut [std::process::Child],
    timeout: Duration,
) -> ToolResult<(i32, bool)> {
    let started = Instant::now();
    supervise_with(
        children,
        timeout,
        || started.elapsed(),
        || std::thread::sleep(Duration::from_millis(15)),
    )
}

// The same state machine uses injected elapsed time in deterministic tests.
fn supervise_with(
    children: &mut [impl ChildProcess],
    timeout: Duration,
    mut elapsed: impl FnMut() -> Duration,
    mut pause: impl FnMut(),
) -> ToolResult<(i32, bool)> {
    let mut done = vec![false; children.len()];
    let mut exit_code = -1;
    loop {
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
        if elapsed() >= timeout {
            stop(children);
            return Ok((124, true));
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
                || panic!("completed"),
                || panic!("completed")
            )
            .unwrap(),
            (7, false)
        );
        assert!(!children[0].killed);
    }
}

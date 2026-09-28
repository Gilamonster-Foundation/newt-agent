//! A reaped leader with a surviving pipe writer exercises blocking abandonment.

use std::path::Path;
use std::time::{Duration, Instant};

use agent_bridle_core::Tool;

use super::{context, Fixture};

/// #2274: dropping the adapter used to join the core reaper on Tokio's sole
/// reactor thread, stalling unrelated timers throughout the pipe-drain grace.
pub(super) fn cancellation_does_not_block_current_thread() {
    std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let fixture = Fixture::new();
                let arguments = fixture.arguments("orphan-pipe");
                let cx = context(&fixture.caveats());
                let tool = fixture.tool(true);
                let invocation = tokio::spawn(async move { tool.invoke(arguments, &cx).await });
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if let Ok(pid) =
                            std::fs::read_to_string(fixture.workspace.join("exiting-root-pid"))
                        {
                            if !Path::new(&format!("/proc/{pid}")).exists() {
                                break;
                            }
                        }
                        assert!(
                            !invocation.is_finished(),
                            "the live descendant must keep the terminal pending"
                        );
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("leader must be reaped while descendant holds pipe open");
                let before = Instant::now();
                let sibling = tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Instant::now()
                });
                invocation.abort();
                assert!(invocation.await.unwrap_err().is_cancelled());
                let elapsed = sibling.await.unwrap().duration_since(before);
                assert!(
                    elapsed < Duration::from_millis(500),
                    "reactor timer blocked by managed Drop: {elapsed:?}"
                );

                // Abort acknowledgement is not cleanup completion. The existing
                // supervisor must still finish its bounded drain and tree join.
                tokio::time::sleep(Duration::from_millis(3100)).await;
                assert!(
                    !fixture.workspace.join("late-marker").exists(),
                    "cleanup must reap the surviving writer"
                );
            });
    })
    .join()
    .expect("current-thread reactor regression");
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12

//! Real-spawn coverage for the construction-time shell output observer.
#![cfg(all(unix, feature = "shell"))]

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use agent_bridle_core::{Caveats, LimitsPolicy, Registry, Scope};
use agent_bridle_tool_shell::{
    ShellInvocationId, ShellOutputObserver, ShellOutputStream, ShellTool,
};

#[derive(Default)]
struct RecordingObserver {
    chunks: Mutex<Vec<(ShellInvocationId, ShellOutputStream, Vec<u8>)>>,
    finished: Mutex<Vec<ShellInvocationId>>,
    finished_cv: Condvar,
}

impl RecordingObserver {
    fn bytes(&self, stream: ShellOutputStream) -> Vec<u8> {
        self.chunks
            .lock()
            .expect("recorded chunks lock")
            .iter()
            .filter(|(_, seen, _)| *seen == stream)
            .flat_map(|(_, _, chunk)| chunk.iter().copied())
            .collect()
    }

    fn stdout_by_invocation(&self) -> BTreeMap<u64, Vec<u8>> {
        let mut grouped = BTreeMap::new();
        for (invocation, stream, chunk) in self.chunks.lock().expect("recorded chunks lock").iter()
        {
            if *stream == ShellOutputStream::Stdout {
                grouped
                    .entry(invocation.get())
                    .or_insert_with(Vec::new)
                    .extend_from_slice(chunk);
            }
        }
        grouped
    }

    fn wait_finished(&self, count: usize) {
        let finished = self.finished.lock().expect("finished lock");
        let (finished, _timeout) = self
            .finished_cv
            .wait_timeout_while(finished, Duration::from_secs(2), |ids| ids.len() < count)
            .expect("finished condition variable");
        assert!(
            finished.len() >= count,
            "timed out waiting for {count} finishes"
        );
    }

    fn clear(&self) {
        self.chunks.lock().expect("recorded chunks lock").clear();
        self.finished.lock().expect("finished lock").clear();
    }
}

impl ShellOutputObserver for RecordingObserver {
    fn on_output(&self, invocation: ShellInvocationId, stream: ShellOutputStream, chunk: &[u8]) {
        self.chunks.lock().expect("recorded chunks lock").push((
            invocation,
            stream,
            chunk.to_vec(),
        ));
    }

    fn on_finish(&self, invocation: ShellInvocationId) {
        self.finished
            .lock()
            .expect("finished lock")
            .push(invocation);
        self.finished_cv.notify_all();
    }
}

fn exec_only(names: &[&str]) -> Caveats {
    Caveats {
        exec: Scope::only(names.iter().map(|name| (*name).to_string())),
        ..Caveats::top()
    }
}

#[tokio::test]
async fn registry_dispatch_observes_only_admitted_captured_output() {
    let observer = Arc::new(RecordingObserver::default());
    let registry = Registry::builder()
        .tool(Arc::new(
            ShellTool::new().with_output_observer(observer.clone()),
        ))
        .build();
    let grant = registry.mint_grant(Caveats::top());

    let out = registry
        .dispatch(
            "shell",
            serde_json::json!({"program": "echo", "args": ["observed"]}),
            &grant,
        )
        .await
        .expect("dispatch admitted echo");

    observer.wait_finished(1);
    assert_eq!(observer.bytes(ShellOutputStream::Stdout), b"observed\n");
    assert_eq!(observer.bytes(ShellOutputStream::Stderr), b"");
    assert_eq!(out["stdout"], "observed\n");

    observer.clear();
    let restricted_grant = registry.mint_grant(exec_only(&["echo"]));
    let denied = registry
        .dispatch(
            "shell",
            serde_json::json!({"program": "cat", "args": ["/dev/null"]}),
            &restricted_grant,
        )
        .await
        .expect("an in-tool leash refusal is a structured envelope");
    assert_eq!(denied["denied"], true);
    assert!(
        observer
            .chunks
            .lock()
            .expect("recorded chunks lock")
            .is_empty(),
        "a pre-spawn denial must not notify the observer"
    );
}

#[tokio::test]
async fn observer_is_bounded_by_the_configured_output_cap() {
    let observer = Arc::new(RecordingObserver::default());
    let limits = LimitsPolicy {
        max_output_bytes: 4,
        ..LimitsPolicy::default()
    };
    let registry = Registry::builder()
        .tool(Arc::new(
            ShellTool::with_config(limits).with_output_observer(observer.clone()),
        ))
        .build();

    let grant = registry.mint_grant(Caveats::top());
    let out = registry
        .dispatch(
            "shell",
            serde_json::json!({"program": "echo", "args": ["long-output"]}),
            &grant,
        )
        .await
        .expect("dispatch");

    observer.wait_finished(1);
    assert_eq!(observer.bytes(ShellOutputStream::Stdout), b"long");
    assert_eq!(out["stdout"], "long");
    assert_eq!(out["stdout_truncated"], true);
}

struct PanickingObserver(mpsc::Sender<()>);

impl ShellOutputObserver for PanickingObserver {
    fn on_output(&self, _invocation: ShellInvocationId, _stream: ShellOutputStream, _chunk: &[u8]) {
        self.0.send(()).expect("test observes callback");
        panic!("an embedder observer must not unwind through shell execution");
    }
}

#[tokio::test]
async fn observer_panic_does_not_change_the_tool_result() {
    let (called_tx, called_rx) = mpsc::channel();
    let registry = Registry::builder()
        .tool(Arc::new(
            ShellTool::new().with_output_observer(Arc::new(PanickingObserver(called_tx))),
        ))
        .build();

    let grant = registry.mint_grant(Caveats::top());
    let out = registry
        .dispatch(
            "shell",
            serde_json::json!({"program": "echo", "args": ["still-runs"]}),
            &grant,
        )
        .await
        .expect("observer panic is contained");

    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["stdout"], "still-runs\n");
    called_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("panicking callback ran asynchronously");
}

#[tokio::test]
async fn concurrent_dispatches_have_distinct_invocation_ids() {
    let observer = Arc::new(RecordingObserver::default());
    let registry = Registry::builder()
        .tool(Arc::new(
            ShellTool::new().with_output_observer(observer.clone()),
        ))
        .build();
    let grant = registry.mint_grant(Caveats::top());

    let (alpha, beta) = tokio::join!(
        registry.dispatch(
            "shell",
            serde_json::json!({"program": "echo", "args": ["alpha"]}),
            &grant,
        ),
        registry.dispatch(
            "shell",
            serde_json::json!({"program": "echo", "args": ["beta"]}),
            &grant,
        )
    );
    alpha.expect("alpha dispatch");
    beta.expect("beta dispatch");
    observer.wait_finished(2);

    let grouped = observer.stdout_by_invocation();
    assert_eq!(grouped.len(), 2, "each dispatch gets a distinct identity");
    let mut outputs: Vec<Vec<u8>> = grouped.into_values().collect();
    outputs.sort();
    assert_eq!(outputs, vec![b"alpha\n".to_vec(), b"beta\n".to_vec()]);
}

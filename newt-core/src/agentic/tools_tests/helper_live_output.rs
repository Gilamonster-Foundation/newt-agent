use super::*;

/// #1264: the `find` live-stream protocol — the dispatch arm's per-hit
/// producer (each discovery framed as one `line\n` chunk through the
/// relay) emits start → incremental writes in DISCOVERY order → finish.
/// Fully mocked (no fs): the walk's `on_hit` seam is driven directly with
/// the same closure shape the arm wires.
#[test]
fn find_live_stream_emits_start_incremental_writes_then_finish() {
    let sink = std::sync::Arc::new(RecordingLiveOutput::default());
    let mut session = LiveOutputSession::start(Some(sink.clone())).expect("live session");
    let relay = session.relay();
    let on_hit = |line: &str| {
        let mut chunk = line.as_bytes().to_vec();
        chunk.push(b'\n');
        relay.write(crate::agentic::ToolOutputStream::Stdout, &chunk);
    };
    // Discovery order (deliberately not sorted — the live frame shows the
    // walk as it happens; the canonical listing is ordered separately).
    for hit in ["src/b.rs", "src/a.rs", "src/c.rs"] {
        on_hit(hit);
    }
    session.finish();
    assert_eq!(
        *sink.events.lock().unwrap(),
        [
            "start",
            "Stdout:src/b.rs\n",
            "Stdout:src/a.rs\n",
            "Stdout:src/c.rs\n",
            "finish"
        ]
    );
}

/// #1264: after finish, a straggler hit must never reopen the frame — the
/// abandon/no-reopen contract the arm relies on when the walk outruns the
/// presentation worker.
#[test]
fn find_live_stream_hit_after_finish_is_dropped() {
    let sink = std::sync::Arc::new(RecordingLiveOutput::default());
    let mut session = LiveOutputSession::start(Some(sink.clone())).expect("live session");
    let relay = session.relay();
    relay.write(crate::agentic::ToolOutputStream::Stdout, b"early.rs\n");
    session.finish();
    relay.write(crate::agentic::ToolOutputStream::Stdout, b"late.rs\n");
    assert_eq!(
        *sink.events.lock().unwrap(),
        ["start", "Stdout:early.rs\n", "finish"],
        "a post-finish hit must be a no-op"
    );
}

#[test]
fn dropping_live_output_session_finishes_before_returning() {
    struct FinishSignal {
        finished: std::sync::mpsc::Sender<()>,
        abandoned: std::sync::mpsc::Sender<()>,
    }
    impl crate::agentic::LiveToolOutput for FinishSignal {
        fn start(&self, _generation: u64) {}
        fn write(
            &self,
            _generation: u64,
            _stream: crate::agentic::ToolOutputStream,
            _chunk: &[u8],
        ) {
        }
        fn finish(&self, _generation: u64) {
            let _ = self.finished.send(());
        }
        fn abandon(&self, _generation: u64) {
            let _ = self.abandoned.send(());
        }
    }

    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
    let session = LiveOutputSession::start(Some(std::sync::Arc::new(FinishSignal {
        finished: finished_tx,
        abandoned: abandoned_tx,
    })))
    .unwrap();
    drop(session);

    finished_rx
        .try_recv()
        .expect("drop closed the live frame synchronously");
    assert!(
        abandoned_rx.try_recv().is_err(),
        "a responsive sink should finish rather than be abandoned"
    );
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_live_sink_cannot_delay_host_timeout() {
    struct BlockingOutput {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl crate::agentic::LiveToolOutput for BlockingOutput {
        fn start(&self, _generation: u64) {}
        fn write(
            &self,
            _generation: u64,
            _stream: crate::agentic::ToolOutputStream,
            _chunk: &[u8],
        ) {
            let _ = self.entered.send(());
            let _ = self.release.lock().unwrap().recv();
        }
        fn finish(&self, _generation: u64) {}
        fn abandon(&self, _generation: u64) {}
    }

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let sink = std::sync::Arc::new(BlockingOutput {
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    });
    let mut session = LiveOutputSession::start(Some(sink)).unwrap();
    let relay = session.relay();
    let run = tokio::spawn(async move {
        host_shell_output_with_timeout(
            "printf ready; sleep 5",
            ".",
            Some(relay),
            std::time::Duration::from_millis(100),
        )
        .await
    });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(std::time::Duration::from_secs(1)))
        .await
        .unwrap()
        .expect("renderer entered its blocking write");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), run).await;
    if outcome.is_err() {
        let _ = release_tx.send(());
        panic!("blocked presentation defeated the host timeout");
    }
    let run = outcome.unwrap().unwrap().unwrap();
    assert!(run.timed_out);
    assert_eq!(run.exit_code, 124);

    session.cancel();
    release_tx.send(()).unwrap();
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_live_sink_cannot_backpressure_host_pipe_capture() {
    struct BlockingOutput {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl crate::agentic::LiveToolOutput for BlockingOutput {
        fn start(&self, _generation: u64) {}
        fn write(
            &self,
            _generation: u64,
            _stream: crate::agentic::ToolOutputStream,
            _chunk: &[u8],
        ) {
            let _ = self.entered.send(());
            let _ = self.release.lock().unwrap().recv();
        }
        fn finish(&self, _generation: u64) {}
        fn abandon(&self, _generation: u64) {}
    }

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let sink = std::sync::Arc::new(BlockingOutput {
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    });
    let mut session = LiveOutputSession::start(Some(sink)).unwrap();
    let relay = session.relay();
    let run = tokio::spawn(async move {
        host_shell_output_with_timeout(
            "head -c 262144 /dev/zero",
            ".",
            Some(relay),
            std::time::Duration::from_secs(5),
        )
        .await
    });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(std::time::Duration::from_secs(1)))
        .await
        .unwrap()
        .expect("renderer entered its blocking write");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), run).await;
    if outcome.is_err() {
        let _ = release_tx.send(());
        panic!("blocked presentation backpressured host pipe capture");
    }
    let run = outcome.unwrap().unwrap().unwrap();
    assert!(!run.timed_out);
    assert_eq!(run.exit_code, 0);
    assert_eq!(run.stdout.len(), 262_144);

    session.cancel();
    release_tx.send(()).unwrap();
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn host_bypass_publishes_output_before_command_completion() {
    struct ChannelOutput(std::sync::mpsc::Sender<Vec<u8>>);
    impl crate::agentic::LiveToolOutput for ChannelOutput {
        fn start(&self, _generation: u64) {}
        fn write(&self, _generation: u64, _stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
            let _ = self.0.send(chunk.to_vec());
        }
        fn finish(&self, _generation: u64) {}
        fn abandon(&self, _generation: u64) {}
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let sink = std::sync::Arc::new(ChannelOutput(tx));
    let session = LiveOutputSession::start(Some(sink)).unwrap();
    let relay = session.relay();
    let handle = tokio::spawn(async move {
        host_shell_output("printf ready; sleep 0.2; printf done", ".", Some(relay)).await
    });

    let first =
        tokio::task::spawn_blocking(move || rx.recv_timeout(std::time::Duration::from_secs(2)))
            .await
            .unwrap()
            .expect("first live host-shell chunk");
    assert_eq!(first, b"ready");
    assert!(
        !handle.is_finished(),
        "command completed before its live chunk"
    );

    let run = handle.await.unwrap().unwrap();
    assert_eq!(run.stdout, b"readydone");
    assert_eq!(run.exit_code, 0);
}

#[cfg(not(windows))]
#[tokio::test]
async fn bridled_shell_forwards_live_bytes_without_changing_the_envelope() {
    let sink = std::sync::Arc::new(RecordingLiveOutput::default());
    let caveats = crate::caveats::Caveats {
        exec: crate::caveats::Scope::only(["echo".to_string()]),
        ..crate::caveats::Caveats::top()
    };

    let envelope = super::shell::dispatch_bridled_shell(
        serde_json::json!({"cmd": "echo observed", "cwd": "."}),
        &caveats,
        Some(sink.clone()),
    )
    .await
    .expect("confined echo dispatch");

    assert_eq!(envelope["stdout"], "observed\n");
    assert_eq!(
        *sink.events.lock().unwrap(),
        ["start", "Stdout:observed\n", "finish"]
    );
}

//! The live-output relay: bounded-queue streaming from a running shell child
//! to the session's [`crate::agentic::LiveToolOutput`] sink, with panic
//! containment and generation invalidation — carved from `tools.rs`
//! (kernel-first decomposition, handoff §D, carve 3/3; the catalog.rs /
//! output_budget.rs sibling precedent). Pure mechanism: nothing here knows
//! about tools, caveats, or presentation. Everything is `pub(super)`; the
//! public seam is unchanged.
//!
//! The relay's fake-sink test suite lives HERE, beside the state machine it
//! pins (the shared `RecordingLiveOutput` helper stays in the parent's test
//! mod — the `find` live-stream tests use it too).

const LIVE_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
const LIVE_OUTPUT_QUEUE_CHUNKS: usize = 32;
const LIVE_OUTPUT_OBSERVER_WAIT: std::time::Duration = std::time::Duration::from_millis(100);
const LIVE_OUTPUT_FINISH_WAIT: std::time::Duration = std::time::Duration::from_millis(500);
const LIVE_OUTPUT_OPEN: u8 = 0;
const LIVE_OUTPUT_FINISHING: u8 = 1;
const LIVE_OUTPUT_CANCELLED: u8 = 2;
const LIVE_OUTPUT_CLOSED: u8 = 3;

pub(super) enum LiveOutputDispatch {
    Write(crate::agentic::ToolOutputStream, Vec<u8>),
    Wake,
}

pub(super) struct LiveOutputCompletion {
    finished: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
}

pub(super) struct LiveOutputRelay {
    sender: std::sync::mpsc::SyncSender<LiveOutputDispatch>,
    phase: std::sync::Arc<std::sync::atomic::AtomicU8>,
    completion: std::sync::Arc<LiveOutputCompletion>,
}

impl LiveOutputRelay {
    pub(super) fn write(&self, stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::TrySendError;

        for part in chunk.chunks(LIVE_OUTPUT_CHUNK_BYTES) {
            if self.phase.load(Ordering::Acquire) != LIVE_OUTPUT_OPEN {
                break;
            }
            match self
                .sender
                .try_send(LiveOutputDispatch::Write(stream, part.to_vec()))
            {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => break,
                Err(TrySendError::Disconnected(_)) => {
                    self.phase.store(LIVE_OUTPUT_CLOSED, Ordering::Release);
                    break;
                }
            }
        }
    }

    fn request_finish(&self) {
        use std::sync::atomic::Ordering;
        if self
            .phase
            .compare_exchange(
                LIVE_OUTPUT_OPEN,
                LIVE_OUTPUT_FINISHING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let _ = self.sender.try_send(LiveOutputDispatch::Wake);
        }
    }

    pub(super) fn cancel(&self) {
        use std::sync::atomic::Ordering;
        let changed = self
            .phase
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |phase| {
                (phase != LIVE_OUTPUT_CLOSED).then_some(LIVE_OUTPUT_CANCELLED)
            })
            .is_ok();
        if changed {
            let _ = self.sender.try_send(LiveOutputDispatch::Wake);
        }
    }

    fn wait_finished(&self, timeout: std::time::Duration) -> bool {
        let finished = self
            .completion
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (finished, _) = self
            .completion
            .wake
            .wait_timeout_while(finished, timeout, |finished| !*finished)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *finished
    }
}

impl agent_bridle::ShellOutputObserver for LiveOutputRelay {
    fn on_output(
        &self,
        _invocation: agent_bridle::ShellInvocationId,
        stream: agent_bridle::ShellOutputStream,
        chunk: &[u8],
    ) {
        let stream = match stream {
            agent_bridle::ShellOutputStream::Stdout => crate::agentic::ToolOutputStream::Stdout,
            agent_bridle::ShellOutputStream::Stderr => crate::agentic::ToolOutputStream::Stderr,
        };
        self.write(stream, chunk);
    }

    fn on_finish(&self, _invocation: agent_bridle::ShellInvocationId) {
        self.request_finish();
    }
}

pub(super) struct LiveOutputSession {
    relay: std::sync::Arc<LiveOutputRelay>,
    sink: std::sync::Arc<dyn crate::agentic::LiveToolOutput>,
    generation: u64,
    closed: bool,
}

impl LiveOutputSession {
    pub(super) fn start(
        sink: Option<std::sync::Arc<dyn crate::agentic::LiveToolOutput>>,
    ) -> Option<Self> {
        static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let sink = sink?;
        let generation = NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<LiveOutputDispatch>(LIVE_OUTPUT_QUEUE_CHUNKS);
        let phase = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(LIVE_OUTPUT_OPEN));
        let completion = std::sync::Arc::new(LiveOutputCompletion {
            finished: std::sync::Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        let worker_phase = phase.clone();
        let worker_completion = completion.clone();
        let worker_sink = sink.clone();
        if std::thread::Builder::new()
            .name(format!("newt-live-output-{generation}"))
            .spawn(move || {
                run_live_output_dispatch(
                    receiver,
                    worker_sink,
                    generation,
                    &worker_phase,
                    &worker_completion,
                );
            })
            .is_err()
        {
            return None;
        }
        Some(Self {
            relay: std::sync::Arc::new(LiveOutputRelay {
                sender,
                phase,
                completion,
            }),
            sink,
            generation,
            closed: false,
        })
    }

    pub(super) fn relay(&self) -> std::sync::Arc<LiveOutputRelay> {
        self.relay.clone()
    }

    pub(super) fn finish(&mut self) {
        if self.closed {
            return;
        }
        self.relay.request_finish();
        if !self.relay.wait_finished(LIVE_OUTPUT_FINISH_WAIT) {
            self.relay.cancel();
            self.abandon_generation();
        }
        self.closed = true;
    }

    pub(super) fn finish_after_observer(&mut self) {
        if self.closed {
            return;
        }
        if !self.relay.wait_finished(LIVE_OUTPUT_OBSERVER_WAIT) {
            self.relay.request_finish();
            if !self.relay.wait_finished(LIVE_OUTPUT_FINISH_WAIT) {
                self.relay.cancel();
                self.abandon_generation();
            }
        }
        self.closed = true;
    }

    #[cfg(all(test, not(windows)))]
    pub(super) fn cancel(&mut self) {
        if !self.closed {
            self.relay.cancel();
            self.abandon_generation();
            self.closed = true;
        }
    }

    fn abandon_generation(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.sink.abandon(self.generation);
        }));
    }
}

impl Drop for LiveOutputSession {
    fn drop(&mut self) {
        self.finish();
    }
}

pub(super) fn run_live_output_dispatch(
    receiver: std::sync::mpsc::Receiver<LiveOutputDispatch>,
    sink: std::sync::Arc<dyn crate::agentic::LiveToolOutput>,
    generation: u64,
    phase: &std::sync::atomic::AtomicU8,
    completion: &LiveOutputCompletion,
) {
    use std::sync::atomic::Ordering;

    let abandon = || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sink.abandon(generation);
        }));
    };

    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.start(generation))).is_err() {
        phase.store(LIVE_OUTPUT_CANCELLED, Ordering::Release);
        abandon();
        phase.store(LIVE_OUTPUT_CLOSED, Ordering::Release);
        mark_live_output_complete(completion);
        return;
    }

    if phase.load(Ordering::Acquire) == LIVE_OUTPUT_CANCELLED {
        abandon();
        phase.store(LIVE_OUTPUT_CLOSED, Ordering::Release);
        mark_live_output_complete(completion);
        return;
    }

    let deliver = |stream, chunk: Vec<u8>| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sink.write(generation, stream, &chunk);
        }))
        .is_ok()
    };

    loop {
        match receiver.recv() {
            Ok(LiveOutputDispatch::Write(stream, chunk))
                if phase.load(Ordering::Acquire) != LIVE_OUTPUT_CANCELLED =>
            {
                if !deliver(stream, chunk) {
                    phase.store(LIVE_OUTPUT_CANCELLED, Ordering::Release);
                }
            }
            Ok(LiveOutputDispatch::Write(_, _)) | Ok(LiveOutputDispatch::Wake) => {}
            Err(_) => {
                phase.store(LIVE_OUTPUT_CANCELLED, Ordering::Release);
            }
        }

        match phase.load(Ordering::Acquire) {
            LIVE_OUTPUT_OPEN => continue,
            LIVE_OUTPUT_FINISHING => {
                while phase.load(Ordering::Acquire) == LIVE_OUTPUT_FINISHING {
                    let Ok(dispatch) = receiver.try_recv() else {
                        break;
                    };
                    if let LiveOutputDispatch::Write(stream, chunk) = dispatch {
                        if !deliver(stream, chunk) {
                            phase.store(LIVE_OUTPUT_CANCELLED, Ordering::Release);
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
        break;
    }

    if phase.load(Ordering::Acquire) == LIVE_OUTPUT_FINISHING {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.finish(generation)))
            .is_err()
        {
            abandon();
        }
    } else {
        abandon();
    }
    phase.store(LIVE_OUTPUT_CLOSED, Ordering::Release);
    mark_live_output_complete(completion);
}

pub(super) fn mark_live_output_complete(completion: &LiveOutputCompletion) {
    let mut finished = completion
        .finished
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *finished = true;
    completion.wake.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shared fake sink stays in the parent's test mod (the `find`
    // live-stream tests use it too); a child module may reach it.
    use super::super::tests::RecordingLiveOutput;

    #[test]
    fn live_output_session_closes_before_late_chunks() {
        let sink = std::sync::Arc::new(RecordingLiveOutput::default());
        let mut session = LiveOutputSession::start(Some(sink.clone())).expect("live session");
        let relay = session.relay();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"now");
        session.finish();
        relay.write(crate::agentic::ToolOutputStream::Stderr, b"late");

        assert_eq!(
            *sink.events.lock().unwrap(),
            ["start", "Stdout:now", "finish"]
        );
    }

    #[test]
    fn live_output_slow_start_does_not_block_execution_and_is_abandoned_on_drop() {
        struct BlockingStart {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            abandoned: std::sync::mpsc::Sender<()>,
            writes: std::sync::atomic::AtomicUsize,
            finishes: std::sync::atomic::AtomicUsize,
        }
        impl crate::agentic::LiveToolOutput for BlockingStart {
            fn start(&self, _generation: u64) {
                let _ = self.entered.send(());
                let _ = self.release.lock().unwrap().recv();
            }
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
                self.writes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            fn finish(&self, _generation: u64) {
                self.finishes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            fn abandon(&self, _generation: u64) {
                let _ = self.abandoned.send(());
            }
        }

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(BlockingStart {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
            abandoned: abandoned_tx,
            writes: std::sync::atomic::AtomicUsize::new(0),
            finishes: std::sync::atomic::AtomicUsize::new(0),
        });

        let (created_tx, created_rx) = std::sync::mpsc::channel();
        let creator_sink = sink.clone();
        let creator = std::thread::spawn(move || {
            let session = LiveOutputSession::start(Some(creator_sink)).expect("live session");
            let _ = created_tx.send(session);
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("presentation worker entered start");
        let session = match created_rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(session) => session,
            Err(error) => {
                let _ = release_tx.send(());
                creator.join().unwrap();
                panic!("arbitrary sink startup blocked tool execution: {error}");
            }
        };
        creator.join().unwrap();
        let relay = session.relay();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"queued");
        drop(session);
        abandoned_rx
            .try_recv()
            .expect("drop invalidated the blocked startup synchronously");

        release_tx.send(()).unwrap();
        assert!(
            relay.wait_finished(std::time::Duration::from_secs(1)),
            "worker did not close after blocked startup returned"
        );
        assert_eq!(sink.writes.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(
            sink.finishes.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "delayed startup queued a late erase"
        );
    }

    #[test]
    fn live_output_start_panic_is_contained_and_abandoned() {
        struct PanickingStart {
            abandoned: std::sync::mpsc::Sender<()>,
        }
        impl crate::agentic::LiveToolOutput for PanickingStart {
            fn start(&self, _generation: u64) {
                panic!("startup failed");
            }
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
                panic!("failed startup must not receive writes");
            }
            fn finish(&self, _generation: u64) {
                panic!("failed startup must not finish");
            }
            fn abandon(&self, _generation: u64) {
                let _ = self.abandoned.send(());
            }
        }

        let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
        let mut session = LiveOutputSession::start(Some(std::sync::Arc::new(PanickingStart {
            abandoned: abandoned_tx,
        })))
        .expect("worker creation succeeds independently of sink startup");
        session
            .relay()
            .write(crate::agentic::ToolOutputStream::Stdout, b"ignored");
        session.finish();

        abandoned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("startup panic invalidated its generation");
    }

    #[test]
    fn live_output_finish_does_not_wait_for_an_inflight_write() {
        struct BlockingOutput {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            finished: std::sync::mpsc::Sender<()>,
            abandoned: std::sync::mpsc::Sender<()>,
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
            fn finish(&self, _generation: u64) {
                let _ = self.finished.send(());
            }
            fn abandon(&self, _generation: u64) {
                let _ = self.abandoned.send(());
            }
        }

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(BlockingOutput {
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
            finished: finished_tx,
            abandoned: abandoned_tx,
        });
        let mut session = LiveOutputSession::start(Some(sink)).unwrap();
        let relay = session.relay();
        let writer = std::thread::spawn(move || {
            relay.write(crate::agentic::ToolOutputStream::Stdout, b"held");
        });
        entered_rx.recv().unwrap();
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let finisher = std::thread::spawn(move || {
            session.finish();
            let _ = returned_tx.send(());
        });

        returned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("session finish must not wait forever on an arbitrary observer callback");
        abandoned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("timed-out teardown invalidates the generation before returning");
        assert!(
            finished_rx.try_recv().is_err(),
            "timed-out teardown must not queue a late terminal erase"
        );
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        finisher.join().unwrap();
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "worker erased the generation after canonical rendering could resume"
        );
    }

    #[test]
    fn live_output_timeout_invalidates_an_inflight_finish_before_returning() {
        struct SlowFinish {
            finish_entered: std::sync::mpsc::Sender<()>,
            release_finish: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            generation_valid: std::sync::atomic::AtomicBool,
            erased: std::sync::atomic::AtomicBool,
        }
        impl crate::agentic::LiveToolOutput for SlowFinish {
            fn start(&self, _generation: u64) {
                self.generation_valid
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
            }
            fn finish(&self, _generation: u64) {
                let _ = self.finish_entered.send(());
                let _ = self.release_finish.lock().unwrap().recv();
                if self
                    .generation_valid
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    self.erased
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            }
            fn abandon(&self, _generation: u64) {
                self.generation_valid
                    .store(false, std::sync::atomic::Ordering::Release);
            }
        }

        let (finish_entered_tx, finish_entered_rx) = std::sync::mpsc::channel();
        let (release_finish_tx, release_finish_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(SlowFinish {
            finish_entered: finish_entered_tx,
            release_finish: std::sync::Mutex::new(release_finish_rx),
            generation_valid: std::sync::atomic::AtomicBool::new(false),
            erased: std::sync::atomic::AtomicBool::new(false),
        });
        let mut session = LiveOutputSession::start(Some(sink.clone())).unwrap();
        let relay = session.relay();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"paint");
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let finisher = std::thread::spawn(move || {
            session.finish();
            let _ = returned_tx.send(());
        });

        finish_entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker entered finish");
        returned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("bounded teardown returned after invalidating the generation");
        assert!(
            !sink
                .generation_valid
                .load(std::sync::atomic::Ordering::Acquire),
            "canonical rendering resumed before generation invalidation"
        );

        release_finish_tx.send(()).unwrap();
        assert!(relay.wait_finished(std::time::Duration::from_secs(1)));
        finisher.join().unwrap();
        assert!(
            !sink.erased.load(std::sync::atomic::Ordering::Acquire),
            "in-flight finish erased terminal output after canonical rendering resumed"
        );
    }

    #[test]
    fn live_output_cancel_stops_finishing_queue_drain() {
        struct GatedOutput {
            writes: std::sync::Mutex<Vec<Vec<u8>>>,
            first_entered: std::sync::mpsc::Sender<()>,
            release_first: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            held_entered: std::sync::mpsc::Sender<()>,
            release_held: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            finished: std::sync::mpsc::Sender<()>,
        }
        impl crate::agentic::LiveToolOutput for GatedOutput {
            fn start(&self, _generation: u64) {}

            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                chunk: &[u8],
            ) {
                self.writes.lock().unwrap().push(chunk.to_vec());
                match chunk {
                    b"first" => {
                        let _ = self.first_entered.send(());
                        let _ = self.release_first.lock().unwrap().recv();
                    }
                    b"held" => {
                        let _ = self.held_entered.send(());
                        let _ = self.release_held.lock().unwrap().recv();
                    }
                    _ => {}
                }
            }

            fn finish(&self, _generation: u64) {
                let _ = self.finished.send(());
            }

            fn abandon(&self, _generation: u64) {}
        }

        let (first_entered_tx, first_entered_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let (held_entered_tx, held_entered_rx) = std::sync::mpsc::channel();
        let (release_held_tx, release_held_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(GatedOutput {
            writes: std::sync::Mutex::new(Vec::new()),
            first_entered: first_entered_tx,
            release_first: std::sync::Mutex::new(release_first_rx),
            held_entered: held_entered_tx,
            release_held: std::sync::Mutex::new(release_held_rx),
            finished: finished_tx,
        });
        let mut session = LiveOutputSession::start(Some(sink.clone())).unwrap();
        let relay = session.relay();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"first");
        first_entered_rx.recv().unwrap();
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"held");
        relay.write(crate::agentic::ToolOutputStream::Stdout, b"stale");

        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let finisher = std::thread::spawn(move || {
            session.finish();
            let _ = returned_tx.send(());
        });
        while relay.phase.load(std::sync::atomic::Ordering::Acquire) != LIVE_OUTPUT_FINISHING {
            std::thread::yield_now();
        }
        release_first_tx.send(()).unwrap();
        held_entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("finishing worker drained the next queued write");
        returned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("finish cancels after its bounded wait");
        release_held_tx.send(()).unwrap();
        finisher.join().unwrap();
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "cancelled queue drain must not finish after the bounded handoff"
        );

        assert_eq!(
            *sink.writes.lock().unwrap(),
            vec![b"first".to_vec(), b"held".to_vec()]
        );
    }

    #[test]
    fn live_output_write_panic_is_contained_and_abandons_the_generation() {
        struct PanickingOutput(std::sync::mpsc::Sender<()>);
        impl crate::agentic::LiveToolOutput for PanickingOutput {
            fn start(&self, _generation: u64) {}
            fn write(
                &self,
                _generation: u64,
                _stream: crate::agentic::ToolOutputStream,
                _chunk: &[u8],
            ) {
                panic!("presentation failed");
            }
            fn finish(&self, _generation: u64) {
                panic!("cancelled generation must not finish");
            }
            fn abandon(&self, _generation: u64) {
                let _ = self.0.send(());
            }
        }

        let (abandoned_tx, abandoned_rx) = std::sync::mpsc::channel();
        let mut session =
            LiveOutputSession::start(Some(std::sync::Arc::new(PanickingOutput(abandoned_tx))))
                .unwrap();
        session
            .relay()
            .write(crate::agentic::ToolOutputStream::Stdout, b"panic");
        session.finish();

        abandoned_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("panic teardown abandoned the live generation");
    }
}

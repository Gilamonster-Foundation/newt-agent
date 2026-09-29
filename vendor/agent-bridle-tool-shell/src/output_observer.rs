//! Construction-time observer for bounded shell output.

#[cfg(any(feature = "host-shell", feature = "brush"))]
use std::io::Read;
#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
use std::panic::{catch_unwind, AssertUnwindSafe};
#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
use std::sync::{mpsc, Arc, Mutex};

/// The file descriptor from which an observed shell-output chunk originated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellOutputStream {
    /// Bytes captured from standard output.
    Stdout,
    /// Bytes captured from standard error.
    Stderr,
}

/// Process-local identity for one shell tool invocation.
///
/// IDs are monotonically allocated and let a construction-time observer keep
/// concurrent dispatches separate. They carry no authority and are not stable
/// across process restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShellInvocationId(u64);

impl ShellInvocationId {
    /// Return the process-local numeric identity.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Receives bounded shell output while a tool invocation is still running.
///
/// Chunks contain raw bytes and may split a UTF-8 code point. Calls run on a
/// dedicated presentation thread, never a child pipe-draining thread, so a slow
/// observer cannot stall the child or the invocation timeout. A single pipe
/// remains in read order; ordering between stdout, stderr, and separate
/// pipeline-stage stderr pipes follows live enqueue scheduling. Calls for one
/// invocation are serialized on its presentation thread. Separate invocations
/// use separate presentation threads and may call the same observer concurrently,
/// so implementations must synchronize shared state.
///
/// The per-stream budget bounds live delivery, but it does not redefine the
/// completed result. In particular, safe-shell pipeline stderr readers race for
/// the shared live budget, while the final envelope concatenates stderr in
/// pipeline-stage order and then applies its cap. Observed bytes can therefore
/// differ from the final `stderr`; the envelope is authoritative.
///
/// Observation is presentation-only and grants no authority. A callback panic
/// is contained and disables observation for that invocation; it never changes
/// the shell result. Normal completion asynchronously drains chunks already
/// queued within the output cap, then calls [`ShellOutputObserver::on_finish`].
/// Cancellation or timeout stops accepting chunks immediately and does not call
/// `on_finish`, but a callback already dequeued by the presentation thread may
/// begin or finish after the invocation future returns.
pub trait ShellOutputObserver: Send + Sync + 'static {
    /// Observe a non-empty raw output `chunk` from `stream`.
    fn on_output(&self, invocation: ShellInvocationId, stream: ShellOutputStream, chunk: &[u8]);

    /// Report that an ordinarily completed invocation has delivered every
    /// output callback queued before completion.
    ///
    /// This runs on the same presentation thread after all prior `on_output`
    /// calls for `invocation`. Tool dispatch does not wait for it. It is not
    /// called after cancellation, timeout, or an observer panic.
    fn on_finish(&self, _invocation: ShellInvocationId) {}
}

impl<F> ShellOutputObserver for F
where
    F: Fn(ShellInvocationId, ShellOutputStream, &[u8]) + Send + Sync + 'static,
{
    fn on_output(&self, invocation: ShellInvocationId, stream: ShellOutputStream, chunk: &[u8]) {
        self(invocation, stream, chunk);
    }
}

#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
static NEXT_INVOCATION_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
const ACTIVE: u8 = 0;
#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
const FINISHED: u8 = 1;
#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
const CANCELLED: u8 = 2;

#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
struct Remaining {
    stdout_remaining: usize,
    stderr_remaining: usize,
}

#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
struct Session {
    phase: AtomicU8,
    remaining: Mutex<Remaining>,
    sender: mpsc::Sender<Dispatch>,
}

#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
enum Dispatch {
    Output(ShellOutputStream, Vec<u8>),
    Finish,
    Cancel,
}

/// Invocation owner. Dropping it cancels delivery without waiting for arbitrary
/// observer code already running on the presentation thread.
#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
pub(crate) struct OutputGuard {
    session: Option<Arc<Session>>,
}

#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
impl OutputGuard {
    /// Preserve queued chunks on an ordinary completed invocation. Delivery is
    /// asynchronous: this never waits for observer code.
    pub(crate) fn finish(mut self) {
        if let Some(session) = self.session.take() {
            if session
                .phase
                .compare_exchange(ACTIVE, FINISHED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _ = session.sender.send(Dispatch::Finish);
            }
        }
    }
}

#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
impl Drop for OutputGuard {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            session.phase.store(CANCELLED, Ordering::Release);
            let _ = session.sender.send(Dispatch::Cancel);
        }
    }
}

/// Cloneable handle carried into pipe-draining and confinement threads.
#[derive(Clone, Default)]
#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
pub(crate) struct OutputEmitter {
    session: Option<Arc<Session>>,
    /// The worker-side result channel, when this emitter is running inside the
    /// Brush worker. A plain sink callback rather than a concrete stream type:
    /// the emitter's job is to *fan out* a chunk, and it must not become a
    /// second place where lifecycle or bounding decisions are made.
    #[cfg(feature = "brush")]
    execution: Option<WorkerChannelSink>,
}

/// The worker-side result-channel sink an [`OutputEmitter`] fans chunks into.
#[cfg(feature = "brush")]
pub(crate) type WorkerChannelSink = Arc<dyn Fn(ShellOutputStream, &[u8]) + Send + Sync>;

#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
impl OutputEmitter {
    /// Stop presentation when the managed owner reaches its timeout. The async
    /// invocation's guard still owns cancellation when its waiter is dropped.
    #[cfg(feature = "brush")]
    pub(crate) fn cancel(&self) {
        if let Some(session) = &self.session {
            session.phase.store(CANCELLED, Ordering::Release);
            let _ = session.sender.send(Dispatch::Cancel);
        }
    }

    pub(crate) fn emit(&self, stream: ShellOutputStream, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }

        // The worker's framed result channel is the AUTHORITY for what this run
        // produced. The observer session below is presentation only, and its
        // own byte budget never governs what the result channel carries.
        #[cfg(feature = "brush")]
        if let Some(execution) = &self.execution {
            execution(stream, chunk);
        }

        let Some(session) = &self.session else {
            return;
        };
        if session.phase.load(Ordering::Acquire) != ACTIVE {
            return;
        }

        // This lock protects only byte accounting. Arbitrary observer code runs
        // on the dispatcher thread and is never called while this lock is held.
        let mut remaining = session
            .remaining
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if session.phase.load(Ordering::Acquire) != ACTIVE {
            return;
        }
        let stream_remaining = match stream {
            ShellOutputStream::Stdout => &mut remaining.stdout_remaining,
            ShellOutputStream::Stderr => &mut remaining.stderr_remaining,
        };
        let len = chunk.len().min(*stream_remaining);
        if len == 0 {
            return;
        }
        *stream_remaining -= len;
        let retained = chunk[..len].to_vec();
        drop(remaining);

        // `send` on an unbounded std channel is non-blocking. Total queued byte
        // memory remains bounded by the per-stream accounting above.
        if session.phase.load(Ordering::Acquire) == ACTIVE {
            let _ = session.sender.send(Dispatch::Output(stream, retained));
        }
    }

    /// An emitter that feeds only the worker's framed result channel.
    #[cfg(feature = "brush")]
    pub(crate) fn to_worker_channel(sink: WorkerChannelSink) -> Self {
        Self {
            session: None,
            execution: Some(sink),
        }
    }
}

#[cfg(any(feature = "shell", feature = "host-shell", feature = "brush"))]
pub(crate) fn output_session(
    observer: Option<Arc<dyn ShellOutputObserver>>,
    max_output: usize,
) -> (OutputGuard, OutputEmitter) {
    let session = observer.and_then(|observer| {
        let invocation = ShellInvocationId(NEXT_INVOCATION_ID.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = mpsc::channel();
        let session = Arc::new(Session {
            phase: AtomicU8::new(ACTIVE),
            remaining: Mutex::new(Remaining {
                stdout_remaining: max_output,
                stderr_remaining: max_output,
            }),
            sender,
        });
        let dispatcher_session = Arc::clone(&session);
        std::thread::Builder::new()
            .name(format!("agent-bridle-output-{}", invocation.get()))
            .spawn(move || {
                while let Ok(dispatch) = receiver.recv() {
                    match dispatch {
                        Dispatch::Output(stream, chunk) => {
                            if dispatcher_session.phase.load(Ordering::Acquire) == CANCELLED {
                                break;
                            }
                            if catch_unwind(AssertUnwindSafe(|| {
                                observer.on_output(invocation, stream, &chunk);
                            }))
                            .is_err()
                            {
                                dispatcher_session.phase.store(CANCELLED, Ordering::Release);
                                break;
                            }
                        }
                        Dispatch::Finish => {
                            let _ = catch_unwind(AssertUnwindSafe(|| {
                                observer.on_finish(invocation);
                            }));
                            break;
                        }
                        Dispatch::Cancel => break,
                    }
                }
            })
            .ok()
            .map(|_| session)
    });
    (
        OutputGuard {
            session: session.clone(),
        },
        OutputEmitter {
            session,
            #[cfg(feature = "brush")]
            execution: None,
        },
    )
}

/// Drain a stream to EOF while retaining and observing at most `max_output`
/// bytes. Bytes beyond the cap are discarded, preserving the child process's
/// ordinary pipe behavior without allowing captured memory to grow unbounded.
#[cfg(any(feature = "host-shell", feature = "brush"))]
pub(crate) fn drain_capped(
    mut reader: impl Read,
    max_output: usize,
    output: &OutputEmitter,
    stream: ShellOutputStream,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::with_capacity(max_output.min(8 * 1024));
    let mut chunk = [0u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if n == 0 {
            return Ok((buf, truncated));
        }
        // The emitter applies its own finite observer/channel budget. Feed it
        // every byte so the worker channel can count omissions beyond that
        // budget, even after this local capture has filled.
        output.emit(stream, &chunk[..n]);
        let retained = (max_output - buf.len()).min(n);
        if retained > 0 {
            buf.extend_from_slice(&chunk[..retained]);
        }
        truncated |= retained < n;
    }
}

#[cfg(all(
    test,
    any(feature = "shell", feature = "host-shell", feature = "brush")
))]
mod tests {
    use super::*;
    #[cfg(any(feature = "host-shell", feature = "brush"))]
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::time::Duration;

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Output(ShellInvocationId, ShellOutputStream, Vec<u8>),
        Finish(ShellInvocationId),
    }

    struct ChannelObserver(mpsc::Sender<Event>);

    impl ShellOutputObserver for ChannelObserver {
        fn on_output(
            &self,
            invocation: ShellInvocationId,
            stream: ShellOutputStream,
            chunk: &[u8],
        ) {
            self.0
                .send(Event::Output(invocation, stream, chunk.to_vec()))
                .expect("test receives output");
        }

        fn on_finish(&self, invocation: ShellInvocationId) {
            self.0
                .send(Event::Finish(invocation))
                .expect("test receives finish");
        }
    }

    struct BlockingInvocationObserver {
        events: mpsc::Sender<Event>,
        blocked: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl ShellOutputObserver for BlockingInvocationObserver {
        fn on_output(
            &self,
            invocation: ShellInvocationId,
            stream: ShellOutputStream,
            chunk: &[u8],
        ) {
            self.events
                .send(Event::Output(invocation, stream, chunk.to_vec()))
                .expect("record concurrent output");
            if chunk == b"blocked" {
                self.blocked.send(()).expect("record blocked callback");
                self.release
                    .lock()
                    .expect("release lock")
                    .recv()
                    .expect("test releases callback");
            }
        }

        fn on_finish(&self, invocation: ShellInvocationId) {
            self.events
                .send(Event::Finish(invocation))
                .expect("record concurrent finish");
        }
    }

    #[test]
    fn finish_follows_capped_streams_for_the_same_invocation() {
        let (events_tx, events_rx) = mpsc::channel();
        let observer = Arc::new(ChannelObserver(events_tx));
        let (guard, emitter) = output_session(Some(observer), 3);

        emitter.emit(ShellOutputStream::Stdout, b"abcd");
        emitter.emit(ShellOutputStream::Stderr, b"wxyz");
        guard.finish();
        emitter.emit(ShellOutputStream::Stdout, b"late");

        let first = events_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("stdout event");
        let invocation = match first {
            Event::Output(id, ShellOutputStream::Stdout, bytes) => {
                assert_eq!(bytes, b"abc");
                id
            }
            other => panic!("unexpected first event: {other:?}"),
        };
        assert_eq!(
            events_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("stderr event"),
            Event::Output(invocation, ShellOutputStream::Stderr, b"wxy".to_vec())
        );
        assert_eq!(
            events_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("finish event"),
            Event::Finish(invocation),
            "finish is delivered only after every queued output callback"
        );
        assert!(
            events_rx.try_recv().is_err(),
            "post-finish output is ignored"
        );
    }

    #[test]
    fn panic_disables_only_the_observer() {
        let (calls_tx, calls_rx) = mpsc::channel();
        let observer = Arc::new(move |_invocation, _stream, _chunk: &[u8]| {
            calls_tx.send(()).expect("record observer call");
            panic!("observer panic");
        });
        let (_guard, emitter) = output_session(Some(observer), 10);

        emitter.emit(ShellOutputStream::Stdout, b"first");
        emitter.emit(ShellOutputStream::Stdout, b"second");

        calls_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first callback");
        assert!(
            calls_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "the queued second callback is discarded after a panic"
        );
    }

    #[test]
    fn blocked_invocation_does_not_stall_another_invocation() {
        let (events_tx, events_rx) = mpsc::channel();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let observer: Arc<dyn ShellOutputObserver> = Arc::new(BlockingInvocationObserver {
            events: events_tx,
            blocked: blocked_tx,
            release: Mutex::new(release_rx),
        });
        let (guard_a, emitter_a) = output_session(Some(observer.clone()), 16);
        let (guard_b, emitter_b) = output_session(Some(observer), 16);

        emitter_a.emit(ShellOutputStream::Stdout, b"blocked");
        blocked_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("invocation A entered its blocking callback");
        let invocation_a = match events_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("invocation A output")
        {
            Event::Output(id, ShellOutputStream::Stdout, bytes) => {
                assert_eq!(bytes, b"blocked");
                id
            }
            other => panic!("unexpected invocation A event: {other:?}"),
        };

        emitter_b.emit(ShellOutputStream::Stdout, b"progress");
        guard_b.finish();
        let invocation_b = match events_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("invocation B progresses while A is blocked")
        {
            Event::Output(id, ShellOutputStream::Stdout, bytes) => {
                assert_eq!(bytes, b"progress");
                id
            }
            other => panic!("unexpected invocation B event: {other:?}"),
        };
        assert_ne!(invocation_a, invocation_b);
        assert_eq!(
            events_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("invocation B finish"),
            Event::Finish(invocation_b)
        );

        guard_a.finish();
        release_tx.send(()).expect("release invocation A");
        assert_eq!(
            events_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("invocation A finish"),
            Event::Finish(invocation_a)
        );
    }

    #[test]
    #[cfg(any(feature = "host-shell", feature = "brush"))]
    fn capped_drain_observes_the_prefix_but_consumes_to_eof() {
        let (seen_tx, seen_rx) = mpsc::channel();
        let observer = Arc::new(move |_invocation, _stream, chunk: &[u8]| {
            seen_tx.send(chunk.to_vec()).expect("record observed bytes");
        });
        let (guard, emitter) = output_session(Some(observer), 3);
        let mut reader = Cursor::new(b"abcdef".to_vec());

        let (captured, truncated) =
            drain_capped(&mut reader, 3, &emitter, ShellOutputStream::Stdout).expect("drain");

        assert_eq!(captured, b"abc");
        assert!(truncated);
        guard.finish();
        assert_eq!(
            seen_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("observed prefix"),
            b"abc"
        );
        assert_eq!(reader.position(), 6, "excess bytes are drained, not stored");
    }

    #[test]
    #[cfg(any(feature = "host-shell", feature = "brush"))]
    fn capped_drain_retries_an_interrupted_read() {
        struct InterruptedOnce {
            interrupted: bool,
            inner: Cursor<Vec<u8>>,
        }

        impl Read for InterruptedOnce {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
                }
                self.inner.read(buf)
            }
        }

        let reader = InterruptedOnce {
            interrupted: false,
            inner: Cursor::new(b"abcdef".to_vec()),
        };
        let (captured, truncated) = drain_capped(
            reader,
            4,
            &OutputEmitter::default(),
            ShellOutputStream::Stdout,
        )
        .expect("interrupted reads are retried");

        assert_eq!(captured, b"abcd");
        assert!(truncated);
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12

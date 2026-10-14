use super::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

// Deterministic idle-bound tests (#2875). The clock is fake and advances only
// inside the fixture generate, so no test waits on the idle bound itself. The
// bound is idle time since the last checkpoint, not total elapsed time.
const IDLE_BOUND: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(super) struct FakeClock {
    base: Instant,
    elapsed: Arc<std::sync::Mutex<Duration>>,
}

impl FakeClock {
    pub(super) fn new() -> Self {
        Self {
            base: Instant::now(),
            elapsed: Arc::new(std::sync::Mutex::new(Duration::ZERO)),
        }
    }

    pub(super) fn now(&self) -> Instant {
        self.base + *self.elapsed.lock().unwrap()
    }

    pub(super) fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
    }
}

#[tokio::test]
async fn idle_bound_lets_a_streaming_generation_outlive_the_bound() {
    let clock = FakeClock::new();
    let reader = clock.clone();
    let stepper = clock.clone();
    let result = run_generation_with_clock(
        Arc::new(tokio::sync::Semaphore::new(1)),
        Some(IDLE_BOUND),
        move || reader.now(),
        move |checkpoint| {
            // Ten checkpoints, each 0.6x the bound apart: 6x the bound in total.
            for _ in 0..10 {
                stepper.advance(IDLE_BOUND.mul_f64(0.6));
                checkpoint()?;
            }
            Ok("streamed".into())
        },
    )
    .await;
    assert_eq!(result.unwrap(), "streamed");
}

#[tokio::test]
async fn idle_bound_fails_a_generation_silent_past_the_bound() {
    let clock = FakeClock::new();
    let reader = clock.clone();
    let stepper = clock.clone();
    let result = run_generation_with_clock(
        Arc::new(tokio::sync::Semaphore::new(1)),
        Some(IDLE_BOUND),
        move || reader.now(),
        move |_checkpoint| {
            // Two bounds of silence with no checkpoint.
            stepper.advance(IDLE_BOUND * 2);
            Ok("silent".into())
        },
    )
    .await;
    assert!(result.is_err(), "silence past the bound must fail");
}

/// Regression/mutation coverage for #2875 r2 round 3, finding 4: the async poll must sample the
/// clock BEFORE it reads the progress mark. The injected clock is the hook.
/// Call 1 is `start`. On the poll's first sample (call 2, on the test thread)
/// the worker is released through a std channel and the poll waits for it to
/// checkpoint at fake 6 s and 12 s, then reads 13 s. Sampled first, idle time is
/// 13 - 12 = 1 s and the run succeeds. Read progress first (the old order) and
/// the poll sees `last = 0`, idle 13 s, and fails a generation that just
/// progressed. Worker-thread reads return the current fake time without
/// counting, so only the poll's samples advance the script.
///
/// Real time, on purpose. The first poll costs one real 50 ms `IDLE_POLL` tick.
/// Pausing the clock was tried and hangs: the poll thread blocks inside the
/// injected clock until the blocking worker checkpoints, so tokio's paused-time
/// auto-advance and the worker's std-channel handshake wait on each other.
#[tokio::test]
async fn idle_poll_samples_the_clock_before_the_progress_mark() {
    let clock = FakeClock::new();
    let test_thread = std::thread::current().id();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let done_rx = std::sync::Mutex::new(done_rx);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reader = clock.clone();
    let now = move || {
        if std::thread::current().id() != test_thread {
            return reader.now();
        }
        if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == 2 {
            release_tx.send(()).unwrap();
            done_rx.lock().unwrap().recv().unwrap();
            return reader.now() + Duration::from_secs(1);
        }
        reader.now()
    };
    let stepper = clock.clone();
    let result = run_generation_with_clock(
        Arc::new(tokio::sync::Semaphore::new(1)),
        Some(IDLE_BOUND),
        now,
        move |checkpoint| {
            release_rx.recv().unwrap();
            stepper.advance(Duration::from_secs(6));
            checkpoint()?;
            stepper.advance(Duration::from_secs(6));
            checkpoint()?;
            done_tx.send(()).unwrap();
            Ok("progressed".into())
        },
    )
    .await;
    assert_eq!(result.unwrap(), "progressed");
}

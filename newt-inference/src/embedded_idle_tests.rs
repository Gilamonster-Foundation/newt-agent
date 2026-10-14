use super::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

// Deterministic idle-bound tests (#2875). The clock is fake and advances only
// inside the fixture generate, so nothing sleeps. The bound is idle time since
// the last checkpoint, not total elapsed time.
const IDLE_BOUND: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct FakeClock {
    base: Instant,
    elapsed: Arc<std::sync::Mutex<Duration>>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            elapsed: Arc::new(std::sync::Mutex::new(Duration::ZERO)),
        }
    }

    fn now(&self) -> Instant {
        self.base + *self.elapsed.lock().unwrap()
    }

    fn advance(&self, by: Duration) {
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

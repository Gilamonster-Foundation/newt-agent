//! Shared HTTP retry/backoff for inference backends.
//!
//! Both [`LocalOllamaBackend`](crate::local::LocalOllamaBackend) and
//! [`LocalVllmBackend`](crate::local::LocalVllmBackend) drive their single
//! `try_complete` attempt through [`with_backoff`], so the retry policy and
//! the error-classification rules live in exactly one place.
//!
//! ## Why this exists
//!
//! Hosted OpenAI-compatible endpoints (e.g. NVIDIA's inference API) fail
//! *intermittently* — transient connection resets and `429 Too Many Requests`
//! under load. The previous per-backend loops only retried connection errors
//! and `5xx`, so a `429` surfaced as a hard error, and the fixed
//! `[250, 500, 1000]ms` schedule gave up after ~1.75s. [`RetryPolicy`] widens
//! both: `408`/`429` are retryable, and the backoff is true exponential with a
//! configurable ceiling and jitter.

use std::future::Future;
use std::time::Duration;

/// Whether a failed attempt is worth retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryability {
    /// Transient — retrying may succeed (connection failure, timeout, `408`,
    /// `429`, or any `5xx`).
    Retry,
    /// Permanent for this request — retrying will not help (other `4xx`, or a
    /// malformed success body).
    Fatal,
}

/// Classify a backend error by its message.
///
/// Backends produce two error shapes that this inspects:
/// - transport/timeout: `"<backend> request failed: <source>"`
/// - HTTP status:        `"<backend> returned <code>: <body>"`
///
/// Anything else (e.g. a JSON decode error on a `200`) is [`Retryability::Fatal`].
pub fn classify(err: &anyhow::Error) -> Retryability {
    let msg = err.to_string();

    // Transport-level failure from reqwest (connection refused, reset, DNS,
    // or a client timeout). Always worth another attempt.
    if msg.contains("request failed") {
        return Retryability::Retry;
    }

    // HTTP status surfaced as "<backend> returned <code>: <body>".
    if let Some(code) = status_code_in(&msg) {
        if is_retryable_status(code) {
            return Retryability::Retry;
        }
        return Retryability::Fatal;
    }

    Retryability::Fatal
}

/// `408 Request Timeout`, `429 Too Many Requests`, and every `5xx` are
/// retryable; all other statuses are not.
fn is_retryable_status(code: u16) -> bool {
    code == 408 || code == 429 || (500..600).contains(&code)
}

/// Pull the first status code out of a `"... returned <code>: ..."` message.
fn status_code_in(msg: &str) -> Option<u16> {
    let after = msg.split_once("returned ")?.1;
    // The code is the leading run of ASCII digits (the `StatusCode` Display is
    // e.g. "503 Service Unavailable", so digits come first).
    after
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

/// Exponential-backoff retry policy.
///
/// `delay(attempt) = min(max, base * 2^(attempt-1))`, optionally spread with
/// equal jitter (half fixed, half random) to avoid synchronized retries when
/// several agents hammer the same hosted endpoint.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Number of retries *after* the first attempt. `0` disables retrying.
    pub max_retries: u32,
    /// Base delay before the first retry.
    pub base: Duration,
    /// Ceiling for any single backoff delay.
    pub max: Duration,
    /// Whether to apply equal jitter to each delay.
    pub jitter: bool,
}

impl Default for RetryPolicy {
    /// Production default: 4 retries (5 attempts total), 500 ms base doubling
    /// to an 8 s ceiling, with jitter — roughly 15 s of resilience against a
    /// flaky hosted endpoint.
    fn default() -> Self {
        Self {
            max_retries: 4,
            base: Duration::from_millis(500),
            max: Duration::from_secs(8),
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// Build from the environment, falling back to [`RetryPolicy::default`]
    /// for any unset/invalid var:
    /// - `NEWT_HTTP_MAX_RETRIES` — retry count after the first attempt
    /// - `NEWT_HTTP_BACKOFF_BASE_MS` — base delay in milliseconds
    /// - `NEWT_HTTP_BACKOFF_MAX_MS` — ceiling delay in milliseconds
    /// - `NEWT_HTTP_JITTER` — `0`/`false`/`off` disables jitter
    pub fn from_env() -> Self {
        let mut p = Self::default();
        if let Some(n) = env_parse::<u32>("NEWT_HTTP_MAX_RETRIES") {
            p.max_retries = n;
        }
        if let Some(ms) = env_parse::<u64>("NEWT_HTTP_BACKOFF_BASE_MS") {
            p.base = Duration::from_millis(ms);
        }
        if let Some(ms) = env_parse::<u64>("NEWT_HTTP_BACKOFF_MAX_MS") {
            p.max = Duration::from_millis(ms);
        }
        if let Ok(v) = std::env::var("NEWT_HTTP_JITTER") {
            p.jitter = !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off"
            );
        }
        p
    }

    /// A zero-delay policy with `max_retries` retries — for tests that want to
    /// exercise the retry loop without sleeping.
    pub fn immediate(max_retries: u32) -> Self {
        Self {
            max_retries,
            base: Duration::ZERO,
            max: Duration::ZERO,
            jitter: false,
        }
    }

    /// Delay before the `attempt`-th retry (`attempt` is 1-based: `1` is the
    /// first retry). Deterministic when `jitter` is false.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let capped_ms = self.base_delay_ms(attempt);
        if !self.jitter || capped_ms == 0 {
            return Duration::from_millis(capped_ms);
        }
        // Equal jitter: keep half fixed, randomize the other half. This never
        // collapses to ~0 (unlike full jitter), so backoff still grows.
        let half = capped_ms / 2;
        let span = capped_ms - half; // == ceil(capped/2)
        Duration::from_millis(half + jitter_u64() % (span + 1))
    }

    /// The deterministic (pre-jitter) capped exponential delay in ms.
    fn base_delay_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.saturating_sub(1).min(31);
        let factor = 1u64 << shift;
        let raw = (self.base.as_millis() as u64).saturating_mul(factor);
        raw.min(self.max.as_millis() as u64)
    }
}

/// Drive a fallible async operation under `policy`.
///
/// Calls `op` until it succeeds, the error is [`Retryability::Fatal`], or
/// `policy.max_retries` is exhausted — sleeping `policy.delay_for(attempt)`
/// between attempts. On exhaustion the *last* error is returned (so the caller
/// still sees e.g. the final `503`).
pub async fn with_backoff<T, F, Fut>(policy: &RetryPolicy, mut op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut retries = 0u32;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if classify(&err) == Retryability::Fatal || retries >= policy.max_retries {
                    return Err(err);
                }
                retries += 1;
                let delay = policy.delay_for(retries);
                tracing::warn!(
                    attempt = retries,
                    delay_ms = delay.as_millis() as u64,
                    error = %err,
                    "retrying inference request"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Parse an environment variable, returning `None` if unset or unparseable.
fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// Cheap, dependency-free jitter source. Not cryptographic — it only needs to
/// de-correlate retry timing across attempts and processes. Seeded once from
/// the wall clock (entropy only, never used as a coordination primitive),
/// then advanced by a SplitMix64 step + xorshift per call.
fn jitter_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    // One-time seed from the clock's sub-second noise. Concurrent first-callers
    // may both seed; that's harmless — any nonzero start works.
    if STATE.load(Ordering::Relaxed) == 0 {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0)
            ^ 0x9E37_79B9_7F4A_7C15;
        STATE.store(seed | 1, Ordering::Relaxed);
    }
    let mut x = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn err(msg: &str) -> anyhow::Error {
        anyhow::anyhow!("{msg}")
    }

    #[test]
    fn classify_transport_failure_is_retry() {
        assert_eq!(
            classify(&err("vLLM request failed: error sending request for url")),
            Retryability::Retry
        );
        assert_eq!(
            classify(&err("Ollama request failed: connection refused")),
            Retryability::Retry
        );
    }

    #[test]
    fn classify_429_and_408_and_5xx_are_retry() {
        // The core regression: 429 must be retryable.
        assert_eq!(
            classify(&err("vLLM returned 429 Too Many Requests: slow down")),
            Retryability::Retry
        );
        assert_eq!(
            classify(&err("vLLM returned 408 Request Timeout:")),
            Retryability::Retry
        );
        assert_eq!(
            classify(&err("Ollama returned 503 Service Unavailable: down")),
            Retryability::Retry
        );
        assert_eq!(
            classify(&err("vLLM returned 500 Internal Server Error: boom")),
            Retryability::Retry
        );
    }

    #[test]
    fn classify_other_4xx_is_fatal() {
        assert_eq!(
            classify(&err("vLLM returned 400 Bad Request: nope")),
            Retryability::Fatal
        );
        assert_eq!(
            classify(&err("vLLM returned 404 Not Found:")),
            Retryability::Fatal
        );
    }

    #[test]
    fn classify_unknown_shape_is_fatal() {
        assert_eq!(
            classify(&err("error decoding response body: expected value")),
            Retryability::Fatal
        );
    }

    #[test]
    fn status_code_parsing() {
        assert_eq!(
            status_code_in("vLLM returned 503 Service Unavailable: x"),
            Some(503)
        );
        assert_eq!(status_code_in("Ollama returned 429: y"), Some(429));
        assert_eq!(status_code_in("no status here"), None);
    }

    #[test]
    fn base_delay_is_exponential_and_capped() {
        let p = RetryPolicy {
            max_retries: 10,
            base: Duration::from_millis(500),
            max: Duration::from_secs(8),
            jitter: false,
        };
        assert_eq!(p.base_delay_ms(1), 500);
        assert_eq!(p.base_delay_ms(2), 1000);
        assert_eq!(p.base_delay_ms(3), 2000);
        assert_eq!(p.base_delay_ms(4), 4000);
        assert_eq!(p.base_delay_ms(5), 8000);
        // Capped at max thereafter.
        assert_eq!(p.base_delay_ms(6), 8000);
        assert_eq!(p.base_delay_ms(30), 8000);
    }

    #[test]
    fn delay_without_jitter_is_deterministic() {
        let p = RetryPolicy {
            jitter: false,
            ..RetryPolicy::default()
        };
        assert_eq!(p.delay_for(1), Duration::from_millis(500));
        assert_eq!(p.delay_for(2), Duration::from_millis(1000));
    }

    #[test]
    fn jittered_delay_stays_within_equal_jitter_band() {
        let p = RetryPolicy {
            max_retries: 4,
            base: Duration::from_millis(1000),
            max: Duration::from_secs(8),
            jitter: true,
        };
        // attempt 1 → capped 1000ms → band [500, 1000].
        for _ in 0..200 {
            let ms = p.delay_for(1).as_millis() as u64;
            assert!((500..=1000).contains(&ms), "delay {ms} outside [500,1000]");
        }
    }

    #[test]
    fn immediate_policy_never_sleeps() {
        let p = RetryPolicy::immediate(3);
        assert_eq!(p.max_retries, 3);
        assert_eq!(p.delay_for(1), Duration::ZERO);
        assert_eq!(p.delay_for(3), Duration::ZERO);
    }

    #[tokio::test]
    async fn with_backoff_succeeds_after_transient_failures() {
        let calls = Cell::new(0u32);
        let result: anyhow::Result<&str> = with_backoff(&RetryPolicy::immediate(5), || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                if n < 3 {
                    Err(err("vLLM returned 429 Too Many Requests: slow"))
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.get(), 3, "should have retried twice then succeeded");
    }

    #[tokio::test]
    async fn with_backoff_stops_on_fatal() {
        let calls = Cell::new(0u32);
        let result: anyhow::Result<&str> = with_backoff(&RetryPolicy::immediate(5), || {
            calls.set(calls.get() + 1);
            async move { Err(err("vLLM returned 400 Bad Request: nope")) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 1, "fatal error must not retry");
    }

    #[tokio::test]
    async fn with_backoff_gives_up_after_max_retries() {
        let calls = Cell::new(0u32);
        let result: anyhow::Result<&str> = with_backoff(&RetryPolicy::immediate(3), || {
            calls.set(calls.get() + 1);
            async move { Err(err("vLLM returned 503 Service Unavailable: down")) }
        })
        .await;
        let e = result.unwrap_err();
        assert!(e.to_string().contains("503"), "last error preserved: {e}");
        assert_eq!(calls.get(), 4, "1 initial + 3 retries");
    }
}

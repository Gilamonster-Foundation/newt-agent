//! Shared HTTP retry/backoff for inference backends.
//!
//! Lived in `newt-inference` until Step 9.7 moved it here so the relocated
//! agentic loop ([`crate::agentic`]) can share it without creating a
//! `newt-inference` ⇄ `newt-core` dependency cycle; `newt_inference::retry`
//! re-exports this module, so all existing import paths still work.
//!
//! Both `LocalOllamaBackend` and `LocalVllmBackend` (in `newt-inference`)
//! drive their single `try_complete` attempt through [`with_backoff`], so the
//! retry policy and the error-classification rules live in exactly one place.
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

/// Pull the first status code out of an error message.
///
/// Recognises two formats produced by the backends in this workspace:
/// - `"<backend> returned <code> …"` — local backends (Ollama, vLLM)
/// - `"inference endpoint <code> …"` — hosted OpenAI-compatible endpoints
///   (NVIDIA inference API, LiteLLM proxies, etc.)
///
/// The code is the leading run of ASCII digits after the matched prefix
/// (e.g. `StatusCode` Display is `"503 Service Unavailable"`, digits first).
fn status_code_in(msg: &str) -> Option<u16> {
    const PREFIXES: &[&str] = &["returned ", "inference endpoint "];
    for prefix in PREFIXES {
        if let Some(after) = msg.split_once(prefix).map(|(_, r)| r) {
            if let Some(code) = after
                .split(|c: char| !c.is_ascii_digit())
                .find(|s: &&str| !s.is_empty())
                .and_then(|s| s.parse().ok())
            {
                return Some(code);
            }
        }
    }
    None
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
    /// Build from environment variables, falling back to [`RetryPolicy::default`].
    ///
    /// Variables (all optional, unset/invalid values are silently ignored):
    /// - `NEWT_HTTP_MAX_RETRIES` — retry count after the first attempt
    /// - `NEWT_HTTP_BACKOFF_BASE_MS` — base delay in milliseconds
    /// - `NEWT_HTTP_BACKOFF_MAX_MS` — ceiling delay in milliseconds
    /// - `NEWT_HTTP_JITTER` — `0`/`false`/`off` disables jitter
    pub fn from_env() -> Self {
        Self::from_env_or(Self::default())
    }

    /// Like [`from_env`](Self::from_env) but starts from `base` instead of
    /// [`Default`]. Use this when a caller has different default thresholds
    /// from the crate default but still wants the env vars to override them.
    pub fn from_env_or(mut base: Self) -> Self {
        if let Some(n) = env_parse::<u32>("NEWT_HTTP_MAX_RETRIES") {
            base.max_retries = n;
        }
        if let Some(ms) = env_parse::<u64>("NEWT_HTTP_BACKOFF_BASE_MS") {
            base.base = Duration::from_millis(ms);
        }
        if let Some(ms) = env_parse::<u64>("NEWT_HTTP_BACKOFF_MAX_MS") {
            base.max = Duration::from_millis(ms);
        }
        if let Ok(v) = std::env::var("NEWT_HTTP_JITTER") {
            base.jitter = !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off"
            );
        }
        base
    }

    /// A policy tuned for slow, home-lab local inference endpoints (e.g. a DGX
    /// that can drop for 30–60 s under load). More patient than the default
    /// hosted-API policy:
    /// - 6 retries (7 attempts total)
    /// - 2 s base doubling to a 30 s ceiling, with jitter
    /// - Total resilience window: ~90 s
    ///
    /// Env-var overrides (`NEWT_HTTP_MAX_RETRIES` etc.) still apply.
    pub fn for_local_inference() -> Self {
        Self::from_env_or(Self {
            max_retries: 6,
            base: Duration::from_secs(2),
            max: Duration::from_secs(30),
            jitter: true,
        })
    }

    /// A bounded policy for paid / hosted inference endpoints.
    ///
    /// A hosted request can consume the full inference deadline before it
    /// fails. Reusing the seven-attempt home-lab policy therefore turns a
    /// 120-second timeout into roughly fifteen minutes of duplicate requests.
    /// One retry still absorbs a transient edge failure without creating that
    /// retry storm. Environment overrides remain authoritative.
    pub fn for_hosted_inference() -> Self {
        Self::from_env_or(Self::hosted_inference_defaults())
    }

    /// Pure hosted defaults, split from environment resolution so tests never
    /// depend on or race an operator's legitimate `NEWT_HTTP_*` overrides.
    fn hosted_inference_defaults() -> Self {
        Self {
            max_retries: 1,
            base: Duration::from_millis(500),
            max: Duration::from_secs(8),
            jitter: true,
        }
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

/// Like [`with_backoff_notify`], but also supplies the failed attempt's error
/// to the callback so callers can render typed diagnostics.
pub async fn with_backoff_notify_error<T, F, Fut, N>(
    policy: &RetryPolicy,
    mut op: F,
    mut on_retry: N,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
    N: FnMut(u32, Duration, &anyhow::Error),
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
                on_retry(retries, delay, &err);
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

/// Drive a fallible async operation under `policy`, calling `on_retry` before
/// each sleep so the caller can log or display a retry indicator.
///
/// `on_retry(attempt, delay)` is called synchronously before sleeping:
/// `attempt` is 1-based (1 = first retry), `delay` is the sleep duration.
///
/// Calls `op` until it succeeds, the error is [`Retryability::Fatal`], or
/// `policy.max_retries` is exhausted. On exhaustion the *last* error is
/// returned.
pub async fn with_backoff_notify<T, F, Fut, N>(
    policy: &RetryPolicy,
    op: F,
    mut on_retry: N,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
    N: FnMut(u32, Duration),
{
    with_backoff_notify_error(policy, op, |attempt, delay, _| {
        on_retry(attempt, delay);
    })
    .await
}

/// Drive a fallible async operation under `policy`.
///
/// Convenience wrapper around [`with_backoff_notify`] with a no-op callback.
/// Calls `op` until it succeeds, the error is [`Retryability::Fatal`], or
/// `policy.max_retries` is exhausted — sleeping `policy.delay_for(attempt)`
/// between attempts. On exhaustion the *last* error is returned (so the caller
/// still sees e.g. the final `503`).
pub async fn with_backoff<T, F, Fut>(policy: &RetryPolicy, op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    with_backoff_notify(policy, op, |_, _| {}).await
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
    fn classify_inference_endpoint_5xx_is_retry() {
        // Regression: hosted endpoints format errors as "inference endpoint <code> …"
        // which previously fell through status_code_in and was classified Fatal.
        assert_eq!(
            classify(&err(
                r#"inference endpoint 500 Internal Server Error: {"error":{"message":"instance_id not found"}}"#
            )),
            Retryability::Retry
        );
        assert_eq!(
            classify(&err("inference endpoint 503 Service Unavailable: down")),
            Retryability::Retry
        );
        assert_eq!(
            classify(&err("inference endpoint 429 Too Many Requests: slow")),
            Retryability::Retry
        );
    }

    #[test]
    fn status_code_parsing_inference_endpoint_format() {
        assert_eq!(
            status_code_in("inference endpoint 500 Internal Server Error: body"),
            Some(500)
        );
        assert_eq!(
            status_code_in("inference endpoint 429 Too Many Requests: body"),
            Some(429)
        );
        assert_eq!(
            status_code_in("inference endpoint 400 Bad Request: body"),
            Some(400)
        );
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

    #[tokio::test]
    async fn with_backoff_notify_fires_callback_before_each_sleep() {
        let calls = Cell::new(0u32);
        let notified = Cell::new(0u32);
        let result: anyhow::Result<&str> = with_backoff_notify(
            &RetryPolicy::immediate(3),
            || {
                let n = calls.get() + 1;
                calls.set(n);
                async move {
                    if n <= 2 {
                        Err(err("vLLM returned 503 Service Unavailable: down"))
                    } else {
                        Ok("ok")
                    }
                }
            },
            |_, _| {
                notified.set(notified.get() + 1);
            },
        )
        .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.get(), 3, "two retries then success");
        assert_eq!(
            notified.get(),
            2,
            "callback fired once before each retry sleep"
        );
    }

    #[tokio::test]
    async fn error_aware_retry_callback_receives_the_failed_attempt() {
        let calls = Cell::new(0u32);
        let saw_error = Cell::new(false);
        let result: anyhow::Result<&str> = with_backoff_notify_error(
            &RetryPolicy::immediate(1),
            || {
                let n = calls.get() + 1;
                calls.set(n);
                async move {
                    if n == 1 {
                        Err(err("vLLM returned 503 sentinel"))
                    } else {
                        Ok("ok")
                    }
                }
            },
            |_, _, error| {
                saw_error.set(error.to_string().contains("sentinel"));
            },
        )
        .await;
        assert_eq!(result.unwrap(), "ok");
        assert!(saw_error.get(), "callback receives the typed retry error");
    }

    #[test]
    fn for_local_inference_is_more_patient_than_default() {
        let local = RetryPolicy::for_local_inference();
        let default = RetryPolicy::default();
        assert!(
            local.max_retries > default.max_retries,
            "local policy must allow more retries"
        );
        assert!(
            local.max > default.max,
            "local policy must have a longer backoff ceiling"
        );
    }

    #[test]
    fn hosted_inference_defaults_retry_only_once() {
        let hosted = RetryPolicy::hosted_inference_defaults();
        assert_eq!(
            hosted.max_retries, 1,
            "a hosted 120-second deadline must not expand into a seven-attempt retry storm"
        );
        assert_eq!(hosted.base, Duration::from_millis(500));
        assert_eq!(hosted.max, Duration::from_secs(8));
    }

    #[test]
    fn from_env_or_starts_from_provided_base() {
        // No env vars set — result must equal the base.
        let base = RetryPolicy {
            max_retries: 9,
            base: Duration::from_secs(3),
            max: Duration::from_secs(60),
            jitter: false,
        };
        let result = RetryPolicy::from_env_or(base.clone());
        assert_eq!(result.max_retries, 9);
        assert_eq!(result.base, Duration::from_secs(3));
        assert_eq!(result.max, Duration::from_secs(60));
        assert!(!result.jitter);
    }
}

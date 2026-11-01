//! #2909 step 4: the spend poll.
//!
//! A deadline-bounded `GET /api/v1/key` adapter (source name
//! `"openrouter-key"`) plus a per-backend spend cache and a pure,
//! clock-injected 5-minute cadence policy. Every failure — timeout, HTTP
//! error, 401, parse failure, unknown source — degrades to
//! [`SpendStatus::Unknown`], never to an error that reaches a turn.
//!
//! These are library APIs: nothing calls them yet. Running the poll off the
//! turn path is the caller's job (step 5).
//!
//! The three pieces live together because they are one coherent behavior:
//! an adapter that reads a backend's spend from its declared budget source,
//! a cache keyed by backend, and a cadence policy that decides when a poll
//! is due.

use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Production deadline for a single spend poll — a few seconds so a hung
/// `GET /api/v1/key` cannot hold its caller. Callers pass it in; tests inject
/// their own.
pub const POLL_DEADLINE: Duration = Duration::from_secs(5);

/// Cadence: never poll more than once per this window (5 minutes).
pub const SPEND_CADENCE: Duration = Duration::from_secs(5 * 60);

/// The measured spend against a backend's declared budget, or `Unknown`.
///
/// That is what the footer (step 5) renders as "$49.80 left".
#[derive(Debug, Clone, PartialEq)]
pub enum SpendStatus {
    /// The poll succeeded — provider-reported numbers.
    Known {
        remaining_usd: f64,
        limit_usd: f64,
        resets_at: Option<String>,
    },
    /// Could not be read. Honest "unknown" — never a fabricated number.
    Unknown,
}

impl SpendStatus {
    /// True when the meter has a real, provider-reported reading.
    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }
}

/// OpenRouter `GET /api/v1/key` response shape. Only the fields we read are
/// modeled; unknown fields are ignored by serde.
#[derive(Deserialize, Default)]
struct OpenRouterKeyResponse {
    data: Option<KeyData>,
    effective_budget: Option<EffectiveBudget>,
}

#[derive(Deserialize, Default)]
struct KeyData {
    #[serde(default)]
    limit_remaining: Option<f64>,
    #[serde(default)]
    limit: Option<f64>,
    #[serde(default)]
    limit_reset: Option<String>,
}

#[derive(Deserialize, Default)]
struct EffectiveBudget {
    #[serde(default)]
    remaining_usd: Option<f64>,
    #[serde(default)]
    limit_usd: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
}

/// Poll OpenRouter `GET /api/v1/key` for this backend's spend, using the
/// backend's already-resolved API key, bounded by `deadline`.
///
/// Never returns an error: timeout, connect failure, HTTP error (including
/// 401), or parse failure all degrade to [`SpendStatus::Unknown`].
pub async fn poll_openrouter_key(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    deadline: Duration,
) -> SpendStatus {
    let url = format!("{}/v1/key", endpoint.trim_end_matches('/'));
    read_key_response(client.get(&url).bearer_auth(api_key).send(), deadline).await
}

/// Read a `/v1/key` response from `send` within `deadline`. `send` is the
/// transport seam: production passes reqwest's `send()`, tests a controlled
/// future, so the deadline tests run on paused time with no socket.
async fn read_key_response(
    send: impl Future<Output = reqwest::Result<reqwest::Response>>,
    deadline: Duration,
) -> SpendStatus {
    let response = match tokio::time::timeout(deadline, send).await {
        Ok(Ok(response)) => response,
        // Timeout, connect failure, DNS, TLS — all mean unknown.
        Ok(Err(_)) | Err(_) => return SpendStatus::Unknown,
    };
    if !response.status().is_success() {
        // 401, 4xx, 5xx — unknown, never an error that reaches a turn.
        return SpendStatus::Unknown;
    }
    let body = match response.text().await {
        Ok(body) => body,
        Err(_) => return SpendStatus::Unknown,
    };
    let parsed: OpenRouterKeyResponse = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(_) => return SpendStatus::Unknown,
    };
    let remaining = parsed
        .effective_budget
        .as_ref()
        .and_then(|b| b.remaining_usd)
        .or_else(|| parsed.data.as_ref().and_then(|d| d.limit_remaining));
    let limit = parsed
        .effective_budget
        .as_ref()
        .and_then(|b| b.limit_usd)
        .or_else(|| parsed.data.as_ref().and_then(|d| d.limit));
    let resets_at = parsed
        .effective_budget
        .as_ref()
        .and_then(|b| b.resets_at.as_deref())
        .or_else(|| parsed.data.as_ref().and_then(|d| d.limit_reset.as_deref()))
        .map(String::from);
    match (remaining, limit) {
        (Some(remaining), Some(limit)) => SpendStatus::Known {
            remaining_usd: remaining,
            limit_usd: limit,
            resets_at,
        },
        _ => SpendStatus::Unknown,
    }
}

/// Dispatch a backend's declared budget source to the right adapter.
///
/// An unknown source name means `Unknown` — the source name is data, kept
/// even when no adapter understands it yet; the adapter that understands it
/// simply reports "unknown" until it exists. So does a backend that resolves
/// no key (`api_key: None`).
pub async fn poll_budget_source(
    source: &str,
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
    deadline: Duration,
) -> SpendStatus {
    match (source, api_key) {
        ("openrouter-key", Some(key)) => poll_openrouter_key(client, endpoint, key, deadline).await,
        _ => SpendStatus::Unknown,
    }
}

/// One cached spend reading for a backend.
struct CacheEntry {
    last_polled: Instant,
    status: SpendStatus,
}

/// Per-backend spend cache, keyed by backend name.
#[derive(Default)]
pub struct SpendCache {
    entries: HashMap<String, CacheEntry>,
}

impl SpendCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// The last recorded spend for `backend`, if a poll has ever been recorded.
    pub fn cached(&self, backend: &str) -> Option<&SpendStatus> {
        self.entries.get(backend).map(|e| &e.status)
    }

    /// Record a fresh poll result for `backend` at `now`.
    pub fn record(&mut self, backend: String, now: Instant, status: SpendStatus) {
        self.entries.insert(
            backend,
            CacheEntry {
                last_polled: now,
                status,
            },
        );
    }

    /// Whether a poll is due for `backend` at `now`, per the cadence policy:
    /// poll at first selection, then no more than every 5 minutes, and never
    /// while a turn is in flight. Pure — inject `now`; no wall clock here.
    pub fn is_poll_due(&self, now: Instant, backend: &str, turn_in_flight: bool) -> bool {
        if turn_in_flight {
            return false;
        }
        match self.entries.get(backend) {
            None => true, // poll at backend selection
            Some(entry) => now.duration_since(entry.last_polled) >= SPEND_CADENCE,
        }
    }

    /// Run `poll` for `backend` if the cadence allows, caching whatever it
    /// returns — `Unknown` included, so a failing source is retried no faster
    /// than a working one.
    ///
    /// Returns the current (cached or freshly polled) spend status. The caller
    /// supplies the poll, normally
    /// `|| poll_budget_source(source, client, endpoint, key, POLL_DEADLINE)`.
    pub async fn poll_if_due<F: Future<Output = SpendStatus>>(
        &mut self,
        backend: &str,
        now: Instant,
        turn_in_flight: bool,
        poll: impl FnOnce() -> F,
    ) -> SpendStatus {
        if !self.is_poll_due(now, backend, turn_in_flight) {
            return self
                .cached(backend)
                .cloned()
                .unwrap_or(SpendStatus::Unknown);
        }
        let status = poll().await;
        self.record(backend.to_string(), now, status.clone());
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// For tests that are not about the deadline: no timer can fire, so a
    /// loopback mock never races one.
    const NO_DEADLINE: Duration = Duration::MAX;

    fn json_body(remaining: f64, limit: f64, resets_at: &str) -> serde_json::Value {
        serde_json::json!({
            "effective_budget": {
                "remaining_usd": remaining,
                "limit_usd": limit,
                "spend_usd": limit - remaining,
                "resets_at": resets_at,
            }
        })
    }

    /// A response built in memory: headers are already here, no socket.
    fn response(status: u16, body: impl Into<reqwest::Body>) -> reqwest::Response {
        http::Response::builder()
            .status(status)
            .body(body.into())
            .unwrap()
            .into()
    }

    /// The parsed spend of an in-memory response, under no deadline.
    async fn read(status: u16, body: impl Into<reqwest::Body>) -> SpendStatus {
        let response = response(status, body);
        read_key_response(async { Ok(response) }, NO_DEADLINE).await
    }

    fn known() -> SpendStatus {
        SpendStatus::Known {
            remaining_usd: 49.8,
            limit_usd: 50.0,
            resets_at: None,
        }
    }

    /// A poll operation that counts its calls — each call is one request.
    fn counted(
        calls: &Cell<u32>,
        status: SpendStatus,
    ) -> impl FnOnce() -> std::future::Ready<SpendStatus> + '_ {
        move || {
            calls.set(calls.get() + 1);
            std::future::ready(status)
        }
    }

    /// The real client path: `GET <endpoint>/v1/key` with the bearer key.
    #[tokio::test]
    async fn success_parses_known_spend() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/key"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json_body(
                49.80,
                50.0,
                "2026-11-01T00:00:00.000Z",
            )))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let status = poll_openrouter_key(&client, &server.uri(), "test-key", NO_DEADLINE).await;
        assert!(
            matches!(status, SpendStatus::Known { remaining_usd, limit_usd, .. } if (remaining_usd - 49.80).abs() < 1e-6 && (limit_usd - 50.0).abs() < 1e-6),
            "{status:?}"
        );
    }

    #[tokio::test]
    async fn unauthorized_gives_unknown() {
        assert_eq!(read(401, "").await, SpendStatus::Unknown);
    }

    #[tokio::test]
    async fn malformed_json_gives_unknown() {
        assert_eq!(read(200, "not json {{{").await, SpendStatus::Unknown);
    }

    #[tokio::test]
    async fn unknown_source_or_missing_key_gives_unknown() {
        let client = reqwest::Client::new();
        let endpoint = "http://unused.invalid";
        for (source, key) in [
            ("no-such-source", Some("test-key")),
            ("openrouter-key", None),
        ] {
            let status = poll_budget_source(source, &client, endpoint, key, NO_DEADLINE).await;
            assert_eq!(status, SpendStatus::Unknown, "{source} {key:?}");
        }
    }

    #[tokio::test]
    async fn data_shape_fields_are_read() {
        let body = serde_json::json!({
            "data": {
                "limit_remaining": 12.5,
                "limit": 100.0,
                "limit_reset": "monthly"
            }
        });
        let status = read(200, body.to_string()).await;
        assert!(
            matches!(status, SpendStatus::Known { remaining_usd, limit_usd, ref resets_at } if (remaining_usd - 12.5).abs() < 1e-6 && (limit_usd - 100.0).abs() < 1e-6 && resets_at.as_deref() == Some("monthly")),
            "{status:?}"
        );
    }

    /// Review P1: the headers arrive at once, then the body stalls after one
    /// byte. The deadline covers the body too, so the poll ends at it. The
    /// outer 60x guard turns a hang into a failure.
    #[tokio::test(start_paused = true)]
    async fn stalled_body_gives_unknown_at_the_deadline() {
        use futures_util::{stream, StreamExt};
        let deadline = Duration::from_secs(3);
        let stalls = stream::iter([Ok::<_, std::io::Error>("{")]).chain(stream::pending());
        let response = response(200, reqwest::Body::wrap_stream(stalls));
        let started = tokio::time::Instant::now();
        let read = read_key_response(async { Ok(response) }, deadline);
        let status = tokio::time::timeout(60 * deadline, read).await;
        assert_eq!(
            (status, started.elapsed()),
            (Ok(SpendStatus::Unknown), deadline)
        );
    }

    /// Review P1: a valid key record padded to 1 MiB is not read.
    #[tokio::test]
    async fn oversized_body_gives_unknown() {
        let mut body = json_body(49.8, 50.0, "2026-11-01T00:00:00.000Z");
        body["padding"] = "x".repeat(1 << 20).into();
        assert_eq!(read(200, body.to_string()).await, SpendStatus::Unknown);
    }

    /// A response that lands 1 ms inside the deadline is read, on paused time.
    #[tokio::test(start_paused = true)]
    async fn response_inside_the_deadline_is_read() {
        let deadline = Duration::from_secs(3);
        let body = json_body(49.8, 50.0, "2026-11-01T00:00:00.000Z").to_string();
        let send = async {
            tokio::time::sleep(deadline - Duration::from_millis(1)).await;
            Ok(response(200, body))
        };
        assert!(read_key_response(send, deadline).await.is_known());
    }

    /// The deadline is the one injected through the poll operation (3 s, not
    /// the production 5 s), it fires on paused time, and the timeout is cached
    /// like any other result.
    #[tokio::test(start_paused = true)]
    async fn poll_if_due_caches_a_timeout_at_the_injected_deadline() {
        let deadline = Duration::from_secs(3);
        let mut cache = SpendCache::new();
        let now = Instant::now();
        let started = tokio::time::Instant::now();
        let status = cache
            .poll_if_due("openrouter", now, false, || {
                read_key_response(std::future::pending(), deadline)
            })
            .await;
        assert_eq!(
            (status, started.elapsed()),
            (SpendStatus::Unknown, deadline)
        );
        assert_eq!(cache.cached("openrouter"), Some(&SpendStatus::Unknown));
        assert!(!cache.is_poll_due(now, "openrouter", false));
    }

    /// A success is served from cache 1 ns under the cadence, and re-polled at
    /// exactly the cadence.
    #[tokio::test]
    async fn cadence_boundaries_after_a_success() {
        let calls = Cell::new(0);
        let mut cache = SpendCache::new();
        let t0 = Instant::now();
        let first = cache.poll_if_due("openrouter", t0, false, counted(&calls, known()));
        assert_eq!(first.await, known());
        let just_under = t0 + SPEND_CADENCE - Duration::from_nanos(1);
        let cached = cache.poll_if_due(
            "openrouter",
            just_under,
            false,
            counted(&calls, SpendStatus::Unknown),
        );
        assert_eq!((cached.await, calls.get()), (known(), 1));
        let at = cache.poll_if_due(
            "openrouter",
            t0 + SPEND_CADENCE,
            false,
            counted(&calls, SpendStatus::Unknown),
        );
        assert_eq!((at.await, calls.get()), (SpendStatus::Unknown, 2));
    }

    /// A failed poll is cached too: no retry until the cadence elapses.
    #[tokio::test]
    async fn failure_is_cached_until_the_cadence() {
        let calls = Cell::new(0);
        let mut cache = SpendCache::new();
        let t0 = Instant::now();
        let failed = cache.poll_if_due(
            "openrouter",
            t0,
            false,
            counted(&calls, SpendStatus::Unknown),
        );
        assert_eq!(failed.await, SpendStatus::Unknown);
        let cached = cache.poll_if_due(
            "openrouter",
            t0 + Duration::from_secs(1),
            false,
            counted(&calls, known()),
        );
        assert_eq!((cached.await, calls.get()), (SpendStatus::Unknown, 1));
        let retried = cache.poll_if_due(
            "openrouter",
            t0 + SPEND_CADENCE,
            false,
            counted(&calls, known()),
        );
        assert_eq!((retried.await, calls.get()), (known(), 2));
    }

    /// Mid-turn, no request is made — neither for a never-polled backend nor
    /// for an overdue one.
    #[tokio::test]
    async fn mid_turn_call_makes_no_request() {
        let calls = Cell::new(0);
        let mut cache = SpendCache::new();
        let t0 = Instant::now();
        let unseen = cache.poll_if_due("openrouter", t0, true, counted(&calls, known()));
        assert_eq!(unseen.await, SpendStatus::Unknown);
        cache.record("openrouter".to_string(), t0, known());
        let overdue = cache.poll_if_due(
            "openrouter",
            t0 + 2 * SPEND_CADENCE,
            true,
            counted(&calls, SpendStatus::Unknown),
        );
        assert_eq!((overdue.await, calls.get()), (known(), 0));
    }

    #[test]
    fn cadence_keyed_by_backend() {
        let mut cache = SpendCache::new();
        let now = Instant::now();
        cache.record("a".to_string(), now, SpendStatus::Unknown);
        assert!(!cache.is_poll_due(now, "a", false));
        assert!(cache.is_poll_due(now, "b", false));
    }
}

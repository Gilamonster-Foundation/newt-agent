//! #2909 step 4: the spend poll.
//!
//! A detached, deadline-bounded `GET /api/v1/key` adapter (source name
//! `"openrouter-key"`) plus a per-backend spend cache and a pure,
//! clock-injected 5-minute cadence policy. Never blocks a turn: every
//! failure — timeout, HTTP error, 401, parse failure, unknown source —
//! degrades to [`SpendStatus::Unknown`], never to an error that reaches a
//! turn.
//!
//! The three pieces live together because they are one coherent behavior:
//! an adapter that reads a backend's spend from its declared budget source,
//! a cache keyed by backend, and a cadence policy that decides when a poll
//! is due.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Overall deadline for a single spend poll — a few seconds so a hung
/// `GET /api/v1/key` can never block a turn.
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
/// backend's already-resolved API key.
///
/// Never returns an error: timeout, connect failure, HTTP error (including
/// 401), or parse failure all degrade to [`SpendStatus::Unknown`].
pub async fn poll_openrouter_key(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
) -> SpendStatus {
    let url = format!("{}/v1/key", endpoint.trim_end_matches('/'));
    let response =
        match tokio::time::timeout(POLL_DEADLINE, client.get(&url).bearer_auth(api_key).send())
            .await
        {
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
/// simply reports "unknown" until it exists.
pub async fn poll_budget_source(
    source: &str,
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
) -> SpendStatus {
    match source {
        "openrouter-key" => poll_openrouter_key(client, endpoint, api_key).await,
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

    /// Poll the spend for `backend` if the cadence allows, caching the result.
    ///
    /// Returns the current (cached or freshly polled) spend status. `api_key` is
    /// `None` when the backend resolves no key — that degrades to `Unknown`.
    #[allow(clippy::too_many_arguments)]
    pub async fn poll_if_due(
        &mut self,
        client: &reqwest::Client,
        backend: &str,
        source: &str,
        endpoint: &str,
        api_key: Option<&str>,
        now: Instant,
        turn_in_flight: bool,
    ) -> SpendStatus {
        if !self.is_poll_due(now, backend, turn_in_flight) {
            return self
                .cached(backend)
                .cloned()
                .unwrap_or(SpendStatus::Unknown);
        }
        let status = match api_key {
            Some(key) => poll_budget_source(source, client, endpoint, key).await,
            None => SpendStatus::Unknown,
        };
        self.record(backend.to_string(), now, status.clone());
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn server() -> MockServer {
        MockServer::start().await
    }

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

    #[tokio::test]
    async fn success_parses_known_spend() {
        let server = server().await;
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
        let status = poll_openrouter_key(&client, &server.uri(), "test-key").await;
        assert!(
            matches!(status, SpendStatus::Known { remaining_usd, limit_usd, .. } if (remaining_usd - 49.80).abs() < 1e-6 && (limit_usd - 50.0).abs() < 1e-6),
            "{status:?}"
        );
    }

    #[tokio::test]
    async fn timeout_gives_unknown() {
        let server = server().await;
        // The mock never responds; the 5s deadline fires.
        Mock::given(method("GET"))
            .and(path("/v1/key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json_body(49.80, 50.0, "2026-11-01T00:00:00.000Z"))
                    .set_delay(std::time::Duration::from_secs(30)),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let status = poll_openrouter_key(&client, &server.uri(), "test-key").await;
        assert_eq!(status, SpendStatus::Unknown);
    }

    #[tokio::test]
    async fn unauthorized_gives_unknown() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/v1/key"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let status = poll_openrouter_key(&client, &server.uri(), "wrong-key").await;
        assert_eq!(status, SpendStatus::Unknown);
    }

    #[tokio::test]
    async fn malformed_json_gives_unknown() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/v1/key"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json {{{"))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let status = poll_openrouter_key(&client, &server.uri(), "test-key").await;
        assert_eq!(status, SpendStatus::Unknown);
    }

    #[tokio::test]
    async fn unknown_source_gives_unknown() {
        let server = server().await;
        let client = reqwest::Client::new();
        let status = poll_budget_source("no-such-source", &client, &server.uri(), "test-key").await;
        assert_eq!(status, SpendStatus::Unknown);
    }

    #[tokio::test]
    async fn data_shape_fields_are_read() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/v1/key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "limit_remaining": 12.5,
                    "limit": 100.0,
                    "limit_reset": "monthly"
                }
            })))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let status = poll_openrouter_key(&client, &server.uri(), "test-key").await;
        assert!(
            matches!(status, SpendStatus::Known { remaining_usd, limit_usd, ref resets_at } if (remaining_usd - 12.5).abs() < 1e-6 && (limit_usd - 100.0).abs() < 1e-6 && resets_at.as_deref() == Some("monthly")),
            "{status:?}"
        );
    }

    #[test]
    fn cadence_polls_at_selection() {
        let cache = SpendCache::new();
        let now = Instant::now();
        assert!(cache.is_poll_due(now, "openrouter", false));
    }

    #[test]
    fn cadence_under_five_minutes_does_not_poll() {
        let mut cache = SpendCache::new();
        let now = Instant::now();
        cache.record("openrouter".to_string(), now, SpendStatus::Unknown);
        assert!(!cache.is_poll_due(now, "openrouter", false));
    }

    #[test]
    fn cadence_over_five_minutes_polls() {
        let mut cache = SpendCache::new();
        let now = Instant::now();
        cache.record(
            "openrouter".to_string(),
            now - SPEND_CADENCE - Duration::from_secs(1),
            SpendStatus::Unknown,
        );
        assert!(cache.is_poll_due(now, "openrouter", false));
    }

    #[test]
    fn cadence_never_mid_turn() {
        let mut cache = SpendCache::new();
        let now = Instant::now();
        // Never polled but a turn is in flight -> not due.
        assert!(!cache.is_poll_due(now, "openrouter", true));
        // Polled long ago but a turn is in flight -> not due.
        cache.record(
            "openrouter".to_string(),
            now - Duration::from_secs(600),
            SpendStatus::Unknown,
        );
        assert!(!cache.is_poll_due(now, "openrouter", true));
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

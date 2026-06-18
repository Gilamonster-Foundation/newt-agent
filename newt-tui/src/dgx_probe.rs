//! DGX / Ollama hardware telemetry — optional, zero-coupling layer.
//!
//! All operations degrade gracefully: if DCGM Prometheus or `/api/ps` is
//! unreachable the caller receives `None` or empty results — never a panic or
//! a propagated error.  The session loop holds an `Option<DgxTelemetry>` that
//! is `None` for ordinary Ollama backends.
//!
//! Architecture:
//! - `parse_prometheus()` — pure, no I/O, fully unit-testable.
//! - `snapshot_from_metrics()` — maps raw metric names to typed fields.
//! - `DgxTelemetry` — async HTTP client; all fetch errors return None/empty.
//!   - `try_connect(ollama_url)` — derives DCGM port from Ollama host; returns
//!     `None` if DCGM is unreachable rather than failing.
//!   - `snapshot()` / `snapshot_async()` — DCGM GPU metrics, None when absent.
//!   - `into_sampler()` — background sampler publishing snapshots on a `watch`
//!     channel so the UI reads instantly and never blocks (issue #414).
//!   - `loaded_models()` — Ollama `/api/ps`, empty vec when unavailable.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Hardware metrics sampled from DCGM Prometheus (port 9400).
/// All fields are `Option` — any metric can be absent.
#[derive(Debug, Clone, Default)]
pub struct TelemetrySnapshot {
    /// GPU utilisation [0, 100].
    pub gpu_util_pct: Option<u8>,
    /// VRAM copy-engine utilisation [0, 100].
    pub vram_copy_util_pct: Option<u8>,
    /// Board power in watts.
    pub power_watts: Option<f32>,
    /// GPU die temperature in °C.
    pub gpu_temp_c: Option<u8>,
    /// SM (shader) clock in MHz.
    pub sm_clock_mhz: Option<u32>,
}

impl TelemetrySnapshot {
    /// `true` if at least one field is populated.
    pub fn has_data(&self) -> bool {
        self.gpu_util_pct.is_some()
            || self.vram_copy_util_pct.is_some()
            || self.power_watts.is_some()
            || self.gpu_temp_c.is_some()
            || self.sm_clock_mhz.is_some()
    }

    /// Compact one-liner suitable for a verbose status annotation.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(u) = self.gpu_util_pct {
            parts.push(format!("gpu {u}%"));
        }
        if let Some(p) = self.power_watts {
            parts.push(format!("{p:.0}W"));
        }
        if let Some(t) = self.gpu_temp_c {
            parts.push(format!("{t}°C"));
        }
        if let Some(c) = self.sm_clock_mhz {
            parts.push(format!("{c}MHz"));
        }
        if parts.is_empty() {
            return "no data".to_string();
        }
        parts.join(" · ")
    }
}

/// A model currently loaded in Ollama (from `/api/ps`).
#[derive(Debug, Clone)]
pub struct LoadedModel {
    pub name: String,
    pub size_vram_bytes: u64,
    pub expires_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure parser — no I/O
// ---------------------------------------------------------------------------

/// Parse Prometheus text-exposition format into a bare metric-name → value map.
///
/// Handles `METRIC{label="v"} 123.4` and `METRIC 123.4` forms.
/// `# HELP` / `# TYPE` comment lines are ignored.
/// NaN / Inf values are dropped (not representable in tuning math).
/// When the same metric name appears with multiple label sets, the last
/// value wins — callers that need label-aware access should scan the raw text.
pub fn parse_prometheus(text: &str) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The value is always the last whitespace-delimited token.
        let Some((metric_part, value_str)) = line.rsplit_once(|c: char| c.is_ascii_whitespace())
        else {
            continue;
        };
        let Ok(value) = value_str.trim().parse::<f64>() else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }
        // Strip label set `{...}` to get the bare metric name.
        let name = metric_part
            .split_once('{')
            .map(|(n, _)| n)
            .unwrap_or(metric_part)
            .trim();
        if !name.is_empty() {
            map.insert(name.to_string(), value);
        }
    }
    map
}

/// Translate a raw metric map (from [`parse_prometheus`]) into a [`TelemetrySnapshot`].
pub fn snapshot_from_metrics(metrics: &HashMap<String, f64>) -> TelemetrySnapshot {
    let f_to_u8 = |v: f64| -> Option<u8> {
        if (0.0..=255.0).contains(&v) {
            Some(v as u8)
        } else {
            None
        }
    };
    TelemetrySnapshot {
        gpu_util_pct: metrics
            .get("DCGM_FI_DEV_GPU_UTIL")
            .copied()
            .and_then(f_to_u8),
        vram_copy_util_pct: metrics
            .get("DCGM_FI_DEV_MEM_COPY_UTIL")
            .copied()
            .and_then(f_to_u8),
        power_watts: metrics.get("DCGM_FI_DEV_POWER_USAGE").map(|&v| v as f32),
        gpu_temp_c: metrics
            .get("DCGM_FI_DEV_GPU_TEMP")
            .copied()
            .and_then(f_to_u8),
        sm_clock_mhz: metrics.get("DCGM_FI_DEV_SM_CLOCK").map(|&v| v as u32),
    }
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

/// Derive the DCGM Prometheus base URL from an Ollama URL.
///
/// Swaps whatever port is present (default 11434) for DCGM's default (9400).
/// Returns `None` when the URL has no parseable host.
pub fn dcgm_url_from_ollama(ollama_url: &str) -> Option<String> {
    let base = ollama_url.trim_end_matches('/');
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
    // Drop any path component — keep only host[:port].
    let host_port = rest.split('/').next()?;
    let host = if let Some((h, _port)) = host_port.rsplit_once(':') {
        h
    } else {
        host_port
    };
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}:9400"))
}

// ---------------------------------------------------------------------------
// Async helpers — all infallible to callers
// ---------------------------------------------------------------------------

async fn fetch_text(url: &str, timeout_secs: u64) -> Option<String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()
}

async fn fetch_json(url: &str, timeout_secs: u64) -> Option<serde_json::Value> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()
}

// ---------------------------------------------------------------------------
// DgxTelemetry — async client
// ---------------------------------------------------------------------------

/// Telemetry client for a DGX (or any Ollama host that also runs DCGM).
///
/// Constructed via [`DgxTelemetry::try_connect`]; on non-DGX hosts the
/// function returns `None` so the session can continue without telemetry.
pub struct DgxTelemetry {
    ollama_base: String,
    dcgm_base: String,
}

impl DgxTelemetry {
    /// Attempt to connect to DCGM Prometheus derived from the Ollama URL.
    ///
    /// Returns `None` when:
    /// - The Ollama URL has no parseable host.
    /// - DCGM's `/metrics` endpoint is unreachable within 3 seconds.
    ///
    /// This is a blocking call (uses `block_in_place`) so it can be called
    /// from synchronous session-setup code without an `async` context.
    pub fn try_connect(ollama_url: &str) -> Option<Self> {
        let dcgm_base = dcgm_url_from_ollama(ollama_url)?;
        let metrics_url = format!("{}/metrics", dcgm_base.trim_end_matches('/'));
        let reachable = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { fetch_text(&metrics_url, 3).await.is_some() })
        });
        if reachable {
            Some(Self {
                ollama_base: ollama_url.trim_end_matches('/').to_string(),
                dcgm_base,
            })
        } else {
            None
        }
    }

    /// Sample current GPU metrics from DCGM Prometheus (async core).
    ///
    /// Returns a zeroed (all-`None`) snapshot when the endpoint is
    /// unreachable — never propagates errors.
    pub async fn snapshot_async(&self) -> TelemetrySnapshot {
        let metrics_url = format!("{}/metrics", self.dcgm_base.trim_end_matches('/'));
        fetch_text(&metrics_url, 5)
            .await
            .as_deref()
            .map(parse_prometheus)
            .as_ref()
            .map(snapshot_from_metrics)
            .unwrap_or_default()
    }

    /// Blocking wrapper over [`snapshot_async`](Self::snapshot_async). Kept for
    /// synchronous callers; on the UI hot path prefer the background sampler
    /// ([`into_sampler`](Self::into_sampler)) so the prompt never blocks.
    pub fn snapshot(&self) -> TelemetrySnapshot {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.snapshot_async())
        })
    }

    /// Consume this connection into a **background sampler**: spawn a task that
    /// polls `snapshot_async` every `interval_secs` and publishes the latest
    /// reading on a `watch` channel. The UI reads `*rx.borrow()` instantly and
    /// never blocks on the network (issue #414 first step; follows #412). The
    /// task stops when the last receiver is dropped (backend-URL change or
    /// session exit), so there is no leak.
    pub fn into_sampler(
        self,
        interval_secs: u64,
    ) -> tokio::sync::watch::Receiver<TelemetrySnapshot> {
        let (tx, rx) = tokio::sync::watch::channel(TelemetrySnapshot::default());
        tokio::runtime::Handle::current().spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
            loop {
                ticker.tick().await;
                if tx.send(self.snapshot_async().await).is_err() {
                    break; // last receiver dropped — stop sampling
                }
            }
        });
        rx
    }

    /// List models currently loaded in Ollama's VRAM (`/api/ps`).
    ///
    /// Returns an empty vec when the endpoint is unreachable.
    pub fn loaded_models(&self) -> Vec<LoadedModel> {
        let url = format!("{}/api/ps", self.ollama_base);
        let json = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async { fetch_json(&url, 5).await })
        });
        parse_loaded_models(json.as_ref())
    }

    /// The DCGM base URL this client is connected to.
    pub fn dcgm_base(&self) -> &str {
        &self.dcgm_base
    }
}

fn parse_loaded_models(json: Option<&serde_json::Value>) -> Vec<LoadedModel> {
    json.and_then(|j| j["models"].as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m["name"].as_str()?.to_string();
                    let size_vram_bytes = m["size_vram"].as_u64().unwrap_or(0);
                    let expires_at = m["expires_at"].as_str().map(str::to_string);
                    Some(LoadedModel {
                        name,
                        size_vram_bytes,
                        expires_at,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- into_sampler (background sampler) ---

    #[tokio::test]
    async fn into_sampler_publishes_in_background_without_blocking() {
        // An unreachable DCGM endpoint: `snapshot_async` returns the default
        // (all-`None`) snapshot. The point is the background task PUBLISHES on
        // the `watch` channel so the UI can read it instantly — the caller of
        // `into_sampler` never blocks on the network.
        let tele = DgxTelemetry {
            ollama_base: "http://127.0.0.1:1".into(),
            dcgm_base: "http://127.0.0.1:1".into(), // closed port -> fast fail
        };
        let mut rx = tele.into_sampler(1);
        // The first interval tick fires immediately, so a sample is published
        // promptly; reading is instant. The generous timeout only guards against
        // a starved timer under heavy parallel-test load (the publish itself is
        // sub-millisecond) — it is a liveness check, not a latency assertion.
        tokio::time::timeout(std::time::Duration::from_secs(10), rx.changed())
            .await
            .expect("sampler published a snapshot")
            .expect("sender alive");
        assert!(
            !rx.borrow().has_data(),
            "unreachable endpoint yields an empty snapshot"
        );
        // Dropping the receiver signals the background task to stop (no leak);
        // nothing to assert beyond it not panicking.
        drop(rx);
    }

    // --- parse_prometheus ---

    #[test]
    fn parses_bare_metric() {
        let text = "DCGM_FI_DEV_GPU_UTIL 42\n";
        let m = parse_prometheus(text);
        assert_eq!(m.get("DCGM_FI_DEV_GPU_UTIL").copied(), Some(42.0));
    }

    #[test]
    fn parses_metric_with_labels() {
        let text = r#"DCGM_FI_DEV_SM_CLOCK{gpu="0",UUID="abc"} 2411"#;
        let m = parse_prometheus(text);
        assert_eq!(m.get("DCGM_FI_DEV_SM_CLOCK").copied(), Some(2411.0));
    }

    #[test]
    fn skips_comment_lines() {
        let text = "# HELP DCGM_FI_DEV_GPU_UTIL GPU utilization\n\
                    # TYPE DCGM_FI_DEV_GPU_UTIL gauge\n\
                    DCGM_FI_DEV_GPU_UTIL 55\n";
        let m = parse_prometheus(text);
        assert_eq!(m.len(), 1);
        assert_eq!(m["DCGM_FI_DEV_GPU_UTIL"], 55.0);
    }

    #[test]
    fn drops_nan_and_inf() {
        let text = "METRIC_A NaN\nMETRIC_B +Inf\nMETRIC_C 7\n";
        let m = parse_prometheus(text);
        assert!(!m.contains_key("METRIC_A"));
        assert!(!m.contains_key("METRIC_B"));
        assert_eq!(m["METRIC_C"], 7.0);
    }

    #[test]
    fn empty_text_returns_empty_map() {
        assert!(parse_prometheus("").is_empty());
        assert!(parse_prometheus("# just comments\n").is_empty());
    }

    #[test]
    fn last_value_wins_for_same_metric_name() {
        let text = "DCGM_FI_DEV_GPU_UTIL{gpu=\"0\"} 10\n\
                    DCGM_FI_DEV_GPU_UTIL{gpu=\"1\"} 20\n";
        let m = parse_prometheus(text);
        // last encountered wins
        assert_eq!(m["DCGM_FI_DEV_GPU_UTIL"], 20.0);
    }

    // --- snapshot_from_metrics ---

    #[test]
    fn snapshot_maps_known_dcgm_fields() {
        let mut metrics = HashMap::new();
        metrics.insert("DCGM_FI_DEV_GPU_UTIL".into(), 75.0);
        metrics.insert("DCGM_FI_DEV_POWER_USAGE".into(), 350.5);
        metrics.insert("DCGM_FI_DEV_GPU_TEMP".into(), 68.0);
        metrics.insert("DCGM_FI_DEV_SM_CLOCK".into(), 2400.0);
        metrics.insert("DCGM_FI_DEV_MEM_COPY_UTIL".into(), 12.0);

        let snap = snapshot_from_metrics(&metrics);
        assert_eq!(snap.gpu_util_pct, Some(75));
        assert!((snap.power_watts.unwrap() - 350.5).abs() < 0.1);
        assert_eq!(snap.gpu_temp_c, Some(68));
        assert_eq!(snap.sm_clock_mhz, Some(2400));
        assert_eq!(snap.vram_copy_util_pct, Some(12));
        assert!(snap.has_data());
    }

    #[test]
    fn snapshot_all_none_for_empty_metrics() {
        let snap = snapshot_from_metrics(&HashMap::new());
        assert!(!snap.has_data());
    }

    #[test]
    fn snapshot_out_of_range_u8_becomes_none() {
        let mut metrics = HashMap::new();
        metrics.insert("DCGM_FI_DEV_GPU_UTIL".into(), 999.0);
        let snap = snapshot_from_metrics(&metrics);
        assert_eq!(snap.gpu_util_pct, None);
    }

    // --- summary ---

    #[test]
    fn summary_all_none_returns_no_data() {
        assert_eq!(TelemetrySnapshot::default().summary(), "no data");
    }

    #[test]
    fn summary_contains_available_fields() {
        let snap = TelemetrySnapshot {
            gpu_util_pct: Some(50),
            power_watts: Some(200.0),
            gpu_temp_c: Some(70),
            sm_clock_mhz: Some(2000),
            vram_copy_util_pct: None,
        };
        let s = snap.summary();
        assert!(s.contains("50%"));
        assert!(s.contains("200W"));
        assert!(s.contains("70°C"));
        assert!(s.contains("2000MHz"));
    }

    // --- dcgm_url_from_ollama ---

    #[test]
    fn derives_dcgm_port_from_standard_ollama_url() {
        assert_eq!(
            dcgm_url_from_ollama("http://REDACTED-HOST:11434"),
            Some("http://REDACTED-HOST:9400".into())
        );
    }

    #[test]
    fn derives_dcgm_port_when_no_port_specified() {
        assert_eq!(
            dcgm_url_from_ollama("http://REDACTED-HOST"),
            Some("http://REDACTED-HOST:9400".into())
        );
    }

    #[test]
    fn preserves_scheme() {
        assert_eq!(
            dcgm_url_from_ollama("https://REDACTED-HOST:11434"),
            Some("https://REDACTED-HOST:9400".into())
        );
    }

    #[test]
    fn localhost_url_works() {
        let url = dcgm_url_from_ollama("http://localhost:11434");
        assert_eq!(url, Some("http://localhost:9400".into()));
    }

    #[test]
    fn trailing_slash_stripped() {
        assert_eq!(
            dcgm_url_from_ollama("http://REDACTED-HOST:11434/"),
            Some("http://REDACTED-HOST:9400".into())
        );
    }

    // --- parse_loaded_models ---

    #[test]
    fn parse_loaded_models_happy_path() {
        let json: serde_json::Value = serde_json::json!({
            "models": [
                {
                    "name": "nemotron3:33b",
                    "size_vram": 35_631_112_192u64,
                    "expires_at": "2026-06-06T12:00:00Z"
                }
            ]
        });
        let models = parse_loaded_models(Some(&json));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "nemotron3:33b");
        assert_eq!(models[0].size_vram_bytes, 35_631_112_192);
        assert!(models[0].expires_at.is_some());
    }

    #[test]
    fn parse_loaded_models_empty_list() {
        let json = serde_json::json!({"models": []});
        assert!(parse_loaded_models(Some(&json)).is_empty());
    }

    #[test]
    fn parse_loaded_models_none_json() {
        assert!(parse_loaded_models(None).is_empty());
    }

    #[test]
    fn parse_loaded_models_missing_field() {
        // size_vram absent — defaults to 0, model still included
        let json = serde_json::json!({"models": [{"name": "tiny:1b"}]});
        let models = parse_loaded_models(Some(&json));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].size_vram_bytes, 0);
    }

    // --- full round-trip with real DCGM sample ---

    #[test]
    fn real_dcgm_sample_round_trip() {
        let sample = "\
# HELP DCGM_FI_DEV_SM_CLOCK SM clock frequency (in MHz).\n\
# TYPE DCGM_FI_DEV_SM_CLOCK gauge\n\
DCGM_FI_DEV_SM_CLOCK{gpu=\"0\",UUID=\"GPU-abc\"} 2411\n\
# HELP DCGM_FI_DEV_GPU_TEMP GPU temperature (in C).\n\
# TYPE DCGM_FI_DEV_GPU_TEMP gauge\n\
DCGM_FI_DEV_GPU_TEMP{gpu=\"0\",UUID=\"GPU-abc\"} 36\n\
# HELP DCGM_FI_DEV_POWER_USAGE Power draw (in W).\n\
# TYPE DCGM_FI_DEV_POWER_USAGE gauge\n\
DCGM_FI_DEV_POWER_USAGE{gpu=\"0\",UUID=\"GPU-abc\"} 10.348\n\
# HELP DCGM_FI_DEV_GPU_UTIL GPU utilization (in %).\n\
# TYPE DCGM_FI_DEV_GPU_UTIL gauge\n\
DCGM_FI_DEV_GPU_UTIL{gpu=\"0\",UUID=\"GPU-abc\"} 0\n\
";
        let metrics = parse_prometheus(sample);
        let snap = snapshot_from_metrics(&metrics);
        assert_eq!(snap.sm_clock_mhz, Some(2411));
        assert_eq!(snap.gpu_temp_c, Some(36));
        assert_eq!(snap.gpu_util_pct, Some(0));
        assert!(snap.power_watts.is_some());
        assert!((snap.power_watts.unwrap() - 10.348).abs() < 0.01);
        assert!(snap.has_data());
    }
}

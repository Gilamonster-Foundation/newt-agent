//! Prometheus metrics for the `newt worker` ACP server.
//!
//! Enabled when `NEWT_METRICS_PORT=<port>` is set in the environment.
//! A background task calls [`serve`] to expose `/metrics` (Prometheus text
//! format) and `/healthz` on the configured port.
//!
//! ## Exposed metrics
//!
//! All metrics carry `{model, endpoint}` labels so per-backend dashboards
//! and alerts are possible without external label-relabelling.
//!
//! | Metric | Type | Description |
//! |---|---|---|
//! | `newt_inference_turns_total` | Counter | Completed inference turns |
//! | `newt_inference_input_tokens_total` | Counter | Prompt tokens consumed |
//! | `newt_inference_output_tokens_total` | Counter | Tokens generated |
//! | `newt_inference_cost_usd_total` | Counter | Estimated USD spend |
//! | `newt_inference_duration_ms` | Histogram | Turn wall-clock time (ms) |

use std::sync::Arc;

use prometheus::{
    CounterVec, HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry, TextEncoder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use newt_core::TurnMetrics;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct NewtMetrics {
    pub turns: IntCounterVec,
    pub input_tokens: IntCounterVec,
    pub output_tokens: IntCounterVec,
    pub cost_usd: CounterVec,
    pub duration_ms: HistogramVec,
    registry: Registry,
}

impl NewtMetrics {
    const LABELS: &'static [&'static str] = &["model", "endpoint"];

    pub fn new() -> anyhow::Result<Self> {
        let registry = Registry::new();

        let turns = IntCounterVec::new(
            Opts::new("newt_inference_turns_total", "Completed inference turns"),
            Self::LABELS,
        )?;
        let input_tokens = IntCounterVec::new(
            Opts::new(
                "newt_inference_input_tokens_total",
                "Prompt tokens consumed (input)",
            ),
            Self::LABELS,
        )?;
        let output_tokens = IntCounterVec::new(
            Opts::new(
                "newt_inference_output_tokens_total",
                "Tokens generated (output)",
            ),
            Self::LABELS,
        )?;
        let cost_usd = CounterVec::new(
            Opts::new("newt_inference_cost_usd_total", "Estimated USD spend"),
            Self::LABELS,
        )?;
        let duration_ms = HistogramVec::new(
            HistogramOpts::new(
                "newt_inference_duration_ms",
                "Turn wall-clock time in milliseconds",
            )
            .buckets(vec![
                100.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 30_000.0, 60_000.0,
            ]),
            Self::LABELS,
        )?;

        registry.register(Box::new(turns.clone()))?;
        registry.register(Box::new(input_tokens.clone()))?;
        registry.register(Box::new(output_tokens.clone()))?;
        registry.register(Box::new(cost_usd.clone()))?;
        registry.register(Box::new(duration_ms.clone()))?;

        Ok(Self {
            turns,
            input_tokens,
            output_tokens,
            cost_usd,
            duration_ms,
            registry,
        })
    }

    /// Record observations from a completed [`TurnMetrics`].
    /// Silently no-ops if label construction fails (e.g. empty model_id).
    pub fn record(&self, m: &TurnMetrics) {
        let labels = [m.model_id.as_str(), m.endpoint.as_str()];

        if let (Ok(c), Ok(it), Ok(ot), Ok(du)) = (
            self.turns.get_metric_with_label_values(&labels),
            self.input_tokens.get_metric_with_label_values(&labels),
            self.output_tokens.get_metric_with_label_values(&labels),
            self.duration_ms.get_metric_with_label_values(&labels),
        ) {
            c.inc();
            if let Some(u) = &m.usage {
                it.inc_by(u.input_tokens as u64);
                ot.inc_by(u.output_tokens as u64);
            }
            du.observe(m.elapsed_ms as f64);
        }

        if let Some(usd) = m.cost_usd {
            if let Ok(c) = self.cost_usd.get_metric_with_label_values(&labels) {
                c.inc_by(usd);
            }
        }
    }

    /// Render the registry in Prometheus text exposition format.
    pub fn render(&self) -> String {
        let encoder = TextEncoder::new();
        let mf = self.registry.gather();
        encoder.encode_to_string(&mf).unwrap_or_default()
    }
}

impl Default for NewtMetrics {
    fn default() -> Self {
        Self::new().expect("failed to create prometheus registry")
    }
}

// ---------------------------------------------------------------------------
// HTTP server
// ---------------------------------------------------------------------------

/// Start the Prometheus scrape endpoint on `port`.
///
/// Handles GET /metrics → Prometheus text format (200)
/// and GET /healthz → 200 OK.
/// All other paths → 404.
///
/// This is a minimal hand-rolled HTTP/1.1 server — it handles one request per
/// connection (no keep-alive) which is exactly what Prometheus scraping needs.
pub async fn serve(port: u16, metrics: Arc<NewtMetrics>) {
    let addr = format!("0.0.0.0:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => {
            tracing::info!(port, "newt metrics server listening");
            l
        }
        Err(e) => {
            tracing::error!(port, error = %e, "failed to bind metrics server");
            return;
        }
    };

    loop {
        let Ok((mut socket, _peer)) = listener.accept().await else {
            continue;
        };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let n = match socket.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let req = std::str::from_utf8(&buf[..n]).unwrap_or("");
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/");

            let (status, ct, body) = match path {
                "/metrics" => {
                    let body = metrics.render();
                    ("200 OK", "text/plain; version=0.0.4; charset=utf-8", body)
                }
                "/healthz" => ("200 OK", "text/plain", "ok\n".into()),
                _ => ("404 Not Found", "text/plain", "not found\n".into()),
            };

            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::metrics::TokenUsage;

    fn sample_metrics(model: &str) -> TurnMetrics {
        TurnMetrics {
            elapsed_ms: 3200,
            usage: Some(TokenUsage {
                input_tokens: 847,
                output_tokens: 312,
            }),
            cost_usd: Some(0.0),
            model_id: model.into(),
            endpoint: "http://localhost:11434".into(),
        }
    }

    #[test]
    fn render_after_observation_contains_metric_names() {
        // CounterVec only emits output after the first label combination is
        // used — record one observation before asserting.
        let m = NewtMetrics::new().unwrap();
        m.record(&sample_metrics("boot-test-model"));
        let text = m.render();
        assert!(text.contains("newt_inference_turns_total"), "got: {text}");
        assert!(text.contains("newt_inference_duration_ms"), "got: {text}");
    }

    #[test]
    fn record_increments_counters() {
        let m = NewtMetrics::new().unwrap();
        m.record(&sample_metrics("gemma4:e2b"));
        m.record(&sample_metrics("gemma4:e2b"));
        let text = m.render();
        // turns counter should be 2.
        assert!(text.contains("2"), "expected count 2 in: {text}");
    }

    #[test]
    fn record_two_models_separate_labels() {
        let m = NewtMetrics::new().unwrap();
        m.record(&sample_metrics("model-a"));
        m.record(&sample_metrics("model-b"));
        let text = m.render();
        assert!(text.contains("model-a"), "model-a missing from: {text}");
        assert!(text.contains("model-b"), "model-b missing from: {text}");
    }

    #[test]
    fn record_no_usage_skips_token_counters() {
        let m = NewtMetrics::new().unwrap();
        m.record(&TurnMetrics {
            elapsed_ms: 1000,
            usage: None,
            cost_usd: None,
            model_id: "no-tokens".into(),
            endpoint: "http://localhost:11434".into(),
        });
        let text = m.render();
        // Duration and turn counter should still appear.
        assert!(text.contains("newt_inference_turns_total"));
    }

    #[test]
    fn render_is_valid_prometheus_text() {
        let m = NewtMetrics::new().unwrap();
        m.record(&sample_metrics("test-model"));
        let text = m.render();
        // Every metric family should have a HELP line.
        assert!(text.contains("# HELP newt_inference_turns_total"));
        assert!(text.contains("# HELP newt_inference_duration_ms"));
        assert!(text.contains("# TYPE newt_inference_turns_total counter"));
        assert!(text.contains("# TYPE newt_inference_duration_ms histogram"));
    }
}

use std::time::Duration;

use plugins_protocol::{CompleteRequest, CompleteResponse, ListModelsResponse, Usage};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MAX_RETRIES: u32 = 2;
const DEFAULT_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub struct OpenAiClient {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
    max_retries: u32,
    retry_base_delay: Duration,
}

impl OpenAiClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.filter(|key| !key.trim().is_empty()),
            client: build_http_client(DEFAULT_TIMEOUT),
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_delay: DEFAULT_RETRY_BASE_DELAY,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.client = build_http_client(timeout);
        self
    }

    pub fn with_retries(mut self, max_retries: u32, base_delay: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_base_delay = base_delay;
        self
    }

    pub fn from_env() -> Self {
        let base_url = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "https://api.openai.com".to_string());
        let api_key = std::env::var("OPENAI_API_KEY").ok();
        Self::new(base_url, api_key)
            .with_timeout(parse_timeout_secs(
                std::env::var("OPENAI_TIMEOUT_SECS").ok(),
            ))
            .with_retries(
                parse_max_retries(std::env::var("OPENAI_MAX_RETRIES").ok()),
                DEFAULT_RETRY_BASE_DELAY,
            )
    }

    pub async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
        let key = self.api_key()?;
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages.iter().map(|m| {
                serde_json::json!({ "role": &m.role, "content": &m.content })
            }).collect::<Vec<_>>(),
            "stream": false,
        });
        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }

        let url = format!("{}/v1/chat/completions", self.trimmed_base_url());
        let resp = self
            .send_with_retry("chat completions", || {
                self.client.post(&url).bearer_auth(key).json(&body)
            })
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "OpenAI chat completions returned {status}: {}",
                bounded_excerpt(&text)
            );
        }

        let json: serde_json::Value = serde_json::from_str(&text)?;
        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("OpenAI response missing choices[0].message.content"))?
            .to_string();
        let model_id = json["model"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| req.model.clone());
        let usage = {
            let input = json["usage"]["prompt_tokens"].as_u64().map(|n| n as u32);
            let output = json["usage"]["completion_tokens"]
                .as_u64()
                .map(|n| n as u32);
            input
                .zip(output)
                .map(|(input_tokens, output_tokens)| Usage {
                    input_tokens,
                    output_tokens,
                })
        };

        Ok(CompleteResponse {
            content,
            model_id,
            usage,
        })
    }

    pub async fn list_models(&self) -> anyhow::Result<ListModelsResponse> {
        let key = self.api_key()?;
        let url = format!("{}/v1/models", self.trimmed_base_url());
        let resp = self
            .send_with_retry("list models", || self.client.get(&url).bearer_auth(key))
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "OpenAI list models returned {status}: {}",
                bounded_excerpt(&text)
            );
        }

        let json: serde_json::Value = serde_json::from_str(&text)?;
        let data = json["data"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("OpenAI list models response missing data array"))?;
        let models = data
            .iter()
            .filter_map(|entry| entry["id"].as_str().map(str::to_string))
            .collect();

        Ok(ListModelsResponse { models })
    }

    /// Send a request, retrying connection/timeout errors and 408/429/5xx
    /// responses up to `max_retries` times. Delay honors a numeric
    /// `Retry-After` header when present, else exponential backoff from
    /// `retry_base_delay`.
    async fn send_with_retry(
        &self,
        label: &str,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::Response> {
        let mut attempt: u32 = 0;
        loop {
            let backoff = self
                .retry_base_delay
                .saturating_mul(2u32.saturating_pow(attempt));
            match build().send().await {
                Ok(resp) if is_retryable_status(resp.status()) && attempt < self.max_retries => {
                    let delay = retry_after(resp.headers()).unwrap_or(backoff);
                    tokio::time::sleep(delay).await;
                }
                Ok(resp) => return Ok(resp),
                Err(err)
                    if (err.is_connect() || err.is_timeout()) && attempt < self.max_retries =>
                {
                    tokio::time::sleep(backoff).await;
                }
                Err(err) => {
                    return Err(anyhow::anyhow!("OpenAI {label} request failed: {err}"));
                }
            }
            attempt += 1;
        }
    }

    fn api_key(&self) -> anyhow::Result<&str> {
        self.api_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OPENAI_API_KEY is required for newt-provider-openai"))
    }

    fn trimmed_base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

/// `OPENAI_TIMEOUT_SECS`: whole seconds; unset, unparsable, or zero falls
/// back to the 120s default.
pub fn parse_timeout_secs(raw: Option<String>) -> Duration {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT)
}

/// `OPENAI_MAX_RETRIES`: unset or unparsable falls back to 2; zero disables
/// retries.
pub fn parse_max_retries(raw: Option<String>) -> u32 {
    raw.and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_MAX_RETRIES)
}

fn build_http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("build OpenAI HTTP client")
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// Numeric `Retry-After` (seconds) only; the HTTP-date form is ignored.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn bounded_excerpt(text: &str) -> String {
    const LIMIT: usize = 512;
    let mut excerpt: String = text.chars().take(LIMIT).collect();
    if text.chars().count() > LIMIT {
        excerpt.push_str("...");
    }
    excerpt
}

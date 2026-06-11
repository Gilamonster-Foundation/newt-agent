use std::time::Duration;

use plugins_protocol::{CompleteRequest, CompleteResponse, ListModelsResponse, Usage};

#[derive(Clone)]
pub struct OpenAiClient {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.filter(|key| !key.trim().is_empty()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("build OpenAI HTTP client"),
        }
    }

    pub fn from_env() -> Self {
        let base_url = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "https://api.openai.com".to_string());
        let api_key = std::env::var("OPENAI_API_KEY").ok();
        Self::new(base_url, api_key)
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
            .client
            .post(url)
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("OpenAI chat completions request failed: {e}"))?;

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
            .client
            .get(url)
            .bearer_auth(key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("OpenAI list models request failed: {e}"))?;

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

    fn api_key(&self) -> anyhow::Result<&str> {
        self.api_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OPENAI_API_KEY is required for newt-provider-openai"))
    }

    fn trimmed_base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

fn bounded_excerpt(text: &str) -> String {
    const LIMIT: usize = 512;
    let mut excerpt: String = text.chars().take(LIMIT).collect();
    if text.chars().count() > LIMIT {
        excerpt.push_str("...");
    }
    excerpt
}

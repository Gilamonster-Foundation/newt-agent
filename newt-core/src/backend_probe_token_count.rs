//! Server-side counting of the actual chat template, including tool schemas.
//!
//! llama.cpp's chat counter uses the generation request's own renderer. Older
//! servers expose the renderer and raw tokenizer separately; tokenizing JSON
//! messages directly would not measure the prompt the model receives.

use anyhow::Context;
use serde_json::{json, Value};

/// One server measurement and its exact observed source bytes.
/// Hosts persist these bytes through their existing content-addressed store.
#[derive(Debug, PartialEq, Eq)]
pub struct TokenCount {
    /// Positive count of the fully rendered input.
    pub tokens: usize,
    /// Exact body of the final counting response, before JSON interpretation.
    pub response_bytes: Vec<u8>,
    /// The server protocol used to obtain this measurement.
    pub method: &'static str,
}

struct ProbeResponse {
    json: Value,
    bytes: Vec<u8>,
}

impl ProbeResponse {
    fn measured(self, tokens: usize, method: &'static str) -> TokenCount {
        TokenCount {
            tokens,
            response_bytes: self.bytes,
            method,
        }
    }
}

/// Count one fully assembled OpenAI-compatible chat request on its server.
///
/// The endpoint is the same base used for `/v1/chat/completions`, without a
/// trailing `/v1`. The model and all template-affecting request fields must be
/// finalized before counting; no request or capability result is cached.
///
/// Prefer llama.cpp's direct chat counter, then vLLM's full chat renderer.
/// A raw-only llama `/tokenize` answers a chat probe with an empty token array;
/// then `/apply-template` and raw `/tokenize` provide its count only when
/// `/v1/models` explicitly identifies that model as completion-only. This
/// excludes MTMD's distinct preprocessing, even for text-only messages.
/// No capability or count is cached: older servers need multiple read-only
/// round trips.
/// Unsupported endpoints (404/405/501) or unsupported counting semantics
/// return `None`, allowing the caller's calibrated estimate. Malformed counts
/// and other HTTP failures are errors.
///
/// Legacy vLLM chat `/tokenize` is not an exact generation counter: tool choice,
/// reasoning options, and Harmony preprocessing can differ from generation,
/// even when messages/tools are forwarded intact. Model names may be aliases,
/// so without the full renderer its chat counts retain the calibrated fallback.
///
/// The count covers the input; callers still reserve their output budget.
/// A server can change its model/template or shared KV availability between
/// counting and dispatch. This measurement does not reserve server capacity.
///
/// Upstream protocols:
/// - <https://github.com/ggml-org/llama.cpp/blob/82d6bb284d1ff1c6ef37f29a4c3b63d1a8b11806/tools/server/server-context.cpp>
/// - <https://github.com/vllm-project/vllm/blob/46d2b23ac5047a813ebb082122166e4ae09b5f39/vllm/entrypoints/serve/tokenize/protocol.py>
/// - <https://github.com/vllm-project/vllm/blob/46d2b23ac5047a813ebb082122166e4ae09b5f39/vllm/entrypoints/scale_out/render/serving.py>
pub async fn count_chat_tokens(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    api_key: Option<&str>,
    body: &Value,
) -> anyhow::Result<Option<TokenCount>> {
    anyhow::ensure!(
        body["model"].as_str() == Some(model) && body["messages"].is_array(),
        "token counting requires the final chat request and its matching model"
    );
    // A raw token array does not measure image/audio/embedding positions.
    if !body["prompt_embeds"].is_null()
        || body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"].as_array().is_some_and(|parts| {
                    parts
                        .iter()
                        .any(|part| part["type"].as_str() != Some("text"))
                })
            })
        })
    {
        return Ok(None);
    }
    if let Some(count) = post_count_json(
        client,
        endpoint,
        "/v1/chat/completions/input_tokens",
        api_key,
        body,
    )
    .await?
    {
        let tokens = positive_count(&count.json, "input_tokens")?;
        return Ok(Some(count.measured(tokens, "llama_chat_input_tokens")));
    }

    if let Some(rendered) = post_count_json(
        client,
        endpoint,
        "/v1/chat/completions/render",
        api_key,
        body,
    )
    .await?
    {
        anyhow::ensure!(
            rendered.json["model"].as_str() == Some(model),
            "chat renderer responded for a different model"
        );
        let count = token_count(&rendered.json, "token_ids")?;
        // Multimodal/embedding input length can exceed its token-ID array.
        // This API deliberately measures text requests only.
        return Ok(rendered.json["features"]
            .is_null()
            .then(|| rendered.measured(count, "vllm_chat_render")));
    }

    let Some(tokenized) = post_count_json(client, endpoint, "/tokenize", api_key, body).await?
    else {
        return Ok(None);
    };
    if tokenized.json.get("count").is_some() {
        let count = positive_count(&tokenized.json, "count")?;
        anyhow::ensure!(
            token_count(&tokenized.json, "tokens")? == count,
            "token-count response disagrees with its token array"
        );
        return Ok(None);
    }
    // llama.cpp does not reject a missing `content`: it returns an empty
    // array. That proves only a raw-tokenizer API, never a zero-token prompt.
    anyhow::ensure!(
        tokenized.json["tokens"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "token-count response has no valid chat count"
    );
    let models = request_count_json(
        super::maybe_bearer(
            client.get(format!("{}/v1/models", endpoint.trim_end_matches('/'))),
            api_key,
        ),
        "/v1/models",
    )
    .await?;
    if !models.is_some_and(|models| is_llama_text_model(&models.json, model)) {
        return Ok(None);
    }
    let Some(rendered) =
        post_count_json(client, endpoint, "/apply-template", api_key, body).await?
    else {
        return Ok(None);
    };
    let prompt = rendered.json["prompt"]
        .as_str()
        .filter(|prompt| !prompt.is_empty())
        .context("chat-template response has no nonempty prompt")?;
    let raw = json!({
        "model": model,
        "content": prompt,
        // Match llama.cpp's chat-generation tokenizer, including BOS and
        // recognition of the special tokens emitted by its own template.
        "add_special": true,
        "parse_special": true,
    });
    post_count_json(client, endpoint, "/tokenize", api_key, &raw)
        .await?
        .map(|response| {
            let count = token_count(&response.json, "tokens")?;
            Ok(response.measured(count, "llama_template_tokenize"))
        })
        .transpose()
}

/// The paired llama model cards expose `mctx != nullptr` as the multimodal
/// capability. An alias is accepted only when the server links it to the same
/// canonical model. Unknown, ambiguous, or future capabilities stay heuristic.
fn is_llama_text_model(value: &Value, model: &str) -> bool {
    let (Some(cards), Some(models)) = (value["data"].as_array(), value["models"].as_array()) else {
        return false;
    };
    let mut matching = cards.iter().filter(|card| {
        card["id"].as_str() == Some(model)
            || card["aliases"]
                .as_array()
                .is_some_and(|aliases| aliases.iter().any(|alias| alias.as_str() == Some(model)))
    });
    let Some(card) = matching.next() else {
        return false;
    };
    if matching.next().is_some() || card["owned_by"].as_str() != Some("llamacpp") {
        return false;
    }
    let Some(name) = card["id"].as_str() else {
        return false;
    };
    let mut matching = models
        .iter()
        .filter(|entry| entry["name"].as_str() == Some(name));
    matching.next().is_some_and(|entry| {
        entry["capabilities"] == json!(["completion"]) && matching.next().is_none()
    })
}

async fn post_count_json(
    client: &reqwest::Client,
    endpoint: &str,
    path: &str,
    api_key: Option<&str>,
    body: &Value,
) -> anyhow::Result<Option<ProbeResponse>> {
    request_count_json(
        super::maybe_bearer(
            client
                .post(format!("{}{path}", endpoint.trim_end_matches('/')))
                .json(body),
            api_key,
        ),
        path,
    )
    .await
}

async fn request_count_json(
    request: reqwest::RequestBuilder,
    path: &str,
) -> anyhow::Result<Option<ProbeResponse>> {
    let response = request
        .send()
        .await
        .with_context(|| format!("token counting request failed at {path}"))?;
    let status = response.status();
    if matches!(status.as_u16(), 404 | 405 | 501) {
        return Ok(None);
    }
    let (bytes, read_error) = crate::retry::read_response_bytes(response).await;
    let body = String::from_utf8_lossy(&bytes);
    // Preserve capacity evidence even if the server then disconnects. The
    // shared recovery classifier must see the rejection before the read error.
    if let Some(error) = read_error {
        return Err(error)
            .with_context(|| format!("token-count endpoint {path} returned {status}: {body}"));
    }
    anyhow::ensure!(
        status.is_success(),
        "token-count endpoint {path} returned {status}: {body}"
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("token-count endpoint {path} returned invalid JSON"))
        .map(|json| Some(ProbeResponse { json, bytes }))
}

fn positive_count(value: &Value, key: &str) -> anyhow::Result<usize> {
    value[key]
        .as_u64()
        .filter(|count| *count > 0)
        .and_then(|count| usize::try_from(count).ok())
        .with_context(|| format!("token-count response requires a positive integer `{key}`"))
}

fn token_count(value: &Value, key: &str) -> anyhow::Result<usize> {
    let tokens = value[key]
        .as_array()
        .context("token-count response requires a token array")?;
    anyhow::ensure!(
        !tokens.is_empty() && tokens.iter().all(|token| token.as_u64().is_some()),
        "token-count response requires nonempty integer token IDs"
    );
    Ok(tokens.len())
}

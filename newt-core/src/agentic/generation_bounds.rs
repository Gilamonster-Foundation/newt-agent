//! #2782: generation limits are independent of idle progress and cognition.
use super::observability::{DispatchError, ErrorClass};
use serde_json::Value;
use std::time::Duration;

fn positive_env(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

pub(super) fn total_duration() -> Duration {
    Duration::from_secs(u64::from(positive_env("NEWT_GENERATION_TIMEOUT_SECS", 900)))
}

pub(super) fn output_cap(
    allowance: Option<u32>,
    window: Option<u32>,
    input: usize,
) -> anyhow::Result<u32> {
    let maximum = positive_env("NEWT_GENERATION_MAX_TOKENS", 16_384);
    let mut cap = allowance.unwrap_or(maximum).min(maximum);
    if let Some(window) = window {
        let (_, remaining) = super::send_budget::context_window_split(window, input, 0);
        cap = cap.min(remaining);
    }
    if cap == 0 {
        return Err(DispatchError::context_exceeded(
            "pre-dispatch: context size has been exceeded; no room remains for bounded generation",
        )
        .into());
    }
    Ok(cap)
}

pub(super) fn apply_chat(
    body: &mut Value,
    url: &str,
    allowance: Option<u32>,
    window: Option<u32>,
    input: usize,
) -> anyhow::Result<u32> {
    let cap = output_cap(allowance, window, input)?;
    // Compatible servers traditionally accept max_tokens. First-party OpenAI
    // reasoning models require the modern field; proxies can select it explicitly.
    let modern = std::env::var("NEWT_CHAT_TOKEN_FIELD").ok().as_deref()
        == Some("max_completion_tokens")
        || reqwest::Url::parse(url)
            .ok()
            .is_some_and(|u| u.host_str() == Some("api.openai.com"));
    let object = body
        .as_object_mut()
        .expect("assembled Chat request is an object");
    object.remove("max_tokens");
    object.remove("max_completion_tokens");
    object.insert(
        if modern {
            "max_completion_tokens"
        } else {
            "max_tokens"
        }
        .into(),
        cap.into(),
    );
    Ok(cap)
}

pub(super) fn apply_responses(
    body: &mut Value,
    allowance: Option<u32>,
    window: Option<u32>,
    input: usize,
) -> anyhow::Result<u32> {
    let cap = output_cap(allowance, window, input)?;
    body["max_output_tokens"] = cap.into();
    Ok(cap)
}

/// A provider's explicit output-cap termination is not a usable completion.
pub(super) fn output_limited(json: &Value) -> bool {
    json["error"].is_null()
        && (json["choices"][0]["finish_reason"] == "length"
            || (json["status"] == "incomplete"
                && json["incomplete_details"]["reason"] == "max_output_tokens"))
}

/// Retain only visible assistant prose; never parse or recover capped tool calls.
pub(super) fn partial_responses_text(json: &Value) -> String {
    let text = json["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            item["type"] == "message" && (item["role"].is_null() || item["role"] == "assistant")
        })
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        json["output_text"].as_str().unwrap_or_default().to_string()
    } else {
        text
    }
}

pub(super) fn output_limit_notice(
    text: String,
    usage: Option<crate::TokenUsage>,
) -> super::FinalReply {
    let count = usage.map_or_else(
        || "token usage unavailable".to_string(),
        |usage| format!("{} generated tokens", usage.output_tokens),
    );
    let notice = format!("(model output limit reached; response truncated; {count}. No tool calls from this response were executed. Continuation requires a new operator request.)");
    super::FinalReply::from(text).with_notice(notice)
}

/// An ephemeral transport deadline, never persisted or used as identity.
#[derive(Clone, Copy)]
struct Deadline(tokio::time::Instant);

#[derive(Debug)]
pub(super) struct Stopped(String);
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Stopped {}

fn stopped(reason: &str, bytes: usize, class: ErrorClass) -> anyhow::Error {
    let message = format!("model generation stopped: {reason}; received {bytes} response bytes");
    anyhow::Error::new(Stopped(message.clone())).context(DispatchError::new(class, message))
}

/// Start the clock at the actual send, including headers, not after first output.
pub(super) fn execute<'a>(
    client: &'a reqwest::Client,
    request: reqwest::Request,
    failure: &'a str,
) -> impl std::future::Future<Output = anyhow::Result<reqwest::Response>> + 'a {
    // Return the box directly: an async wrapper would retain another request
    // slot in every enclosing attempt/provider future (#2824).
    Box::pin(execute_with_duration(
        client,
        request,
        failure,
        total_duration(),
    ))
}

async fn execute_with_duration(
    client: &reqwest::Client,
    request: reqwest::Request,
    failure: &str,
    duration: Duration,
) -> anyhow::Result<reqwest::Response> {
    let bounded = request.url().path().ends_with("/chat/completions")
        || request.url().path().ends_with("/responses");
    let send = Box::pin(client.execute(request));
    if !bounded {
        return send
            .await
            .map_err(|e| DispatchError::from_reqwest(failure, e).into());
    }
    let deadline = tokio::time::Instant::now() + duration;
    let mut response = tokio::time::timeout_at(deadline, send)
        .await
        .map_err(|_| {
            stopped(
                "total generation deadline reached before response headers",
                0,
                ErrorClass::Timeout,
            )
        })?
        .map_err(|e| DispatchError::from_reqwest(failure, e))?;
    response.extensions_mut().insert(Deadline(deadline));
    Ok(response)
}

/// Read while preserving partial bytes and dropping the response on either bound.
pub(super) fn read(
    response: reqwest::Response,
    bytes: &mut Vec<u8>,
) -> impl std::future::Future<Output = anyhow::Result<Option<reqwest::Error>>> + '_ {
    // #2824: the timeout and checked-reader state otherwise propagates through
    // every provider/retry/cancellation future and overflows coverage stacks.
    // Keep response ownership inside the box: dropping the caller still cancels
    // the read, while the borrowed buffer retains bytes already received.
    Box::pin(read_bounded(response, bytes))
}

async fn read_bounded(
    response: reqwest::Response,
    bytes: &mut Vec<u8>,
) -> anyhow::Result<Option<reqwest::Error>> {
    let Some(deadline) = response.extensions().get::<Deadline>().copied() else {
        return Ok(crate::retry::read_response_bytes_into(response, bytes).await);
    };
    let mut watch = super::generation_repetition::Watch::default();
    let read = crate::retry::read_response_bytes_into_checked(response, bytes, |chunk| {
        watch.observe(chunk)
    });
    match tokio::time::timeout_at(deadline.0, read).await {
        Ok(Ok(error)) => Ok(error),
        Ok(Err(_)) => Err(stopped(
            "repeated generation loop detected",
            bytes.len(),
            ErrorClass::Model,
        )),
        Err(_) => Err(stopped(
            "total generation deadline reached",
            bytes.len(),
            ErrorClass::Timeout,
        )),
    }
}

pub(super) fn append_notice(text: &mut super::FinalReply, error: &anyhow::Error) {
    if error.downcast_ref::<Stopped>().is_some() {
        text.notices.push(error.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// #2824: deadline/repetition state must not inflate every provider caller.
    #[test]
    fn bounds_2824_read_future_keeps_transport_state_off_caller_stack() {
        fn returned_size<T, F>(_: impl FnOnce(T) -> F) -> usize {
            std::mem::size_of::<F>()
        }
        let mut bytes = Vec::new();
        let size = returned_size(|response| read(response, &mut bytes));
        eprintln!("bounded reader future: {size} bytes");
        assert!(size <= 512, "bounded reader future occupies {size} bytes");
    }

    /// #2782: an injected total bound includes the wait for response headers.
    #[tokio::test]
    async fn bounds_2782_total_deadline_includes_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            sent.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let client = reqwest::Client::new();
        let request = client
            .post(url)
            .json(&serde_json::json!({}))
            .build()
            .unwrap();
        let mut send = Box::pin(execute_with_duration(
            &client,
            request,
            "request failed",
            Duration::from_secs(2),
        ));
        tokio::select! {
            result = &mut send => panic!("headers were not sent: {result:?}"),
            _ = received => {}
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        let error = send.await.unwrap_err();
        server.abort();
        assert!(
            error.to_string().contains("before response headers"),
            "{error:#}"
        );
        assert_eq!(
            crate::retry::classify(&error),
            crate::retry::Retryability::Fatal
        );
    }
}

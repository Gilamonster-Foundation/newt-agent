//! Protocol-grounded fixtures: count the rendered chat, not serialized JSON.
use super::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::body_json;

const MODEL: &str = "test/model:latest";
const KEY: &str = "count-test-token";

fn chat() -> Value {
    json!({
        "model": MODEL,
        "messages": [
            {"role":"system", "content":"test policy"},
            {"role":"user", "content":"read the file"},
            {"role":"assistant", "tool_calls":[{"id":"call_1", "type":"function",
                "function":{"name":"read_file", "arguments":"{\"path\":\"example.rs\"}"}}]},
            {"role":"tool", "tool_call_id":"call_1", "content":"file contents"}
        ],
        "tools":[{"type":"function", "function":{"name":"read_file",
            "parameters":{"type":"object", "properties":{"path":{"type":"string"}}}}}],
        "tool_choice":"auto",
        "chat_template_kwargs":{"enable_thinking":false},
        "stream":false
    })
}

async fn count(server: &MockServer, body: &Value) -> anyhow::Result<Option<TokenCount>> {
    count_chat_tokens(
        &reqwest::Client::new(),
        &server.uri(),
        MODEL,
        Some(KEY),
        body,
    )
    .await
}

fn text_model_metadata() -> Value {
    json!({
        "data":[{"id":MODEL,"owned_by":"llamacpp","aliases":[]}],
        "models":[{"name":MODEL,"capabilities":["completion"]}]
    })
}

async fn mount_text_model(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", format!("Bearer {KEY}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_model_metadata()))
        .mount(server)
        .await;
}

#[tokio::test]
async fn chat_token_count_prefers_the_exact_llama_chat_endpoint() {
    let server = MockServer::start().await;
    let body = chat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/input_tokens"))
        .and(header("authorization", format!("Bearer {KEY}")))
        .and(body_json(&body))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{ "input_tokens": 37 }"#))
        .expect(1)
        .mount(&server)
        .await;
    let measured = count(&server, &body).await.unwrap().unwrap();
    assert_eq!(measured.tokens, 37);
    assert_eq!(
        measured.response_bytes.as_slice(),
        br#"{ "input_tokens": 37 }"#
    );
    assert_eq!(measured.method, "llama_chat_input_tokens");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn chat_token_count_vllm_renderer_receives_the_complete_request() {
    let server = MockServer::start().await;
    let mut body = chat();
    body["reasoning_effort"] = json!("high");
    body["response_format"] = json!({"type":"json_object"});
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/render"))
        .and(header("authorization", format!("Bearer {KEY}")))
        .and(body_json(&body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model":MODEL,"token_ids":[1,2,3],"features":null
        })))
        .expect(1)
        .mount(&server)
        .await;
    let measured = count(&server, &body).await.unwrap().unwrap();
    assert_eq!(measured.tokens, 3);
    assert_eq!(
        serde_json::from_slice::<Value>(&measured.response_bytes).unwrap(),
        json!({"model":MODEL,"token_ids":[1,2,3],"features":null})
    );
    assert_eq!(measured.method, "vllm_chat_render");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn chat_token_count_vllm_renderer_refuses_invalid_or_nontext_measurements() {
    for rendered in [
        json!({}),
        json!({"model":MODEL,"token_ids":[]}),
        json!({"model":MODEL,"token_ids":[-1]}),
        json!({"model":"another-model","token_ids":[1]}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions/render"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rendered))
            .mount(&server)
            .await;
        assert!(count(&server, &chat()).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/render"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model":MODEL,"token_ids":[1],"features":{"image":{}}
        })))
        .mount(&server)
        .await;
    assert_eq!(count(&server, &chat()).await.unwrap(), None);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn chat_token_count_nontext_requests_retain_the_calibrated_fallback() {
    let server = MockServer::start().await;
    let mut body = chat();
    body["messages"][1]["content"] = json!([{"type":"image_url", "image_url":{
        "url":"data:image/png;base64,AAAA"}}]);
    assert_eq!(count(&server, &body).await.unwrap(), None);
    let mut body = chat();
    body["prompt_embeds"] = json!("opaque-embedding-data");
    assert_eq!(count(&server, &body).await.unwrap(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn chat_token_count_renderer_rejection_retains_context_recovery_evidence() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/render"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error":{"message":"Context size has been exceeded"}
        })))
        .mount(&server)
        .await;
    let error = count(&server, &chat()).await.unwrap_err();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

async fn drain_count_request(socket: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut request = Vec::new();
    loop {
        let mut byte = [0_u8];
        socket.read_exact(&mut byte).await.unwrap();
        request.push(byte[0]);
        if request.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let length = String::from_utf8(request)
        .unwrap()
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|length| length.parse::<usize>().ok())
        })
        .unwrap();
    socket.read_exact(&mut vec![0; length]).await.unwrap();
}

/// Grounds retry classification in a peer that closes after receiving the
/// complete request, before any HTTP response exists. No observed status or
/// body may be invented to turn this transport failure into a fatal error.
#[tokio::test]
async fn chat_token_count_transport_disconnect_retains_transient_retry_classification() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        drain_count_request(&mut socket).await;
        socket.shutdown().await.unwrap();
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let error = count_chat_tokens(&client, &endpoint, MODEL, Some(KEY), &chat())
        .await
        .unwrap_err();
    server.await.unwrap();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::Retry
    );
    assert!(error
        .to_string()
        .contains("/v1/chat/completions/input_tokens"));
}

/// Grounds mocked rejection handling in an HTTP peer that closes mid-body.
#[tokio::test]
async fn chat_token_count_partial_rejection_retains_observed_context_evidence() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        drain_count_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 500\r\nConnection: close\r\n\r\nContext size has been exceeded").await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let error = count_chat_tokens(&client, &endpoint, MODEL, Some(KEY), &chat())
        .await
        .unwrap_err();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded
    );
    server.await.unwrap();
}

#[tokio::test]
async fn chat_token_count_legacy_vllm_retains_fallback_for_unproven_chat_semantics() {
    let server = MockServer::start().await;
    let body = chat();
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .and(header("authorization", format!("Bearer {KEY}")))
        .and(body_json(&body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "count":3, "tokens":[1,2,3], "max_model_len":65536
        })))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(count(&server, &body).await.unwrap(), None);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn chat_token_count_older_llama_tokenizes_its_rendered_template() {
    let server = MockServer::start().await;
    mount_text_model(&server).await;
    let mut body = chat();
    body["response_format"] = json!({"type":"json_object"});
    body["parallel_tool_calls"] = json!(false);
    body["reasoning_effort"] = json!("none");
    let original = body.clone();
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .and(header("authorization", format!("Bearer {KEY}")))
        .respond_with(move |request: &wiremock::Request| {
            let actual: Value = serde_json::from_slice(&request.body).unwrap();
            if actual.get("content").is_some() {
                assert_eq!(
                    actual,
                    json!({"model":MODEL, "content":"<bos>templated tools and chat<assistant>",
                    "add_special":true, "parse_special":true})
                );
                ResponseTemplate::new(200).set_body_json(json!({"tokens":[1,2,3,4]}))
            } else {
                assert_eq!(actual, original);
                ResponseTemplate::new(200).set_body_json(json!({"tokens":[]}))
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/apply-template"))
        .and(header("authorization", format!("Bearer {KEY}")))
        .and(body_json(&body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "prompt":"<bos>templated tools and chat<assistant>"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let measured = count(&server, &body).await.unwrap().unwrap();
    assert_eq!(measured.tokens, 4);
    assert_eq!(
        serde_json::from_slice::<Value>(&measured.response_bytes).unwrap(),
        json!({"tokens":[1,2,3,4]})
    );
    assert_eq!(measured.method, "llama_template_tokenize");
    let paths = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "/v1/chat/completions/input_tokens",
            "/v1/chat/completions/render",
            "/tokenize",
            "/v1/models",
            "/apply-template",
            "/tokenize"
        ]
    );
}

#[tokio::test]
async fn chat_token_count_absent_endpoints_leave_anchored_fallback_available() {
    for status in [404, 405, 501] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        assert_eq!(count(&server, &chat()).await.unwrap(), None);
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
}

#[tokio::test]
async fn chat_token_count_missing_llama_template_is_not_a_zero_token_admission() {
    let server = MockServer::start().await;
    mount_text_model(&server).await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens":[]})))
        .mount(&server)
        .await;
    assert_eq!(count(&server, &chat()).await.unwrap(), None);
    assert_eq!(server.received_requests().await.unwrap().len(), 5);
}

#[tokio::test]
async fn chat_token_count_rejects_zero_malformed_and_inconsistent_counts() {
    for body in [
        json!({}),
        json!({"input_tokens":0}),
        json!({"input_tokens":-1}),
        json!({"input_tokens":"37"}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions/input_tokens"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        assert!(count(&server, &chat()).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
    for body in [
        json!({"count":0,"tokens":[]}),
        json!({"count":2,"tokens":[1]}),
        json!({"count":1,"tokens":[-1]}),
        json!({"count":1}),
        json!({"tokens":[1]}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        assert!(count(&server, &chat()).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
}

#[tokio::test]
async fn chat_token_count_does_not_treat_auth_or_server_failure_as_absence() {
    for status in [400, 401, 403, 429, 500, 503] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let error = count(&server, &chat()).await.unwrap_err();
        assert_eq!(
            crate::retry::classify(&error),
            if status == 429 || status >= 500 {
                crate::retry::Retryability::Retry
            } else {
                crate::retry::Retryability::Fatal
            },
            "observed HTTP {status} retains its classification: {error:#}"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn chat_token_count_recounts_every_access_and_requires_the_same_model() {
    let server = MockServer::start().await;
    let calls = AtomicUsize::new(0);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/input_tokens"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200)
                .set_body_json(json!({"input_tokens":calls.fetch_add(1, Ordering::Relaxed) + 1}))
        })
        .mount(&server)
        .await;
    assert_eq!(count(&server, &chat()).await.unwrap().unwrap().tokens, 1);
    assert_eq!(count(&server, &chat()).await.unwrap().unwrap().tokens, 2);
    let mut other = chat();
    other["model"] = json!("another-model");
    assert!(count(&server, &other).await.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn chat_token_count_rejects_invalid_json_and_template_token_shapes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/input_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{invalid"))
        .mount(&server)
        .await;
    assert!(count(&server, &chat()).await.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    for template in [json!({}), json!({"prompt":""}), json!({"prompt":42})] {
        let server = MockServer::start().await;
        mount_text_model(&server).await;
        Mock::given(method("POST"))
            .and(path("/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens":[]})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/apply-template"))
            .respond_with(ResponseTemplate::new(200).set_body_json(template))
            .mount(&server)
            .await;
        assert!(count(&server, &chat()).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 5);
    }
    for tokens in [json!({}), json!({"tokens":[]}), json!({"tokens":[-1]})] {
        let server = MockServer::start().await;
        mount_text_model(&server).await;
        Mock::given(method("POST"))
            .and(path("/tokenize"))
            .respond_with(move |request: &wiremock::Request| {
                let actual: Value = serde_json::from_slice(&request.body).unwrap();
                ResponseTemplate::new(200).set_body_json(if actual.get("content").is_some() {
                    tokens.clone()
                } else {
                    json!({"tokens":[]})
                })
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/apply-template"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"prompt":"rendered"})))
            .mount(&server)
            .await;
        assert!(count(&server, &chat()).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 6);
    }
}

#[tokio::test]
async fn chat_token_count_legacy_llama_requires_explicit_matching_text_capability() {
    let mut multimodal = text_model_metadata();
    multimodal["models"][0]["capabilities"] = json!(["completion", "multimodal"]);
    let mut unknown = text_model_metadata();
    unknown["models"][0]["capabilities"] = json!(["completion", "future-capability"]);
    let mut unrelated = text_model_metadata();
    unrelated["models"][0]["name"] = json!("unrelated-model");
    let mut duplicate = text_model_metadata();
    duplicate["data"] = json!([duplicate["data"][0].clone(), duplicate["data"][0].clone()]);
    let mut foreign = text_model_metadata();
    foreign["data"][0]["owned_by"] = json!("another-server");
    for metadata in [
        None,
        Some(json!({})),
        Some(multimodal),
        Some(unknown),
        Some(unrelated),
        Some(duplicate),
        Some(foreign),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens":[]})))
            .mount(&server)
            .await;
        if let Some(metadata) = metadata {
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .and(header("authorization", format!("Bearer {KEY}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
                .mount(&server)
                .await;
        }
        assert_eq!(count(&server, &chat()).await.unwrap(), None);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests.last().unwrap().url.path(), "/v1/models");
    }
}

#[tokio::test]
async fn chat_token_count_legacy_llama_revalidates_alias_capability_every_turn() {
    let server = MockServer::start().await;
    let mut metadata = text_model_metadata();
    metadata["data"][0]["id"] = json!("canonical-model");
    metadata["data"][0]["aliases"] = json!([MODEL]);
    metadata["models"][0]["name"] = json!("canonical-model");
    let calls = AtomicUsize::new(0);
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", format!("Bearer {KEY}")))
        .respond_with(move |_: &wiremock::Request| {
            let mut observed = metadata.clone();
            if calls.fetch_add(1, Ordering::Relaxed) != 0 {
                observed["models"][0]["capabilities"] = json!(["completion", "multimodal"]);
            }
            ResponseTemplate::new(200).set_body_json(observed)
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            ResponseTemplate::new(200).set_body_json(if body.get("content").is_some() {
                json!({"tokens":[1,2]})
            } else {
                json!({"tokens":[]})
            })
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/apply-template"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"prompt":"rendered"})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(count(&server, &chat()).await.unwrap().unwrap().tokens, 2);
    assert_eq!(count(&server, &chat()).await.unwrap(), None);
    assert_eq!(server.received_requests().await.unwrap().len(), 10);
}

#[tokio::test]
async fn chat_token_count_legacy_llama_capability_errors_are_not_absence() {
    for status in [400, 401, 403, 429, 500, 503] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tokens":[]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        assert!(count(&server, &chat()).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }
}

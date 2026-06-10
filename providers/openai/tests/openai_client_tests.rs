use newt_provider_openai::OpenAiClient;
use plugins_protocol::{CompleteRequest, Message};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn complete_request() -> CompleteRequest {
    CompleteRequest {
        model: "gpt-test".to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: "hello".to_string(),
        }],
        max_tokens: Some(32),
    }
}

#[tokio::test]
async fn complete_sends_bearer_token_and_maps_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_json(serde_json::json!({
            "model": "gpt-test",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 32,
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "gpt-test-echo",
            "choices": [{
                "message": {"role": "assistant", "content": "mocked"}
            }],
            "usage": {
                "prompt_tokens": 7,
                "completion_tokens": 11
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = OpenAiClient::new(server.uri(), Some("test-key".to_string()));
    let reply = client.complete(complete_request()).await.unwrap();

    assert_eq!(reply.content, "mocked");
    assert_eq!(reply.model_id, "gpt-test-echo");
    let usage = reply.usage.expect("usage propagated");
    assert_eq!(usage.input_tokens, 7);
    assert_eq!(usage.output_tokens, 11);
}

#[tokio::test]
async fn list_models_sends_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "gpt-test", "object": "model"},
                {"id": "gpt-other", "object": "model"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = OpenAiClient::new(server.uri(), Some("test-key".to_string()));
    let models = client.list_models().await.unwrap();

    assert_eq!(models.models, vec!["gpt-test", "gpt-other"]);
}

#[tokio::test]
async fn complete_requires_api_key() {
    let client = OpenAiClient::new("http://127.0.0.1:9", None);

    let err = client.complete(complete_request()).await.unwrap_err();

    assert!(err.to_string().contains("OPENAI_API_KEY"));
}

#[tokio::test]
async fn non_success_status_includes_bounded_body_excerpt() {
    let server = MockServer::start().await;
    let long_body = "x".repeat(800);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string(long_body))
        .mount(&server)
        .await;

    let client = OpenAiClient::new(server.uri(), Some("test-key".to_string()));
    let err = client.complete(complete_request()).await.unwrap_err();
    let text = err.to_string();

    assert!(text.contains("429"));
    assert!(text.len() < 700, "error body should be bounded: {text}");
}

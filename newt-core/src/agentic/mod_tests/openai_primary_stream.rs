//! Primary streaming dispatch, distinct from the final display reissue.
use super::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOOL: &str = "stream_fixture__read_result";

#[derive(Default)]
struct FixtureTools(Vec<String>);

#[async_trait::async_trait]
impl McpTools for FixtureTools {
    fn handles(&self, name: &str) -> bool {
        name == TOOL
    }
    fn tool_defs(&self) -> Vec<Value> {
        vec![json!({"type":"function","function":{"name":TOOL,
            "description":"Read a deterministic fixture.","parameters":{"type":"object",
            "properties":{"stage":{"type":"string"}},"required":["stage"]}}})]
    }
    async fn call(&mut self, call: &LeasedMcpCall<'_>) -> String {
        let stage = call.args()["stage"].as_str().unwrap().to_owned();
        self.0.push(stage.clone());
        format!("fixture result {stage}")
    }
}

fn wire(frames: &[Value], done: bool) -> ResponseTemplate {
    let mut body = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>();
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream")
}

fn streamed_batch(done: bool, valid_arguments: bool) -> ResponseTemplate {
    wire(
        &[
            json!({"model":"served-model","choices":[{"delta":{"reasoning_content":"fixture reasoning",
            "tool_calls":[
                {"index":0,"id":"call-a","type":"function","function":{"name":TOOL,"arguments":"{\"stage\":"}},
                {"index":1,"id":"call-b","type":"function","function":{"name":TOOL,"arguments":"{\"stage\":"}}
            ]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"\"a\"}"}},
            {"index":1,"function":{"arguments":if valid_arguments { "\"b\"}" } else { "\"b" }}}
        ]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":2000,"completion_tokens":4}}),
        ],
        done,
    )
}

fn streamed_answer() -> ResponseTemplate {
    wire(
        &[
            json!({"choices":[{"delta":{"content":"Fixture response."},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":2100,"completion_tokens":2}}),
        ],
        true,
    )
}

fn complete_single_tool_frame() -> Value {
    json!({"model":"served-model","choices":[{"index":0,"delta":{"tool_calls":[{
        "index":0,"id":"call-a","type":"function","function":{
            "name":TOOL,"arguments":"{\"stage\":\"a\"}"
        }
    }]},"finish_reason":"tool_calls"}]})
}

fn complete_single_tool_prefix() -> String {
    format!("data: {}\n\n", complete_single_tool_frame())
}

#[tokio::test]
async fn primary_stream_assembles_a_tool_batch_once_and_preserves_call_ids() {
    let server = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = seen.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body = body_json(request);
            let has_result = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool");
            requests.lock().unwrap().push(body);
            if has_result {
                streamed_answer()
            } else {
                streamed_batch(true, true)
            }
        })
        .mount(&server)
        .await;
    let uri = server.uri();
    let messages = msgs();
    let caveats = Caveats::top();
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    let allowed = [TOOL.to_owned()];
    context.persona_tools = Some(&allowed);
    let mut tools = FixtureTools::default();
    let (text, streamed, usage, hallucinations) = chat_complete(context, &mut tools)
        .await
        .expect("a complete streamed tool batch must reach execution");
    assert_eq!(tools.0, ["a", "b"]);
    assert_eq!(text, "Fixture response.");
    assert!(
        streamed,
        "the accepted final answer retains its display reissue"
    );
    assert_eq!(hallucinations, 0);
    assert_eq!(usage.unwrap().output_tokens, 8);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "tool batch, final answer, display reissue");
    for request in seen.iter() {
        assert_eq!(request["stream"], true);
        assert_eq!(request["stream_options"]["include_usage"], true);
    }
    let results = seen[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["tool_call_id"], "call-a");
    assert_eq!(results[0]["content"], "fixture result a");
    assert_eq!(results[1]["tool_call_id"], "call-b");
    assert_eq!(results[1]["content"], "fixture result b");
}

#[tokio::test]
async fn primary_stream_rejects_a_cut_or_malformed_batch_before_any_tool_runs() {
    for (done, valid_arguments) in [(false, true), (true, false)] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(streamed_batch(done, valid_arguments))
            .mount(&server)
            .await;
        let uri = server.uri();
        let messages = msgs();
        let caveats = Caveats::top();
        let mut context = ctx(&uri, &messages, &caveats);
        context.action_nudges = false;
        let allowed = [TOOL.to_owned()];
        context.persona_tools = Some(&allowed);
        let mut tools = FixtureTools::default();
        assert!(chat_complete(context, &mut tools).await.is_err());
        assert!(tools.0.is_empty(), "no partial batch may authorize a tool");
        let requests = server.received_requests().await.unwrap();
        let generation = requests
            .iter()
            .filter(|request| request.url.path() == "/v1/chat/completions")
            .collect::<Vec<_>>();
        assert_eq!(generation.len(), 1);
        assert_eq!(body_json(generation[0])["stream"], true);
    }
}

#[tokio::test]
async fn cancelled_stream_with_a_complete_tool_call_dispatches_nothing() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let (delivered_tx, delivered_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (_, request) = read_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Connection: close\r\n\r\n{}",
                    complete_single_tool_prefix()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let padding = format!(":{}\n", "x".repeat(64 * 1024 - 2));
        for _ in 0..256 {
            socket.write_all(padding.as_bytes()).await.unwrap();
        }
        delivered_tx.send(request).unwrap();
        match socket.read(&mut [0_u8; 1]).await {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            other => panic!("cancelled generation socket remained active: {other:?}"),
        }
    });

    let messages = msgs();
    let caveats = Caveats::top();
    let cancel = AtomicBool::new(false);
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    context.cancel = Some(&cancel);
    let allowed = [TOOL.to_owned()];
    context.persona_tools = Some(&allowed);
    let mut tools = FixtureTools::default();
    let mut completion = Box::pin(chat_complete(context, &mut tools));
    let request = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            request = delivered_rx => request.unwrap(),
            result = &mut completion => panic!("unfinished response resolved: {result:?}"),
        }
    })
    .await
    .expect("the complete tool-call frame must reach the client");
    assert_eq!(request["stream"], true);

    cancel.store(true, Ordering::Relaxed);
    let (reply, _, _, _) = tokio::time::timeout(Duration::from_secs(30), &mut completion)
        .await
        .expect("cancellation must drop the response body")
        .expect("cancellation is a clean turn outcome");
    assert!(reply.is_empty());
    drop(completion);
    server.await.unwrap();
    assert_eq!(
        tools.0.len(),
        0,
        "a complete call is inert until the whole streamed response is accepted"
    );
}

#[tokio::test]
async fn completed_stream_with_the_same_tool_call_dispatches_once() {
    let server = MockServer::start().await;
    let round = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let round = round.clone();
            move |_: &Request| match round.fetch_add(1, Ordering::Relaxed) {
                0 => wire(&[complete_single_tool_frame()], true),
                _ => streamed_answer(),
            }
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    let allowed = [TOOL.to_owned()];
    context.persona_tools = Some(&allowed);
    let mut tools = FixtureTools::default();

    chat_complete(context, &mut tools)
        .await
        .expect("the same call followed by [DONE] must be accepted");

    assert_eq!(tools.0, ["a"], "the completed twin must exercise dispatch");
}

#[tokio::test]
async fn provider_error_after_a_complete_tool_call_dispatches_nothing() {
    let server = MockServer::start().await;
    let body = format!(
        "{}data: {{\"error\":{{\"message\":\"fixture rejection\",\"code\":400}}}}\n\n",
        complete_single_tool_prefix()
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    let allowed = [TOOL.to_owned()];
    context.persona_tools = Some(&allowed);
    let mut tools = FixtureTools::default();

    assert!(chat_complete(context, &mut tools).await.is_err());
    assert_eq!(
        tools.0.len(),
        0,
        "an observed provider rejection cannot authorize a preceding call"
    );
}

#[tokio::test]
async fn cap_summary_stream_decodes_the_answer_and_usage_without_tools() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(streamed_answer())
        .mount(&server)
        .await;
    let uri = server.uri();
    let messages = msgs();
    let caveats = Caveats::top();
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    context.max_tool_rounds = 0;
    let (text, streamed, usage, _) = chat_complete(context, &mut NoMcp).await.unwrap();
    assert_eq!(text, "Fixture response.");
    assert!(
        !streamed,
        "the cap summary is returned for its existing display path"
    );
    assert_eq!(usage.unwrap().output_tokens, 2);
    let requests = server.received_requests().await.unwrap();
    let generation = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(generation.len(), 1);
    let body = body_json(generation[0]);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("tools").is_none());
}

async fn progressing_core_stream_resets_its_idle_timeout(cap_summary: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (advance_tx, mut advance_rx) = tokio::sync::mpsc::unbounded_channel();
    let (written_tx, mut written_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (path, request) = read_request(&mut socket).await;
        assert_eq!(path, "/v1/chat/completions");
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n").await.unwrap();
        started_tx.send(request.clone()).unwrap();
        for frame in [
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"progressing \"}}]}\n\n".as_slice(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"}}]}\n\n".as_slice(),
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".as_slice(),
        ] {
            advance_rx.recv().await.expect("test advances the stream");
            socket.write_all(frame).await.unwrap();
            written_tx.send(()).unwrap();
        }
        socket.shutdown().await.unwrap();

        if !cap_summary {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (path, display) = read_request(&mut socket).await;
            assert_eq!(path, "/v1/chat/completions");
            assert_eq!(display["stream"], true);
            let body = concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"display answer\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        }
        request
    });

    let messages = msgs();
    let caveats = Caveats::top();
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    context.inference_timeout_secs = 1;
    if cap_summary {
        context.max_tool_rounds = 0;
    }
    let mut tools = NoMcp;
    let mut completion = Box::pin(chat_complete(context, &mut tools));
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            request = started_rx => request.unwrap(),
            result = &mut completion => panic!("stream ended before its first frame: {result:?}"),
        }
    })
    .await
    .expect("the generation must begin");
    assert_eq!(request["stream"], true);

    tokio::time::pause();
    for index in 0..3 {
        tokio::time::advance(Duration::from_millis(400)).await;
        advance_tx.send(()).unwrap();
        written_rx.recv().await.unwrap();
        if index < 2 {
            tokio::select! {
                biased;
                result = &mut completion => {
                    panic!("stream ended before its terminal frame: {result:?}")
                }
                _ = tokio::task::yield_now() => {}
            }
        }
    }
    tokio::time::resume();
    let (text, streamed, _, _) = tokio::time::timeout(Duration::from_secs(5), &mut completion)
        .await
        .expect("the progressing generation must finish")
        .expect("progress must reset the idle timeout");
    server.await.unwrap();
    if cap_summary {
        assert_eq!(text, "progressing answer");
        assert!(!streamed);
    } else {
        assert_eq!(text, "display answer");
        assert!(streamed);
    }
}

#[tokio::test]
async fn primary_progressing_stream_resets_its_idle_timeout() {
    progressing_core_stream_resets_its_idle_timeout(false).await;
}

#[tokio::test]
async fn cap_summary_progressing_stream_resets_its_idle_timeout() {
    progressing_core_stream_resets_its_idle_timeout(true).await;
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let path = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() - header_end < length {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0);
        bytes.extend_from_slice(&chunk[..read]);
    }
    (
        path,
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
    )
}

/// Grounds the decoder tests' cancellation assumption in an actual HTTP peer.
/// Counts may be observed first; the generation blocks behind a deterministic
/// channel so cancellation cannot race the count or its matching dispatch.
async fn cancellation_case(set_flag: bool, cap_summary: bool, measured: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let (started, reading) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut measurement: Option<(Value, usize, usize)> = None;
        let (mut socket, body) = loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (path, body) = read_request(&mut socket).await;
            if path == "/v1/chat/completions" {
                if measured {
                    let (counted, _, _) = measurement.as_ref().expect("count precedes generation");
                    assert_eq!(
                        &body, counted,
                        "generation must match the complete counted request"
                    );
                }
                break (socket, body);
            }
            if measured && path == "/v1/chat/completions/input_tokens" {
                let prior = estimate_request_tokens(
                    body["messages"].as_array().unwrap(),
                    body.get("tools"),
                    crate::tokens::TokenEstimation::default(),
                );
                let tokens = prior * 2;
                assert!(
                    tokens <= 100_000,
                    "fixture count must fit its configured input bound"
                );
                assert!(measurement.replace((body, prior, tokens)).is_none());
                let response = json!({"input_tokens":tokens}).to_string();
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()
                ).as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
                continue;
            }
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        };
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n").await.unwrap();
        started.send(body).unwrap();
        match socket.read(&mut [0_u8; 1]).await {
            Ok(0) => (),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            other => panic!("the generation socket remained active: {other:?}"),
        }
        measurement.map(|(_, prior, tokens)| (prior, tokens))
    });
    let messages = msgs();
    let caveats = Caveats::top();
    let cancel = AtomicBool::new(false);
    let mut state = CompressState::new();
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    context.cancel = Some(&cancel);
    context.compress_state = Some(&mut state);
    if measured {
        context.safe_context = Some(100_000);
    }
    if cap_summary {
        context.max_tool_rounds = 0;
    }
    let mut tools = NoMcp;
    let mut completion = Box::pin(chat_complete(context, &mut tools));
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            request = reading => request.unwrap(),
            result = &mut completion => panic!("unfinished response resolved: {result:?}"),
        }
    })
    .await
    .expect("the generation must reach the server");
    let result = if set_flag {
        cancel.store(true, Ordering::Relaxed);
        Some(tokio::time::timeout(Duration::from_secs(5), &mut completion).await)
    } else {
        None
    };
    drop(completion);
    let measurement = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server must observe socket closure")
        .unwrap();
    // Clean up before asserting on either streaming generation path.
    if let Some(result) = result {
        let (reply, _, _, _) = result
            .expect("cancel flag must interrupt send and body read")
            .unwrap();
        assert!(reply.is_empty());
    }
    assert_eq!(request["stream"], true);
    assert_eq!(request["stream_options"]["include_usage"], true);
    if let Some((prior, tokens)) = measurement {
        assert_eq!(
            state.calibration.ratio(None),
            tokens as f32 / prior as f32,
            "cancellation must retain the completed count of the same request"
        );
    }
}

#[tokio::test]
async fn primary_stream_dropping_future_closes_the_generation_socket() {
    cancellation_case(false, false, false).await;
}

#[tokio::test]
async fn primary_stream_cancel_flag_closes_the_generation_socket() {
    cancellation_case(true, false, false).await;
}

#[tokio::test]
async fn cap_summary_stream_cancel_flag_closes_the_generation_socket() {
    cancellation_case(true, true, false).await;
}

#[tokio::test]
async fn primary_stream_cancellation_retains_its_completed_token_count() {
    cancellation_case(true, false, true).await;
}

#[tokio::test]
async fn cap_summary_stream_cancellation_retains_its_completed_token_count() {
    cancellation_case(true, true, true).await;
}

/// Grounds the response reader's status precedence in a real incomplete HTTP
/// frame: a known provider status is not replaced by a later socket failure.
#[tokio::test]
async fn provider_http_status_survives_a_truncated_response_body() {
    use crate::retry::Retryability;
    for (status, expected) in [
        ("401 Unauthorized", Retryability::Fatal),
        ("503 Service Unavailable", Retryability::Retry),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 128\r\nConnection: close\r\n\r\nfixture provider failure").as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let response = reqwest::Client::new()
                .post(uri)
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            crate::agentic::smart_harness::response(response, None, "inference endpoint").await
        })
        .await
        .unwrap();
        server.await.unwrap();
        let error = result.unwrap_err();
        assert_eq!(crate::retry::classify(&error), expected, "{error}");
        assert_eq!(
            crate::agentic::observability::error_class(&error),
            Some(crate::agentic::observability::ErrorClass::Model)
        );
        assert!(
            error.to_string().contains("fixture provider failure"),
            "{error}"
        );
        assert!(
            error.to_string().contains("response body read failed"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn successful_stream_mid_body_disconnect_is_retryable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 4096\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            )
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
    });
    let response = reqwest::Client::new()
        .post(uri)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let error = crate::agentic::smart_harness::response_with_decoder(
        response,
        None,
        "inference endpoint",
        crate::agentic::smart_harness::decode_openai_response,
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::Retry,
        "a cut successful stream is a retryable transport failure: {error:#}"
    );
    assert_eq!(
        crate::agentic::observability::error_class(&error),
        Some(crate::agentic::observability::ErrorClass::Transport),
        "a body cut is a transport failure even when the status was successful"
    );
}

#[tokio::test]
async fn successful_stream_idle_timeout_is_retryable_and_typed_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            )
            .await
            .unwrap();
        match socket.read(&mut [0_u8; 1]).await {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            other => panic!("idle timeout left the response socket active: {other:?}"),
        }
    });
    let response = reqwest::Client::builder()
        .read_timeout(Duration::from_millis(100))
        .build()
        .unwrap()
        .post(uri)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let error = crate::agentic::smart_harness::response_with_decoder(
        response,
        None,
        "inference endpoint",
        crate::agentic::smart_harness::decode_openai_response,
    )
    .await
    .unwrap_err();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server must observe the idle timeout")
        .unwrap();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::Retry,
        "a successful response body timeout remains retryable: {error:#}"
    );
    assert_eq!(
        crate::agentic::observability::error_class(&error),
        Some(crate::agentic::observability::ErrorClass::Timeout)
    );
    assert!(
        error
            .to_string()
            .contains("request failed reading response"),
        "{error}"
    );
}

/// Grounds the retained-byte reader in the primary and cap transports' real
/// reqwest idle timeout. The peer sends a complete rejection event, then waits
/// for the client to close its unfinished HTTP body; no fixture sleep completes it.
async fn observed_context_error_survives_core_idle_timeout(cap_summary: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let (closed, closure) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut closed = Some(closed);
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (path, body) = read_request(&mut socket).await;
            if path != "/v1/chat/completions" {
                socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                socket.shutdown().await.unwrap();
                continue;
            }
            let first = {
                let mut requests = captured.lock().unwrap();
                requests.push(body);
                requests.len() == 1
            };
            if !first {
                // An erroneous unchanged retry must fail promptly, allowing the
                // test to inspect the actual send count instead of hanging.
                socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                socket.shutdown().await.unwrap();
                return;
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 4096\r\nConnection: close\r\n\r\ndata: {\"error\":{\"message\":\"Context size has been exceeded.\",\"code\":500}}\n\n")
                .await
                .unwrap();
            match socket.read(&mut [0_u8; 1]).await {
                Ok(0) => (),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                other => panic!("the generation socket remained active: {other:?}"),
            }
            closed.take().unwrap().send(()).unwrap();
        }
    });
    let messages = msgs();
    let caveats = Caveats::top();
    let mut observation = observability::SolveObservation::default();
    let mut state = CompressState::new();
    let mut reason = None;
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    context.inference_timeout_secs = 1;
    context.solve_obs = Some(&mut observation);
    context.compress_state = Some(&mut state);
    context.end_reason = Some(&mut reason);
    if cap_summary {
        context.max_tool_rounds = 0;
    }
    let result =
        tokio::time::timeout(Duration::from_secs(10), chat_complete(context, &mut NoMcp)).await;
    let socket_closed = tokio::time::timeout(Duration::from_secs(5), closure).await;
    server.abort();
    if let Err(error) = server.await {
        assert!(error.is_cancelled(), "fixture server failed: {error}");
    }
    socket_closed
        .expect("the generation idle timeout must close the socket")
        .expect("the peer must observe the close");
    let result = result.expect("the retained rejection must end the optional/irreducible phase");
    if cap_summary {
        let (reply, _, _, _) = result.expect("a failed optional summary retains the cap fallback");
        assert!(reply.contains("tool-round limit (0"), "{reply}");
        assert_eq!(reason, Some(crate::TurnEndReason::RoundCap));
    } else {
        let error = result.expect_err("the protected operator prompt cannot shrink further");
        assert_eq!(
            crate::retry::classify(&error),
            crate::retry::Retryability::ContextExceeded,
            "the body idle timeout must not replace observed rejection evidence: {error:#}"
        );
    }
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "a timed-out rejection must not be resent unchanged"
    );
    assert_eq!(requests[0]["stream"], true);
    let rejections = observation
        .behavior_signals
        .iter()
        .filter(|signal| {
            matches!(
                signal,
                observability::BehaviorSignal::ContextExceeded { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rejections.len(),
        1,
        "the observed rejection must be recorded"
    );
    assert!(matches!(
        rejections[0],
        observability::BehaviorSignal::ContextExceeded {
            projected_tokens: None,
            ..
        }
    ));
    assert!(state.calibration.ratio(None) >= 1.5);
}

#[tokio::test]
async fn primary_stream_context_rejection_survives_the_body_idle_timeout() {
    observed_context_error_survives_core_idle_timeout(false).await;
}

#[tokio::test]
async fn cap_summary_stream_context_rejection_survives_the_body_idle_timeout() {
    observed_context_error_survives_core_idle_timeout(true).await;
}

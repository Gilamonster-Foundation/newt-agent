//! Completion evidence for the strict consumer of the shared SSE parser.

use super::decode_response;
use serde_json::{json, Value};

fn sse(frames: &[Value], done: bool) -> Vec<u8> {
    let mut body = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>();
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    body.into_bytes()
}

#[test]
fn stream_assembles_indexed_tool_fragments_and_preserves_raw_usage() {
    let bytes = sse(
        &[
            json!({"id":"response-fixture","model":"served-model","choices":[{"index":0,"delta":{
            "role":"assistant","reasoning_content":"separate reasoning",
            "tool_calls":[
                {"index":1,"id":"call-b","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}},
                {"index":0,"id":"call-a","type":"function","function":{"name":"run_command","arguments":"{\"command\":"}}
            ]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"\"printf café\"}"}},
                {"index":1,"function":{"arguments":"\"fixture.rs\"}"}}
            ]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":6000,"prompt_tokens_details":{"cached_tokens":1000}}}),
        ],
        true,
    );
    let parsed = decode_response(&bytes).unwrap();
    let message = &parsed["choices"][0]["message"];
    assert_eq!(parsed["id"], "response-fixture");
    assert_eq!(parsed["model"], "served-model");
    assert_eq!(parsed["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(message["reasoning_content"], "separate reasoning");
    assert!(!message["content"]
        .as_str()
        .unwrap_or_default()
        .contains("reasoning"));
    assert_eq!(message["tool_calls"][0]["id"], "call-a");
    assert_eq!(message["tool_calls"][1]["id"], "call-b");
    assert_eq!(
        message["tool_calls"][0]["function"]["arguments"],
        "{\"command\":\"printf café\"}"
    );
    assert_eq!(
        message["tool_calls"][1]["function"]["arguments"],
        "{\"path\":\"fixture.rs\"}"
    );
    assert_eq!(
        parsed["usage"],
        json!({"prompt_tokens":6000,"prompt_tokens_details":{"cached_tokens":1000}})
    );
    assert!(parsed["usage"].get("completion_tokens").is_none());
}

#[test]
fn stream_accepts_omitted_and_blank_zero_argument_tool_calls() {
    for raw_arguments in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!("  \t")),
    ] {
        let mut function = json!({"name":"get_context_remaining"});
        if let Some(raw_arguments) = raw_arguments {
            function["arguments"] = raw_arguments;
        }
        let response = decode_response(&sse(
            &[
                json!({"choices":[{"delta":{"tool_calls":[{
                    "index":0,"id":"call-no-args","type":"function","function":function
                }]}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ],
            true,
        ))
        .expect("the shared tool gate accepts an empty argument object");
        assert_eq!(
            response["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
    }
}

#[test]
fn stream_refuses_cut_or_malformed_tool_arguments() {
    for (arguments, done) in [
        ("{\"command\":\"echo fixture\"}", false),
        ("{\"command\":", true),
    ] {
        let bytes = sse(
            &[
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-a","type":"function","function":{"name":"run_command","arguments":arguments}}]}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ],
            done,
        );
        assert!(
            decode_response(&bytes).is_err(),
            "an incomplete tool batch cannot authorize execution"
        );
    }
}

#[test]
fn stream_refuses_a_changed_call_identity_and_malformed_data_frame() {
    let changed = sse(
        &[
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"first","type":"function","function":{"name":"read_file","arguments":"{"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"replacement","function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}),
        ],
        true,
    );
    assert!(decode_response(&changed).is_err());
    let malformed = b"data: {broken JSON\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"later text\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    assert!(decode_response(malformed).is_err());
}

#[test]
fn stream_error_retains_context_evidence_but_quoted_error_text_is_normal_content() {
    let error = sse(
        &[json!({"error":{"message":"Context size has been exceeded."}})],
        false,
    );
    let error = decode_response(&error).unwrap_err();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded
    );
    let answer = sse(
        &[
            json!({"choices":[{"delta":{"content":"The log said Context size has been exceeded."},"finish_reason":"stop"}]}),
        ],
        true,
    );
    assert_eq!(
        decode_response(&answer).unwrap()["choices"][0]["message"]["content"],
        "The log said Context size has been exceeded."
    );
}

#[test]
fn complete_json_response_is_a_single_response_fallback() {
    let expected = json!({"model":"json-server","choices":[{"message":{"role":"assistant","content":"<think>reasoning</think>answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":150,"completion_tokens":2}});
    assert_eq!(
        decode_response(&serde_json::to_vec(&expected).unwrap()).unwrap(),
        expected
    );
}

#[test]
fn stream_preserves_reasoning_only_length_for_bounded_continuation() {
    let bytes = sse(
        &[json!({"choices":[{"delta":{"reasoning":"still thinking"},
        "finish_reason":"length"}]})],
        true,
    );
    let response = decode_response(&bytes).unwrap();
    assert_eq!(response["choices"][0]["finish_reason"], "length");
    assert_eq!(
        response["choices"][0]["message"]["reasoning_content"],
        "still thinking"
    );
    assert_eq!(response["choices"][0]["message"]["content"], "");
    assert!(response.get("usage").is_none());
    assert!(response.get("model").is_none());
    assert!(response.get("id").is_none());
}

#[test]
fn stream_refuses_unfinished_text_and_incomplete_tool_identity() {
    for frames in [
        vec![json!({"choices":[{"delta":{"content":"answer"}}]})],
        vec![json!({"choices":[],"usage":{"prompt_tokens":1}})],
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{
            "name":"read_file","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
        ],
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"one","function":{
            "arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
        ],
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"one","function":{
            "name":"read_file","arguments":"{}"}}]},"finish_reason":"length"}]}),
        ],
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"one","function":{
            "name":"read_file","arguments":"[]"}}]},"finish_reason":"tool_calls"}]}),
        ],
    ] {
        assert!(decode_response(&sse(&frames, true)).is_err());
    }
    assert!(decode_response(&sse(
        &[json!({"choices":[{"delta":{"content":"answer"},
        "finish_reason":"stop"}]})],
        false
    ))
    .is_err());
}

#[test]
fn stream_refuses_duplicate_ids_index_gaps_and_nonzero_choices() {
    for calls in [
        json!([{"index":1,"id":"one","function":{"name":"read_file","arguments":"{}"}}]),
        json!([{"id":"one","function":{"name":"read_file","arguments":"{}"}}]),
        json!([{"index":0,"id":"one","function":{"name":"read_file","arguments":"{}"}},
            {"index":1,"id":"one","function":{"name":"read_file","arguments":"{}"}}]),
    ] {
        assert!(decode_response(&sse(
            &[json!({"choices":[{"delta":{"tool_calls":calls},
            "finish_reason":"tool_calls"}]})],
            true
        ))
        .is_err());
    }
    assert!(decode_response(&sse(
        &[json!({"choices":[{"index":1,"delta":{"content":"answer"},
        "finish_reason":"stop"}]})],
        true
    ))
    .is_err());
}

#[test]
fn stream_refuses_identity_drift_and_post_finish_or_done_mutations() {
    for field in ["id", "model"] {
        let mut first = json!({"choices":[{"delta":{"content":"a"}}]});
        let mut last = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        first[field] = json!("first");
        last[field] = json!("replacement");
        assert!(decode_response(&sse(&[first, last], true)).is_err());
    }
    let finish = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
    let later = json!({"choices":[{"delta":{"content":"after completion"}}]});
    assert!(decode_response(&sse(&[finish.clone(), later.clone()], true)).is_err());
    let mut bytes = sse(&[finish], true);
    bytes.extend(sse(&[later], false));
    assert!(decode_response(&bytes).is_err());
}

#[test]
fn stream_error_event_and_transient_status_remain_distinct_from_quoted_reasoning() {
    let error = decode_response(
        b"event: error\ndata: {\"message\":\"Context size has been exceeded\"}\n\n",
    )
    .unwrap_err();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded
    );
    let error = decode_response(&sse(
        &[json!({"error":{"message":"busy","code":503}})],
        false,
    ))
    .unwrap_err();
    assert!(super::is_provider_error(&error));
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::Retry
    );
    let response = decode_response(&sse(&[json!({"choices":[{"delta":{
        "reasoning_content":"Explain the phrase Context size has been exceeded.","content":"answer"},
        "finish_reason":"stop"}]})], true)).unwrap();
    assert_eq!(response["choices"][0]["message"]["content"], "answer");
}

#[test]
fn stream_provider_error_marker_does_not_accept_parser_failures() {
    let error = decode_response(b"data: {invalid JSON\n\n").unwrap_err();
    assert!(!super::is_provider_error(&error));
    let error = decode_response(&sse(
        &[json!({"error":{"message":"busy","code":503}})],
        false,
    ))
    .unwrap_err()
    .context("body read failed afterward");
    assert!(super::is_provider_error(&error));
}

#[test]
fn complete_json_error_envelope_is_not_a_successful_json_fallback() {
    let error =
        decode_response(br#"{"error":{"message":"Context size has been exceeded"}}"#).unwrap_err();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded
    );
}

#[test]
fn stream_invalid_utf8_refuses_completion_but_keeps_an_observed_error() {
    let mut bytes = sse(
        &[json!({"choices":[{"delta":{"content":"answer"},
        "finish_reason":"stop"}]})],
        true,
    );
    bytes.push(0xff);
    assert!(decode_response(&bytes).is_err());
    let mut bytes = sse(
        &[json!({"error":{"message":"Context size has been exceeded"}})],
        false,
    );
    bytes.push(0xff);
    let error = decode_response(&bytes).unwrap_err();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded
    );
    let mut bytes = b"data: {broken JSON\n\n".to_vec();
    bytes.extend(sse(
        &[json!({"error":{"message":"Context size has been exceeded"}})],
        false,
    ));
    let error = decode_response(&bytes).unwrap_err();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded
    );
}

//! Live probe for tool-call wire format — only runs when `NEWT_LIVE_TEST=1`.
//!
//! Endpoint and model are supplied by the operator; this file names no host and
//! no vendor, so a public checkout reveals nothing about where anyone runs
//! inference.
//!
//! Sends a one-shot POST to the configured inference endpoint with a tool
//! definition that contains a hyphenated namespaced name to determine whether
//! the backend proxy normalises hyphens to underscores in tool names.
//!
//! Configure via environment variables:
//!   NEWT_LIVE_TEST=1            — required to enable
//!   NEWT_LIVE_ENDPOINT=<url>    — inference base URL (default: reads ~/.newt/config)
//!   NEWT_LIVE_MODEL=<model-id>  — model to probe (default: reads ~/.newt/config)
//!
//! Run with:
//!   NEWT_LIVE_TEST=1 cargo test -p newt-inference --test live_tool_probe -- --nocapture

#[tokio::test]
async fn probe_tool_call_wire_format() {
    if std::env::var("NEWT_LIVE_TEST").unwrap_or_default() != "1" {
        eprintln!("skipped — set NEWT_LIVE_TEST=1 to run live probe");
        return;
    }

    // Read API key from ~/.newt/token (same path newt itself uses).
    let token_path = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".newt/token");
    let api_key = std::fs::read_to_string(&token_path)
        .unwrap_or_else(|e| panic!("could not read ~/.newt/token: {e}"))
        .trim()
        .to_string();

    // No defaults: a hardcoded fallback is how an internal hostname ends up in
    // a public repo. Absent configuration skips the probe instead.
    let (Ok(endpoint), Ok(model)) = (
        std::env::var("NEWT_LIVE_ENDPOINT"),
        std::env::var("NEWT_LIVE_MODEL"),
    ) else {
        eprintln!("skipped — set NEWT_LIVE_ENDPOINT and NEWT_LIVE_MODEL");
        return;
    };

    let url = format!("{endpoint}/v1/chat/completions");

    // Two tool definitions — one with hyphens in the server prefix, one without.
    // If the proxy normalises hyphens the model will call back with underscores.
    let body = serde_json::json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": "Please call hyphenated-server__probe_tool and then plain_server__probe_tool."
        }],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "hyphenated-server__probe_tool",
                    "description": "Probe tool for a server with a hyphenated name.",
                    "parameters": { "type": "object", "properties": {} }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "plain_server__probe_tool",
                    "description": "Probe tool for a server without hyphens.",
                    "parameters": { "type": "object", "properties": {} }
                }
            }
        ],
        "tool_choice": "required",
        "max_tokens": 200
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
        .expect("HTTP request failed");

    let status = resp.status();
    let raw_body = resp.text().await.expect("could not read response body");

    eprintln!("=== HTTP {status} ===");
    eprintln!("{raw_body}");

    assert!(
        status.is_success(),
        "unexpected HTTP status {status}: {raw_body}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&raw_body).expect("response body is not valid JSON");

    let tool_calls = json["choices"][0]["message"]["tool_calls"].as_array();
    match tool_calls {
        None => {
            eprintln!("=== NO tool_calls ARRAY in response ===");
            eprintln!("Full message: {}", json["choices"][0]["message"]);
            panic!("no tool_calls in response — model did not call a tool");
        }
        Some(tcs) => {
            eprintln!("=== {} tool_call(s) ===", tcs.len());
            for (i, tc) in tcs.iter().enumerate() {
                eprintln!("--- tool_calls[{i}] raw ---");
                eprintln!("{}", serde_json::to_string_pretty(tc).unwrap());
                eprintln!("--- tool_calls[{i}] parsed ---");
                eprintln!("  function.name  = {:?}", tc["function"]["name"].as_str());
                eprintln!("  name (top)     = {:?}", tc["name"].as_str());
                eprintln!("  function key present = {}", !tc["function"].is_null());
            }
        }
    }
}

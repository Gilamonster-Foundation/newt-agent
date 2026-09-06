use super::*;

// --- R1: BATCH-level atomic tool-call validation (invariant #3) ---

/// Run a batch through the gate and DISPATCH (count) only on Ok — the honest
/// invocation-counting model of the real loop's two phases. Returns the number
/// of tools that would run; a rejected batch runs ZERO.
fn dispatched_count(
    calls: &[(Option<&str>, Option<&str>, &serde_json::Value)],
    require_call_id: bool,
) -> usize {
    match validate_tool_call_batch(calls, require_call_id) {
        Ok(validated) => validated.len(), // phase 2 executes each; count == invocations
        Err(_) => 0,                      // phase 1 rejected → zero executes
    }
}

#[test]
fn batch_valid_then_malformed_dispatches_zero() {
    let a = serde_json::json!("{\"op\":\"status\"}");
    let bad = serde_json::json!("{\"op\": "); // truncated JSON
    let calls = [
        (Some("id1"), Some("git"), &a),
        (Some("id2"), Some("write_file"), &bad),
    ];
    assert_eq!(
        dispatched_count(&calls, true),
        0,
        "a malformed sibling rejects the whole batch — the valid mutating call must NOT run first"
    );
}

#[test]
fn batch_malformed_then_valid_dispatches_zero() {
    let bad = serde_json::json!("not json");
    let b = serde_json::json!("{}");
    let calls = [
        (Some("id1"), Some("git"), &bad),
        (Some("id2"), Some("list_dir"), &b),
    ];
    assert_eq!(dispatched_count(&calls, true), 0);
}

#[test]
fn batch_missing_call_id_dispatches_zero_when_required() {
    let a = serde_json::json!("{}");
    let calls = [(None, Some("git"), &a)];
    assert_eq!(dispatched_count(&calls, true), 0);
    // ...but the id-less Ollama wire (require_call_id=false) accepts it.
    assert_eq!(dispatched_count(&calls, false), 1);
}

#[test]
fn batch_duplicate_call_ids_dispatch_zero() {
    let a = serde_json::json!("{}");
    let calls = [
        (Some("dup"), Some("git"), &a),
        (Some("dup"), Some("list_dir"), &a),
    ];
    assert_eq!(
        dispatched_count(&calls, true),
        0,
        "duplicate ids mis-correlate results — reject the batch"
    );
}

#[test]
fn batch_malformed_argument_json_dispatches_zero() {
    let bad = serde_json::json!("{\"path\": \"a"); // truncated
    let calls = [(Some("id1"), Some("write_file"), &bad)];
    assert_eq!(dispatched_count(&calls, true), 0);
}

#[test]
fn batch_all_valid_dispatches_every_call() {
    let a = serde_json::json!("{\"op\":\"status\"}");
    let b = serde_json::json!(serde_json::json!({"path": "x"})); // object value
    let c = serde_json::Value::Null; // no-arg tool
    let calls = [
        (Some("id1"), Some("git"), &a),
        (Some("id2"), Some("write_file"), &b),
        (Some("id3"), Some("list_dir"), &c),
    ];
    let out = validate_tool_call_batch(&calls, true).expect("all valid");
    assert_eq!(out.len(), 3);
    assert_eq!(
        out.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
        vec!["git", "write_file", "list_dir"]
    );
    assert_eq!(out[0].call_id, "id1");
}

// The rejection CLASS decides recovery: a correlation problem is
// unrecoverable (caller aborts); a content problem is recoverable (caller may
// echo a keyed rejection and re-dispatch).

#[test]
fn batch_missing_id_is_correlation_impossible() {
    let a = serde_json::json!("{}");
    let calls = [(None, Some("git"), &a)];
    assert!(matches!(
        validate_tool_call_batch(&calls, true),
        Err(BatchRejection::CorrelationImpossible(_))
    ));
}

#[test]
fn batch_blank_id_is_correlation_impossible() {
    // #1526 review: a present-but-blank/whitespace id cannot correlate a
    // function_call_output any more than a missing one — reject the batch.
    let a = serde_json::json!("{}");
    let calls = [(Some("   "), Some("git"), &a)];
    assert!(matches!(
        validate_tool_call_batch(&calls, true),
        Err(BatchRejection::CorrelationImpossible(_))
    ));
}

#[test]
fn batch_duplicate_id_is_correlation_impossible() {
    let a = serde_json::json!("{}");
    let calls = [
        (Some("dup"), Some("git"), &a),
        (Some("dup"), Some("list_dir"), &a),
    ];
    assert!(matches!(
        validate_tool_call_batch(&calls, true),
        Err(BatchRejection::CorrelationImpossible(_))
    ));
}

#[test]
fn batch_bad_args_with_valid_ids_is_content_invalid() {
    // ids are present + unique → correlation is fine; the failure is content.
    let bad = serde_json::json!("not json");
    let calls = [(Some("id1"), Some("git"), &bad)];
    assert!(matches!(
        validate_tool_call_batch(&calls, true),
        Err(BatchRejection::ContentInvalid(_))
    ));
}

// --- per-call validator (still used by the batch gate) ---

#[test]
fn validate_accepts_a_string_encoded_object() {
    let (name, args) = validate_tool_call(
        Some("write_file"),
        &serde_json::json!("{\"path\":\"a.txt\"}"),
    )
    .expect("valid");
    assert_eq!(name, "write_file");
    assert_eq!(args["path"], "a.txt");
}

#[test]
fn validate_accepts_an_object_value_directly() {
    let (name, args) =
        validate_tool_call(Some("git"), &serde_json::json!({"op": "status"})).expect("valid");
    assert_eq!(name, "git");
    assert_eq!(args["op"], "status");
}

#[test]
fn validate_treats_absent_or_empty_arguments_as_no_args() {
    // A no-arg tool: null, absent, and "" all mean an empty object — valid.
    for raw in [
        serde_json::Value::Null,
        serde_json::json!(""),
        serde_json::json!("   "),
    ] {
        let (_, args) = validate_tool_call(Some("list_dir"), &raw).expect("valid no-args");
        assert_eq!(args, serde_json::json!({}), "raw={raw:?}");
    }
}

#[test]
fn validate_rejects_unparseable_arguments_instead_of_coercing_to_null() {
    // The core bug this closes: a truncated/garbled args string used to become
    // `null` and execute anyway. It must now be rejected.
    let err = validate_tool_call(Some("write_file"), &serde_json::json!("{\"path\": \"a"))
        .expect_err("truncated JSON must be rejected");
    assert!(err.contains("not valid JSON"), "got: {err}");
    assert!(err.contains("write_file"), "names the tool: {err}");
}

#[test]
fn validate_rejects_non_object_json_arguments() {
    // A JSON scalar or array is not a tool-args object.
    for raw in [serde_json::json!("[1,2,3]"), serde_json::json!("\"bare\"")] {
        let err =
            validate_tool_call(Some("git"), &raw).expect_err("non-object args must be rejected");
        assert!(
            err.contains("must be a JSON object"),
            "raw={raw:?} got: {err}"
        );
    }
    // ...and a live (already-parsed) non-object value is rejected too.
    assert!(validate_tool_call(Some("git"), &serde_json::json!(42)).is_err());
}

#[test]
fn validate_rejects_a_missing_or_blank_name() {
    assert!(validate_tool_call(None, &serde_json::json!({})).is_err());
    assert!(validate_tool_call(Some(""), &serde_json::json!({})).is_err());
    assert!(validate_tool_call(Some("   "), &serde_json::json!({})).is_err());
}

#[test]
fn malformed_calls_are_never_dispatched_invocation_count_is_zero() {
    // The atomic guarantee, as an invocation-counting proof: run a batch of
    // calls through the ONE validation gate, dispatching (incrementing the
    // counter) ONLY on a valid `(name, args)`. Every malformed call must yield
    // ZERO dispatches — no tool is ever invoked on garbage.
    let batch = vec![
        // valid
        serde_json::json!({"name": "git", "arguments": "{\"op\":\"status\"}"}),
        // malformed: truncated JSON args (the historical null-coercion bug)
        serde_json::json!({"name": "write_file", "arguments": "{\"path\": \"a"}),
        // malformed: missing name
        serde_json::json!({"arguments": "{}"}),
        // valid: no-arg tool
        serde_json::json!({"name": "list_dir"}),
        // malformed: non-object args
        serde_json::json!({"name": "git", "arguments": "[1,2]"}),
    ];

    let mut invocations = 0usize;
    let mut dispatched_names = Vec::new();
    for call in &batch {
        match validate_tool_call(call["name"].as_str(), &call["arguments"]) {
            Ok((name, _args)) => {
                // The ONLY path that reaches a tool.
                invocations += 1;
                dispatched_names.push(name);
            }
            Err(_reason) => {
                // Malformed → echoed back, never dispatched. No side effect.
            }
        }
    }

    assert_eq!(
        invocations, 2,
        "exactly the two well-formed calls dispatch; the three malformed ones invoke nothing"
    );
    assert_eq!(dispatched_names, vec!["git", "list_dir"]);
}

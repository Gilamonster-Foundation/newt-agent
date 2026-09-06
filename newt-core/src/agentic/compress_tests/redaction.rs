use super::*;

use super::test_support::{assistant_call, sys, tool_result, user, EST};
use serde_json::json;

/// disclosure-gate-live-path (#5): the observation / compaction / spill
/// memory paths funnel through `redact_secrets`, which now also runs the
/// by-VALUE session filter. A high-entropy registered secret (which the
/// shape-only regexes would MISS) must not survive into model-context memory.
#[test]
fn redact_secrets_value_filters_a_registered_session_secret() {
    let canary = "NEWT-CANARY-9f3a2b7c1d4e";
    // Baseline: with no session filter installed, the canary passes through
    // (the shape-only patterns don't recognise it) — proving the value gate,
    // not a coincidental regex, is what redacts it below.
    assert!(redact_secrets(&format!("observed: {canary}")).contains(canary));

    let mut f = crate::ocap::DisclosureFilter::new();
    f.register(canary);
    let _g = crate::ocap::scoped_session_disclosure(f);
    let out = redact_secrets(&format!("observed: {canary} at end"));
    assert!(
        !out.contains(canary),
        "a registered session secret must be value-filtered from the memory path: {out}"
    );
}

#[tokio::test]
async fn compaction_store_captures_redacted_span_and_names_the_handle() {
    use crate::agentic::content_spill::{SessionSpillStore, SpillCid, SpillStore};
    // #661 group B: with a compaction store, the evicted middle is stored
    // (redacted) and the marker names a `compaction:<cid>` retrieval handle —
    // progressive disclosure. A secret in the middle is redacted on store.
    let compaction = SessionSpillStore::new([7u8; 16]);
    let mut msgs = vec![sys("sys"), user("task")];
    // An early-middle message carrying a secret — it will be evicted + stored.
    msgs.push(user("config api_key=9f8e7d6c5b4a32100ffee and more"));
    for i in 0..24 {
        msgs.push(user(&format!("middle note {i} {}", "m".repeat(200))));
    }
    msgs.push(user("recent tail"));
    let mut state = CompressState::new();
    let out = compress(
        CompressRequest {
            rewrites_history: true,
            messages: &msgs,
            budget: 300,
            max_messages: None,
            replay_protected_tail_len: 0,
            task: "task",
            hard_budget: true,
            authoritative: true,
            focus: None,
            est: EST,
            summary_input_cap_floor_chars: 8_192,
            compaction_store: Some(&compaction),
            compaction_stage: None,
        },
        None, // no summarizer → static marker; the handle still rides
        &mut state,
    )
    .await;
    assert!(out.fired);
    // The marker names a `compaction:<cid>` content handle (not a literal s0) so
    // the model can fault the span in. Extract it, confirm it parses, and confirm
    // it resolves in the store to the redacted verbatim span.
    let marker = out
        .messages
        .iter()
        .find_map(|m| m["content"].as_str().filter(|c| c.contains("compaction:")))
        .expect("the marker must name the compaction handle");
    let handle = marker
        .split("compaction:")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("handle present in the marker");
    let cid = SpillCid::parse(handle).expect("handle is a canonical CID");
    // The store holds the verbatim span — with the secret REDACTED on store.
    let span = compaction
        .fetch(&cid)
        .expect("span must be stored")
        .redacted_text;
    assert!(
        !span.contains("9f8e7d6c5b4a32100ffee"),
        "the secret must be redacted before store: {span}"
    );
    assert!(
        span.contains("[REDACTED]"),
        "redaction marker present: {span}"
    );
}

// -- redaction ----------------------------------------------------------------

#[test]
fn redaction_catches_true_positives() {
    let cases = [
        (
            "the key is sk-AbCdEf1234567890AbCdEf1234567890",
            "sk-AbCdEf",
        ),
        ("ghp_AbCdEf1234567890AbCdEf1234567890", "ghp_"),
        ("github_pat_11ABCDEFG0123456789_abcdefghij", "github_pat_"),
        ("aws id AKIAIOSFODNN7EXAMPLE", "AKIAIOSFODNN7"),
        (
            "Authorization: Bearer abc.def-ghi_jkl012345678901234567890",
            "abc.def-ghi",
        ),
        ("api_key=9f8e7d6c5b4a32100ffee", "9f8e7d6c"),
        ("password: \"hunter2hunter2\"", "hunter2hunter2"),
        (
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc123def456",
            "eyJhbGci",
        ),
    ];
    for (input, leaked) in cases {
        let out = redact_secrets(input);
        assert!(
            !out.contains(leaked),
            "secret fragment {leaked:?} survived: {out}"
        );
        assert!(out.contains("[REDACTED]"), "no redaction marker: {out}");
    }
    // A private key block, including an unterminated one.
    let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow…\n-----END RSA PRIVATE KEY-----";
    assert!(!redact_secrets(key).contains("MIIEow"));
    let cut = "-----BEGIN PRIVATE KEY-----\nMIIEow… (truncated)";
    assert!(!redact_secrets(cut).contains("MIIEow"));
}

#[test]
fn redaction_passes_benign_near_misses() {
    let benign = [
        "the api key is stored in the system keychain",
        "the token budget is 4096 tokens per request",
        "Bearer of good news: the build is green",
        "sk-test was rejected (too short to be a real key)",
        "set password: yes in sshd_config",
        "AKIAFOO is not a full key id",
        "ghp_short",
        "the access_token field is documented in docs/api.md",
        "run `cargo test -p newt-core` and check the password prompt",
    ];
    for input in benign {
        let out = redact_secrets(input);
        assert_eq!(out, input, "benign text must pass unchanged");
    }
}

#[test]
fn redaction_applies_inside_the_summary_request() {
    let middle = vec![tool_result(
        "config: api_key=9f8e7d6c5b4a32100ffee and more text",
    )];
    let request = redact_secrets(&summary_request(
        "the task",
        &middle,
        usize::MAX,
        None,
        ConvShape::Coding,
    ));
    assert!(!request.contains("9f8e7d6c5b4a32100ffee"), "{request}");
    assert!(request.contains("api_key=[REDACTED]"), "{request}");
    assert!(request.contains("the task"), "task still present verbatim");
}

/// F6: tool-call args reach the summarizer rendered AS JSON — the
/// quoted-key credential shape must redact.
#[test]
fn redaction_catches_json_quoted_credential_keys() {
    let cases = [
        (r#"{"api_key": "9f8e7d6c5b4a32100ffee"}"#, "9f8e7d6c"),
        (r#"{"password": "hunter2hunter2"}"#, "hunter2hunter2"),
        (
            r#"body: "client_secret": "abcd1234efgh5678ijkl""#,
            "abcd1234",
        ),
    ];
    for (input, leaked) in cases {
        let out = redact_secrets(input);
        assert!(
            !out.contains(leaked),
            "secret fragment {leaked:?} survived: {out}"
        );
        assert!(out.contains("[REDACTED]"), "no redaction marker: {out}");
    }
}

/// N4: redaction runs BEFORE excerpting — a credential the excerpt cap
/// would slice mid-value must not leak a fragment too short for any
/// pattern to match afterward.
#[test]
fn redaction_survives_excerpt_truncation() {
    let secret = "sk-AbCdEf1234567890AbCdEf1234567890";
    // The serialized args put the secret astride the 200-char arg cap:
    // unredacted it would be cut to an unmatchable `sk-…` fragment.
    let args = json!({
        "command": format!("{} && export OPENAI_API_KEY={secret}", "x".repeat(140))
    });
    let m = assistant_call("run_command", args);
    let line = render_message(&m);
    assert!(!line.contains("sk-AbC"), "{line}");
    assert!(!line.contains("AbCdEf123"), "no fragment may leak: {line}");
    assert!(line.contains("[REDACTED]"), "{line}");
}

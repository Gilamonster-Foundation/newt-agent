use super::*;

#[tokio::test]
async fn state_tools_dispatch_only_with_a_store() {
    use crate::agentic::scratchpad::{ScratchpadStore, SessionScratchpadStore};
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "key": "k", "value": "v" });
    // Step 26.4: without a store the tool was never advertised → unknown.
    let none = execute_tool(
        "state_set",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(none.starts_with("unknown tool: state_set"), "{none}");
    // With a store → routes to the executor and mutates it.
    let store = SessionScratchpadStore::default();
    let set = execute_tool(
        "state_set",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&store as &dyn ScratchpadStore),
        None,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(set, "stored: k");
    assert_eq!(store.get("k").as_deref(), Some("v"));
}

#[tokio::test]
async fn code_search_dispatch_only_with_a_searcher() {
    use crate::agentic::semantic::{CodeSearch, Embedder, SessionSemanticIndex};
    struct E;
    #[async_trait::async_trait]
    impl Embedder for E {
        async fn embed(&self, _t: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![1.0])
        }
    }
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "query": "find it" });
    // Step 26.5.5: no searcher → unknown tool (presence-gate parity).
    let none = execute_tool(
        "code_search",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(none.starts_with("unknown tool: code_search"), "{none}");
    // with a searcher (empty index) → routes to the executor (labelled no-match).
    let idx = SessionSemanticIndex::default();
    let search = CodeSearch {
        embedder: &E,
        index: &idx,
        top_k: 1,
        steer: None,
        status: None,
    };
    let out = execute_tool(
        "code_search",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(search),
        None,
        None,
        None,
    )
    .await;
    assert!(out.contains("no code matched"), "{out}");
}

#[tokio::test]
async fn experiential_dispatch_only_with_a_store() {
    use crate::agentic::experiential::{ExperienceStore, SessionExperienceStore};
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({
        "task": "ci flake", "outcome": "fixed", "lesson": "pin the seed for the fuzz test"
    });
    // Step 26.6a: no store → unknown tool for BOTH arms (presence-gate parity).
    for name in ["experience_record", "experience_recall"] {
        let out = execute_tool(
            name, &args, ".", false, 20, &caveats, &mut NoMcp, None, None, None, None, None, None,
            None, None, None, None, None, None, None,
        )
        .await;
        assert!(out.starts_with(&format!("unknown tool: {name}")), "{out}");
    }
    // with a store → record routes to the executor and mutates it.
    let store = SessionExperienceStore::default();
    let out = execute_tool(
        "experience_record",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&store as &dyn ExperienceStore),
        None,
    )
    .await;
    assert_eq!(out, "recorded experience");
    assert_eq!(store.count(), 1);
}

#[tokio::test]
async fn scheduled_dispatch_only_with_a_ledger() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "plan": [
            { "step": "a", "status": "in_progress" },
            { "step": "b", "status": "pending" },
        ] });
    // Step 26.6b / #716 / #715 PR2: no ledger → unknown tool for ALL plan arms
    // (presence-gate parity, including the read-only plan_get).
    for name in ["update_plan", "plan_get"] {
        let out = execute_tool(
            name, &args, ".", false, 20, &caveats, &mut NoMcp, None, None, None, None, None, None,
            None, None, None, None, None, None, None,
        )
        .await;
        assert!(out.starts_with(&format!("unknown tool: {name}")), "{out}");
    }
    // with a ledger → update_plan routes to the executor and mutates it.
    let ledger = SessionStepLedger::default();
    let out = execute_tool(
        "update_plan",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert!(out.starts_with("<plan>\n"), "{out}");
    assert_eq!(ledger.count(), 2);
    // #716: plan_get with a ledger renders the <plan> block, read-only.
    let got = execute_tool(
        "plan_get",
        &serde_json::json!({}),
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert!(got.starts_with("<plan>\n"), "{got}");
    assert_eq!(ledger.count(), 2, "plan_get is read-only");
}

#[tokio::test]
async fn resume_context_dispatch_degrades_without_a_recall_source() {
    // #714: advertised ALWAYS, so dispatch never reports "unknown tool" —
    // with no recall_source (headless) it returns the clear no-history line.
    let caveats = crate::caveats::Caveats::top();
    let out = execute_tool(
        "resume_context",
        &serde_json::json!({}),
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None, // build_check_cmd
        None, // note_sink
        None, // recall_source
        None, // memory_source
        None, // permission_gate
        None, // exec_floor
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert!(
        out.contains("no conversation history available this session"),
        "{out}"
    );
    assert!(!out.starts_with("unknown tool"), "{out}");
}

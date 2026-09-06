use super::*;

// --- #1528 B3 transactional helper unit tests (B3-CG-004/005) ---

fn assistant(i: usize, chars: usize) -> serde_json::Value {
    serde_json::json!({"role": "assistant", "content": format!("step {i}: {}", "w ".repeat(chars))})
}

/// B3-CG-004: a candidate compaction REJECTED by the post-bridge budget guard
/// commits NOTHING — the live `input` and `CompressState` are untouched and no
/// committed notice is emitted (the helper returns before the commit block). The
/// typed `OverBudgetAfterFence` carries the reason for the caller's error chain.
/// Fails on `711c247` (non-transactional: notice + state mutated before the check).
#[tokio::test]
async fn compact_responses_input_post_fence_overflow_is_transactional() {
    use crate::agentic::compress::{CompressState, Summarizer};
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    let summ: Summarizer = Box::new(move |_r: String| {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok("a short summary".to_string()) })
    });
    let mut input = vec![serde_json::json!({"role": "user", "content": "the task"})];
    for i in 0..6 {
        input.push(assistant(i, 300));
    }
    input.push(serde_json::json!({"role": "user", "content": "recent turn"}));
    let original = input.clone();
    let mut state = CompressState::new();

    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(10), // actionable_budget: tiny → the fenced rebuild overflows it
        400,      // compaction_budget: generous enough that compress FIRES
        1.0,
        crate::tokens::TokenEstimation::default(),
        "the task",
        8_192,
        true,
        None,
        Some(&*summ),
        &mut state,
        false,
    )
    .await;

    assert!(
        matches!(outcome, ResponsesCompaction::OverBudgetAfterFence(_)),
        "expected a post-fence overflow rejection"
    );
    assert_eq!(
        input, original,
        "TRANSACTIONAL: a rejected candidate leaves input UNCHANGED"
    );
    assert_eq!(
        state.counters().compressions,
        0,
        "TRANSACTIONAL: live CompressState attempts UNCHANGED (compaction ran on a clone)"
    );
    assert!(
        !state.is_disabled(),
        "TRANSACTIONAL: disabled latch UNCHANGED"
    );
    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the candidate compaction actually ran (summarizer invoked)"
    );
}

/// B3-CG-004: a forbidden `system` item fails classification (BridgeError) with no
/// compaction and no side effects.
#[tokio::test]
async fn compact_responses_input_bridge_error_is_transactional() {
    use crate::agentic::compress::CompressState;
    let mut input = vec![
        serde_json::json!({"role": "user", "content": "task"}),
        serde_json::json!({"role": "system", "content": "smuggled"}),
    ];
    let original = input.clone();
    let mut state = CompressState::new();
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(5),
        5,
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        None,
        None,
        &mut state,
        false,
    )
    .await;
    assert!(matches!(outcome, ResponsesCompaction::BridgeError));
    assert_eq!(
        input, original,
        "transactional: input unchanged on bridge error"
    );
    assert_eq!(state.counters().compressions, 0, "no compaction ran");
}

/// B3-CG-004: a compressor refusal (protected head alone exceeds the target)
/// commits nothing.
#[tokio::test]
async fn compact_responses_input_refusal_is_transactional() {
    use crate::agentic::compress::CompressState;
    let mut input = vec![serde_json::json!({"role": "user", "content": "x".repeat(4_000)})];
    for i in 0..3 {
        input.push(assistant(i, 10));
    }
    let original = input.clone();
    let mut state = CompressState::new();
    // compaction_budget = 1 → the protected head alone exceeds it → refuse.
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt with a large protected head that cannot shrink"),
        None,
        Some(1),
        1,
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        None,
        None,
        &mut state,
        false,
    )
    .await;
    assert!(
        matches!(
            outcome,
            ResponsesCompaction::Refused | ResponsesCompaction::NotFired
        ),
        "an irreducible tiny budget refuses / makes no progress"
    );
    assert_eq!(input, original, "transactional: input unchanged on refusal");
    assert_eq!(
        state.counters().compressions,
        0,
        "transactional: state unchanged"
    );
}

/// B3-CG-004: a candidate that fits COMMITS all three effects — input rewritten
/// to the compacted form, the anti-thrash attempt recorded, and (structurally,
/// after this point) the notice emitted.
#[tokio::test]
async fn compact_responses_input_commits_only_on_success() {
    use crate::agentic::compress::{CompressState, Summarizer};
    let summ: Summarizer =
        Box::new(|_r: String| Box::pin(async { Ok("brief summary".to_string()) }));
    let mut input = vec![serde_json::json!({"role": "user", "content": "task"})];
    for i in 0..6 {
        input.push(assistant(i, 300));
    }
    input.push(serde_json::json!({"role": "user", "content": "recent turn"}));
    let before_len = input.len();
    let mut state = CompressState::new();
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(100_000), // generous actionable budget → the compacted form fits
        400,           // tight compaction target → compress fires
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        None,
        Some(&*summ),
        &mut state,
        false,
    )
    .await;
    assert!(
        matches!(outcome, ResponsesCompaction::Compacted),
        "a fitting compaction commits"
    );
    assert!(
        input.len() < before_len,
        "committed: input rewritten to fewer items ({} < {before_len})",
        input.len()
    );
    assert!(
        input.iter().any(|m| m["content"]
            .as_str()
            .unwrap_or("")
            .contains("newt-compaction-summary")),
        "committed: the reference-summary envelope is present"
    );
    assert_eq!(
        state.counters().compressions,
        1,
        "committed: the anti-thrash attempt IS recorded on the live state"
    );
}

/// B3-CG-004 / §2.6: the FOURTH effect — the session compaction/spill store — is
/// also transactional. A rejected candidate writes NOTHING to the live store
/// (rejected-candidate-publishes-nothing); a committed one flushes exactly its
/// staged span. Fails on `711c247`, where the shared `store.store(...)` inside
/// `compress` was not rolled back on reject (leaking an orphaned redacted span per
/// rejected proactive attempt).
#[tokio::test]
async fn compact_responses_input_spill_store_is_transactional() {
    use crate::agentic::compress::{CompressState, Summarizer};
    use crate::agentic::content_spill::{SessionSpillStore, SpillStore};
    let make_input = || {
        let mut v = vec![serde_json::json!({"role": "user", "content": "task"})];
        for i in 0..6 {
            v.push(assistant(i, 300));
        }
        v.push(serde_json::json!({"role": "user", "content": "recent turn"}));
        v
    };
    let summarizer =
        || -> Summarizer { Box::new(|_r: String| Box::pin(async { Ok("brief".to_string()) })) };

    // REJECT (tiny actionable budget → post-fence overflow): store stays EMPTY.
    {
        let store = SessionSpillStore::new([7u8; 16]);
        let s = summarizer();
        let mut input = make_input();
        let mut state = CompressState::new();
        let outcome = compact_responses_input(
            &mut input,
            Some("you are newt"),
            None,
            Some(10),
            400,
            1.0,
            crate::tokens::TokenEstimation::default(),
            "task",
            8_192,
            true,
            Some(&store),
            Some(&*s),
            &mut state,
            false,
        )
        .await;
        assert!(matches!(
            outcome,
            ResponsesCompaction::OverBudgetAfterFence(_)
        ));
        assert_eq!(
            store.unique_objects(),
            0,
            "TRANSACTIONAL: a rejected candidate writes NO committed spill"
        );
        assert_eq!(
            store.logical_spill_refs(),
            0,
            "a rejected candidate installs no logical reference either"
        );
        assert_eq!(input, make_input(), "live input is UNCHANGED on reject");
        assert!(
            !serde_json::to_string(&input)
                .unwrap()
                .contains("compaction:"),
            "no retrieval marker leaked into live input"
        );
    }
    // COMMIT (generous budget): the staged span is flushed exactly once.
    {
        let store = SessionSpillStore::new([7u8; 16]);
        let s = summarizer();
        let mut input = make_input();
        let mut state = CompressState::new();
        let outcome = compact_responses_input(
            &mut input,
            Some("you are newt"),
            None,
            Some(100_000),
            400,
            1.0,
            crate::tokens::TokenEstimation::default(),
            "task",
            8_192,
            true,
            Some(&store),
            Some(&*s),
            &mut state,
            false,
        )
        .await;
        assert!(matches!(outcome, ResponsesCompaction::Compacted));
        assert_eq!(
            store.unique_objects(),
            1,
            "committed: the compacted span is flushed to the store exactly once"
        );
        // The live input names the committed span's `compaction:<cid>` handle.
        assert!(
            serde_json::to_string(&input)
                .unwrap()
                .contains("compaction:"),
            "the committed candidate names its retrieval handle"
        );
    }
}

fn spill_middle_input() -> Vec<serde_json::Value> {
    let mut v = vec![serde_json::json!({"role": "user", "content": "task"})];
    for i in 0..6 {
        v.push(assistant(i, 300));
    }
    v.push(serde_json::json!({"role": "user", "content": "recent turn"}));
    v
}

/// Correction 1: with NO real compaction store, a successful compaction still
/// summarizes but promises NO retrieval — no `memory_fetch("compaction:...")`
/// handle. Fails on `8b3a1c8`, which wrapped a `None` store and invented
/// `compaction:s0` (a phantom, unresolvable handle).
#[tokio::test]
async fn compact_responses_input_no_store_emits_no_retrieval_handle() {
    use crate::agentic::compress::Summarizer;
    let summ: Summarizer = Box::new(|_r: String| Box::pin(async { Ok("brief".to_string()) }));
    let mut input = spill_middle_input();
    let mut state = crate::agentic::compress::CompressState::new();
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(100_000),
        400,
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        None, // NO real compaction store
        Some(&*summ),
        &mut state,
        false,
    )
    .await;
    assert!(matches!(outcome, ResponsesCompaction::Compacted));
    let text = serde_json::to_string(&input).unwrap();
    assert!(
        text.contains("newt-compaction-summary"),
        "it still compacted"
    );
    assert!(
        !text.contains("compaction:"),
        "no retrieval handle promised without a store: {text:.200}"
    );
}

/// §2.6 (replaces the obsolete "store-issued id" correction): a committed
/// compaction names a `compaction:<cid>` CONTENT handle — not a predicted or
/// allocated id — and that handle parses as a canonical CID AND resolves in the
/// live store to the committed verbatim span. Content addressing dissolved the
/// allocator, so there is no id to predict or steal.
#[tokio::test]
async fn compact_responses_input_names_a_resolvable_content_handle() {
    use crate::agentic::compress::Summarizer;
    use crate::agentic::content_spill::{SessionSpillStore, SpillCid, SpillStore};
    let store = SessionSpillStore::new([7u8; 16]);
    let summ: Summarizer = Box::new(|_r: String| Box::pin(async { Ok("brief".to_string()) }));
    let mut input = spill_middle_input();
    let mut state = crate::agentic::compress::CompressState::new();
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(100_000),
        400,
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        Some(&store),
        Some(&*summ),
        &mut state,
        false,
    )
    .await;
    assert!(matches!(outcome, ResponsesCompaction::Compacted));
    let text = serde_json::to_string(&input).unwrap();
    // Extract the `compaction:<cid>` handle (a base32-lower CID — ascii
    // alphanumeric, so read up to the first non-alphanumeric terminator).
    let handle: String = text
        .split("compaction:")
        .nth(1)
        .expect("the marker names a compaction handle")
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let cid = SpillCid::parse(&handle).expect("the handle is a canonical content CID");
    assert!(!text.contains("compaction:s0"), "no predicted s0 marker");
    assert!(
        store
            .fetch(&cid)
            .is_some_and(|r| r.redacted_text.contains("step 0")),
        "the emitted handle resolves to the committed verbatim span"
    );
    assert_eq!(store.unique_objects(), 1);
}

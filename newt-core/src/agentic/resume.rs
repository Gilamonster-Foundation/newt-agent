//! The `resume_context` tool seam (#714) — a self-scoped, read-only recovery
//! of what THIS conversation was working on before an interrupt.
//!
//! On an interrupt + auto-resume the restored conversation BECOMES the active
//! one, and the model's two instinctive working-memory reaches both dead-end on
//! it by construction:
//!
//! - `recall` excludes the active conversation by design (`recall.rs`), so it
//!   can never return the very thread the model wants to recover; and
//! - `state_get` reads the in-memory scratchpad (`scratchpad.rs`), which #713
//!   now rehydrates — but the model has to know the key to ask for it.
//!
//! `resume_context` is the missing affordance: one read-only tool that
//! concatenates, for THIS conversation, (a) its recent durable turns (via
//! [`RecallSource::this_conversation_recent`] — the deliberate opposite of
//! `recall`'s filter), (b) the current `<plan>` checklist, and (c) the saved
//! `<state>` scratchpad. No args, no mutation. When no conversation store is
//! present (headless / eval), it degrades to a clear "no history this session"
//! line rather than a dead end.

use super::display::{print_tool_call, print_tool_output};
use super::prompt_read::PromptReadContext;
use super::recall::RecallSource;
use super::scheduled::{plan_block, StepLedger};
use super::scratchpad::{scratchpad_state_block, ScratchpadStore};

/// How many of this conversation's recent turns `resume_context` pulls back.
const RESUME_DEFAULT_TURNS: usize = 6;

/// Model-facing contract for `resume_context` — coaching for a small local LLM
/// reaching for "what was I doing?" after a resume. No args (a self-read), and
/// it names the three things it returns so the model knows it landed.
const RESUME_CONTEXT_DESCRIPTION: &str =
    "Recover what you were working on — returns this conversation's recent \
     turns, the active prompt address/root/digest, the submitted receipt's \
     origin and previous/parent/root links, your current <plan>, and saved \
     <state>. Use prompt_read on a returned address for exact text. Use this \
     after a resume or when you've lost the thread. No args.";

/// The `resume_context` tool definition. Unlike `recall`, this is advertised
/// ALWAYS (it is broadly useful and degrades gracefully when its sources are
/// absent), so the merged set carries it even in headless callers — the
/// executor returns a clear "no history this session" when the store is `None`.
pub fn resume_context_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "resume_context",
            "description": RESUME_CONTEXT_DESCRIPTION,
            "parameters": { "type": "object", "properties": {}, "required": [] }
        }
    })
}

/// Execute a `resume_context` call (#714) — read-only. Concatenates this
/// conversation's recent turns, `<plan>`, and `<state>` into one block. Every
/// branch returns a tool-result String, never a loop abort:
/// - `recall_source` is `None` (headless / eval) → a clear "no history" line;
/// - the turn-load backend fails → the failure is folded into the block as a
///   note, never propagated (the plan/state sections still render);
/// - the plan / state are empty → those sections are simply omitted.
pub(crate) fn execute_resume_context(
    recall_source: Option<&dyn RecallSource>,
    step_ledger: Option<&dyn StepLedger>,
    scratchpad_store: Option<&dyn ScratchpadStore>,
    prompt_context: Option<PromptReadContext<'_>>,
    color: bool,
    tool_output_lines: usize,
) -> String {
    print_tool_call("resume_context", "", color);

    let mut out = String::from("Recovering what this conversation was working on.\n");

    if let Some(context) = prompt_context {
        if let Some(receipt) = context.active_receipt() {
            out.push_str(&format!(
                "\n— active operator prompt —\naddress: {}\nroot: {}\nmodel_digest: {}\n\
                 exact text: prompt_read {{\"address\":\"current\"}}\n",
                receipt.id(),
                receipt.root_prompt_id(),
                receipt.model_digest()
            ));
            if let Some(submitted) = context.submitted_receipt() {
                out.push_str(&format!(
                    "\n— submitted request receipt —\naddress: {}\norigin: {}\n\
                     previous_address: {}\nparent_address: {}\nroot_address: {}\n\
                     exact submitted text: prompt_read {{\"address\":\"submitted\"}}\n",
                    submitted.id(),
                    prompt_origin_name(submitted.origin()),
                    display_optional_prompt_id(submitted.previous_prompt_id()),
                    display_optional_prompt_id(submitted.parent_prompt_id()),
                    submitted.root_prompt_id(),
                ));
                if submitted.previous_prompt_id().is_some() {
                    out.push_str(
                        "recover chronological predecessor: prompt_read \
                         {\"address\":\"previous\"}\n",
                    );
                }
                if submitted.parent_prompt_id().is_some() {
                    out.push_str("recover semantic parent: prompt_read {\"address\":\"parent\"}\n");
                }
            }
        } else {
            out.push_str(
                "\n— active operator prompt —\nexact text: prompt_read \
                 {\"address\":\"current\"} (ephemeral; no durable address)\n\
                 \n— submitted request receipt —\naddress: null\norigin: null\n\
                 previous_address: null\nparent_address: null\nroot_address: null\n",
            );
        }
    }

    // (a) THIS conversation's recent durable turns — the pre-interrupt work the
    //     live window may have dropped to compaction.
    out.push_str("\n— recent turns (oldest first) —\n");
    match recall_source {
        Some(source) => match source.this_conversation_recent(RESUME_DEFAULT_TURNS) {
            Ok(hits) if !hits.is_empty() => {
                for h in &hits {
                    out.push_str(&format!("[{}] {}\n", h.seq, h.snippet));
                }
            }
            Ok(_) => out.push_str("(no earlier turns recorded yet)\n"),
            // A backend failure is a note in the block, not a loop abort — the
            // plan / state below may still orient the model.
            Err(e) => out.push_str(&format!("(could not load earlier turns: {e})\n")),
        },
        None => out.push_str(
            "(no conversation history available this session — headless / eval \
             sessions may have none)\n",
        ),
    }

    // (b) the current <plan> checklist, read-only (omitted when empty).
    if let Some(block) = step_ledger.and_then(plan_block) {
        out.push('\n');
        out.push_str(&block);
        out.push('\n');
    }

    // (c) the saved <state> scratchpad, read-only (omitted when empty). Reuses
    //     the same capped renderer the per-turn injection uses.
    if let Some(block) = scratchpad_store.and_then(scratchpad_state_block) {
        out.push('\n');
        out.push_str(&block);
        out.push('\n');
    }

    print_tool_output(&out, tool_output_lines, color);
    out
}

fn display_optional_prompt_id(id: Option<crate::PromptId>) -> String {
    id.map_or_else(|| "null".to_string(), |id| id.to_string())
}

fn prompt_origin_name(origin: crate::PromptOrigin) -> &'static str {
    match origin {
        crate::PromptOrigin::Operator => "operator",
        crate::PromptOrigin::HarnessRetry => "harness_retry",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SearchHit;
    use std::sync::Mutex;

    /// A `RecallSource` whose `this_conversation_recent` serves canned hits —
    /// the resume-side mirror of `recall::tests::MockSource`.
    #[derive(Default)]
    struct RecentMock {
        recent: Vec<SearchHit>,
        fail_with: Option<String>,
        calls: Mutex<Vec<usize>>,
    }

    impl RecallSource for RecentMock {
        fn search(&self, _query: &str, _limit: usize) -> anyhow::Result<Vec<SearchHit>> {
            Ok(Vec::new())
        }
        fn this_conversation_recent(&self, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
            self.calls.lock().unwrap().push(limit);
            match &self.fail_with {
                Some(e) => Err(anyhow::anyhow!("{e}")),
                None => Ok(self.recent.iter().take(limit).cloned().collect()),
            }
        }
    }

    fn hit(seq: i64, snippet: &str) -> SearchHit {
        SearchHit {
            conversation_id: "conv-x".to_string(),
            title: "the task".to_string(),
            seq,
            snippet: snippet.to_string(),
            rank: 0.0,
        }
    }

    #[test]
    fn definition_is_argless_and_named() {
        let def = resume_context_tool_definition();
        assert_eq!(def["function"]["name"], "resume_context");
        assert_eq!(
            def["function"]["parameters"]["properties"],
            serde_json::json!({})
        );
        let desc = def["function"]["description"].as_str().unwrap();
        assert!(desc.contains("Recover what you were working on"), "{desc}");
        assert!(
            desc.contains("<plan>") && desc.contains("<state>"),
            "{desc}"
        );
    }

    #[test]
    fn renders_turns_plan_and_state_together() {
        use crate::agentic::scheduled::SessionStepLedger;
        use crate::agentic::scratchpad::SessionScratchpadStore;

        let source = RecentMock {
            recent: vec![hit(1, "you: start it\n    me: ok"), hit(2, "you: more")],
            ..Default::default()
        };
        let ledger = SessionStepLedger::default();
        ledger.set_plan(&["scope it".to_string(), "build it".to_string()]);
        let scratch = SessionScratchpadStore::default();
        scratch.set("current_task", "wire the PyO3 bindings".to_string());

        let out = execute_resume_context(
            Some(&source),
            Some(&ledger as &dyn StepLedger),
            Some(&scratch as &dyn ScratchpadStore),
            None,
            false,
            20,
        );

        // (a) recent turns, oldest first, seq-labelled.
        assert!(out.contains("— recent turns (oldest first) —"), "{out}");
        assert!(out.contains("[1] you: start it"), "{out}");
        assert!(out.contains("[2] you: more"), "{out}");
        // (b) the <plan> block, with the active step marked.
        assert!(
            out.contains("<plan>") && out.contains("→ 1. scope it"),
            "{out}"
        );
        // (c) the <state> block.
        assert!(
            out.contains("<state>") && out.contains("current_task: wire the PyO3 bindings"),
            "{out}"
        );
        // It asked the source for the default number of turns, read-only.
        assert_eq!(*source.calls.lock().unwrap(), vec![RESUME_DEFAULT_TURNS]);
        assert_eq!(ledger.count(), 2, "resume_context does not mutate the plan");
        assert_eq!(scratch.keys_count(), 1, "or the scratchpad");
    }

    #[test]
    fn no_source_returns_clear_no_history_message() {
        let out = execute_resume_context(None, None, None, None, false, 20);
        assert!(
            out.contains("no conversation history available this session"),
            "{out}"
        );
    }

    #[test]
    fn resume_names_active_and_submitted_prompt_handles_without_copying_text() {
        let operator = crate::TurnPromptContext::ephemeral_operator(
            "conv",
            "raw operator secret",
            "exact operator secret",
        );
        let retry = crate::TurnPromptContext::ephemeral_harness_retry(
            "conv",
            "retry raw",
            "please continue",
            &operator,
        )
        .unwrap();
        let context = PromptReadContext::new(Some(&retry), "wrong fallback", None);

        let out = execute_resume_context(None, None, None, Some(context), false, 20);
        assert!(out.contains(&operator.active().id().to_string()), "{out}");
        assert!(out.contains(operator.active().model_digest()), "{out}");
        assert!(out.contains(&retry.submitted().id().to_string()), "{out}");
        assert!(out.contains("origin: harness_retry"), "{out}");
        assert!(
            out.contains(&format!("previous_address: {}", operator.submitted().id())),
            "{out}"
        );
        assert!(
            out.contains(&format!("parent_address: {}", operator.submitted().id())),
            "{out}"
        );
        assert!(out.contains("recover chronological predecessor"), "{out}");
        assert!(out.contains("prompt_read"), "{out}");
        assert!(!out.contains("exact operator secret"), "{out}");
        assert!(!out.contains("please continue"), "{out}");
    }

    #[test]
    fn empty_sources_render_without_panicking() {
        // A source with no turns + no plan + no state → just the header and the
        // "no earlier turns" note; the plan / state sections are omitted.
        let source = RecentMock::default();
        let out = execute_resume_context(Some(&source), None, None, None, false, 20);
        assert!(out.contains("(no earlier turns recorded yet)"), "{out}");
        assert!(!out.contains("<plan>"), "{out}");
        assert!(!out.contains("<state>"), "{out}");
    }

    #[test]
    fn turn_load_failure_is_folded_in_not_aborted() {
        use crate::agentic::scheduled::SessionStepLedger;
        let source = RecentMock {
            fail_with: Some("db is on fire".to_string()),
            ..Default::default()
        };
        let ledger = SessionStepLedger::default();
        ledger.set_plan(&["recover".to_string()]);
        let out = execute_resume_context(
            Some(&source),
            Some(&ledger as &dyn StepLedger),
            None,
            None,
            false,
            20,
        );
        assert!(
            out.contains("could not load earlier turns: db is on fire"),
            "{out}"
        );
        // The plan still renders despite the turn-load failure.
        assert!(out.contains("<plan>"), "{out}");
    }
}

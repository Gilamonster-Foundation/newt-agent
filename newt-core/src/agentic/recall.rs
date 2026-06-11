//! The `recall` tool seam — model-driven search over PAST conversations
//! (Step 17.5, #246).
//!
//! Mirrors the `save_note` architecture ([`super::note_sink`]): the loop
//! cannot name [`crate::store::ConversationStore`] directly without dragging
//! persistence wiring into `newt-core::agentic`, so the seam is a minimal
//! trait — the TUI injects [`StoreRecallSource`] (workspace-fenced by the
//! store itself, current conversation excluded) and passes it through
//! `ChatCtx` as `Option<&dyn RecallSource>`. `None` ⇒ the tool is not
//! advertised and the loop never searches — eval/headless callers are
//! unaffected.
//!
//! Design lineage (hermes-agent study, `session_search_tool.py`): coaching
//! schema text ("USE THIS PROACTIVELY when the user references prior
//! work"), snippets as the whole payload (no full content, no aux-LLM
//! recaps), and the 17.3 sanitizer pre-flight so a query of pure
//! operators comes back as coaching, never as a loop-aborting error.

use super::display::{print_tool_call, print_tool_output};
use crate::store::SearchHit;

/// Default hit count when the model omits `limit`.
const RECALL_DEFAULT_LIMIT: usize = 5;
/// Hard ceiling on hits per call — snippets ride back through the model's
/// context, so the cap is a token-budget guard (18.1), not a search knob.
const RECALL_MAX_LIMIT: usize = 10;

/// Read-only search over PAST conversations behind the `recall` tool.
///
/// Object-safe and shareable (the loop holds `&dyn RecallSource`; `Sync`
/// because the borrow crosses `.await` points). Implementations MUST be
/// scoped to the active workspace and MUST exclude the conversation the
/// model is currently in — what's said here is already in context, and a
/// recall hit on it would teach the model to search instead of read.
/// Hit order is the backend's bm25 rank (§6: never re-sorted by any
/// timestamp — wall-clock is a display claim, not an ordering key).
pub trait RecallSource: Send + Sync {
    /// Return up to `limit` best-first hits for `query` (plain keywords;
    /// the executor has already pre-flighted the 17.3 sanitizer).
    fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>>;
}

/// The canonical [`RecallSource`]: [`crate::store::ConversationStore::search`]
/// (already fenced to the store's workspace key) minus the current
/// conversation. The TUI constructs one per turn next to its `NoteSink`.
pub struct StoreRecallSource<'a> {
    store: &'a crate::store::ConversationStore,
    current_conversation_id: &'a str,
}

/// Extra hits fetched beyond `limit` before the current conversation's own
/// turns are filtered out. If the current conversation holds more than this
/// many hits ranked above the cut, fewer than `limit` external hits return —
/// accepted: those would be the worst-ranked matches anyway.
const EXCLUSION_FETCH_HEADROOM: usize = 20;

impl<'a> StoreRecallSource<'a> {
    /// `current_conversation_id` is the conversation the model is in right
    /// now — its turns never appear in results (that's what context is for).
    pub fn new(
        store: &'a crate::store::ConversationStore,
        current_conversation_id: &'a str,
    ) -> Self {
        Self {
            store,
            current_conversation_id,
        }
    }
}

impl RecallSource for StoreRecallSource<'_> {
    fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
        // Over-fetch, drop the current conversation, truncate. bm25 order is
        // preserved end-to-end (§6 discipline: the store's rank IS the
        // ordering; nothing here re-sorts).
        let hits = self
            .store
            .search(query, limit.saturating_add(EXCLUSION_FETCH_HEADROOM))?;
        Ok(hits
            .into_iter()
            .filter(|h| h.conversation_id != self.current_conversation_id)
            .take(limit)
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Tool schema
// ---------------------------------------------------------------------------

/// The model-facing contract for `recall` — coaching text for a small local
/// LLM: what it searches (PAST conversations, this workspace), when to reach
/// for it (user references prior work), when NOT to (already in context),
/// and how to query (plain keywords, not boolean/FTS syntax). Kept tight:
/// schema tokens ride in every request (18.1).
const RECALL_DESCRIPTION: &str =
    "Search PAST conversations in this workspace. The current conversation is \
     never searched — what was said here is already in your context. Use this \
     when the user references earlier work ('like we did before', 'that bug \
     we fixed', 'where did we leave off') or resumes a topic you don't see in \
     context. Do NOT use it for information already in this conversation. \
     Query with plain keywords ('tokio panic retry'), not boolean operators \
     or quotes. Each result is a conversation id, title, and snippet with the \
     match marked «like this»; the user can reopen one with \
     /conversation restore <id>.";

/// The `recall` tool definition. NOT part of [`super::tool_definitions`]:
/// the loop advertises it only when a [`RecallSource`] is present, so
/// headless / eval callers (which pass `recall_source: None`) never see it.
pub fn recall_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "recall",
            "description": RECALL_DESCRIPTION,
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Plain keywords describing what to find, e.g. \
                                        'fts5 sanitizer tests' — no boolean or quote syntax"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max matches to return, 1-10 (default 5)"
                    }
                },
                "required": ["query"]
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Execute one `recall` call against the source and return the result text
/// fed back to the model.
///
/// Outcome contract (every branch is a *tool result*, never a loop abort):
/// - hits → one block per hit: short id, title, `seq N`, snippet with the
///   FTS5 `>>>`/`<<<` markers rewritten to `«`/`»` (the 17.4 convention);
/// - a query the 17.3 sanitizer rejects → "no searchable terms" coaching;
/// - zero hits → "no matches…" (never an empty string);
/// - a real backend failure → `error: …`, verbatim, like every other tool.
pub(crate) fn execute_recall(
    args: &serde_json::Value,
    source: &dyn RecallSource,
    color: bool,
    tool_output_lines: usize,
) -> String {
    let query = args["query"].as_str().unwrap_or("").trim();
    // Absent / non-integer limit → default; present → clamped to [1, max].
    let limit = args["limit"]
        .as_u64()
        .map(|l| {
            usize::try_from(l)
                .unwrap_or(RECALL_MAX_LIMIT)
                .clamp(1, RECALL_MAX_LIMIT)
        })
        .unwrap_or(RECALL_DEFAULT_LIMIT);

    print_tool_call("recall", query, color);

    if query.is_empty() {
        return "error: recall requires `query` — plain keywords describing what to find"
            .to_string();
    }
    // Pre-flight the 17.3 sanitizer so an all-syntax query renders as
    // coaching the model can act on, not an error that ends the turn.
    if crate::store::sanitize_fts5_query(query).is_err() {
        let out = format!(
            "no searchable terms in {query:?} — every term was search syntax or \
             punctuation; try plain keywords (e.g. 'tokio panic retry')"
        );
        print_tool_output(&out, tool_output_lines, color);
        return out;
    }

    let hits = match source.search(query, limit) {
        Ok(hits) => hits,
        Err(e) => return format!("error: {e}"),
    };
    if hits.is_empty() {
        let out =
            format!("no matches in past conversations for {query:?} — try different keywords");
        print_tool_output(&out, tool_output_lines, color);
        return out;
    }

    let mut out = format!(
        "{} match(es) in past conversations (best first):",
        hits.len()
    );
    for hit in &hits {
        let title = hit.title.trim();
        let title = if title.is_empty() {
            "(untitled)"
        } else {
            title
        };
        out.push_str(&format!(
            "\n{}  {}  ·  seq {}\n    {}",
            short_id(&hit.conversation_id),
            title,
            hit.seq,
            readable_snippet(&hit.snippet),
        ));
    }
    print_tool_output(&out, tool_output_lines, color);
    out
}

/// First 12 characters of a conversation id — the 17.4 convention: enough
/// `{unix_nanos}` digits for 10ms granularity, and `resolve_id` accepts any
/// unique prefix, so the short id pastes into `/conversation restore`.
fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// Make a raw FTS5 snippet read naturally to a model: collapse internal
/// whitespace (turn text is multi-line) and rewrite the store's `>>>`/`<<<`
/// match markers to `«`/`»` — the same convention 17.4's `/recall` renders
/// for humans, so the model and the user see identical highlights.
fn readable_snippet(snippet: &str) -> String {
    snippet
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(">>>", "«")
        .replace("<<<", "»")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Scriptable mock: records every `(query, limit)` call, serves canned
    /// hits (truncated to `limit`), or a canned error. Shared with the
    /// dispatch tests in `agentic::tools`.
    #[derive(Default)]
    pub(crate) struct MockSource {
        pub calls: Mutex<Vec<(String, usize)>>,
        pub hits: Vec<SearchHit>,
        pub fail_with: Option<String>,
    }

    impl RecallSource for MockSource {
        fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
            self.calls.lock().unwrap().push((query.to_string(), limit));
            match &self.fail_with {
                Some(e) => Err(anyhow::anyhow!("{e}")),
                None => Ok(self.hits.iter().take(limit).cloned().collect()),
            }
        }
    }

    pub(crate) fn hit(id: &str, title: &str, seq: i64, snippet: &str) -> SearchHit {
        SearchHit {
            conversation_id: id.to_string(),
            title: title.to_string(),
            seq,
            snippet: snippet.to_string(),
            rank: -1.0,
        }
    }

    // -- schema text: the model-facing coaching ------------------------------

    #[test]
    fn schema_says_past_conversations_and_excludes_current() {
        let def = recall_tool_definition();
        let desc = def["function"]["description"].as_str().unwrap();
        assert!(desc.contains("Search PAST conversations in this workspace"));
        assert!(
            desc.contains("The current conversation is never searched"),
            "got: {desc}"
        );
        assert!(desc.contains("already in your context"), "got: {desc}");
    }

    #[test]
    fn schema_coaches_when_to_use_and_when_not() {
        let def = recall_tool_definition();
        let desc = def["function"]["description"].as_str().unwrap();
        assert!(desc.contains("'like we did before'"), "got: {desc}");
        assert!(desc.contains("'where did we leave off'"), "got: {desc}");
        assert!(
            desc.contains("Do NOT use it for information already in this conversation"),
            "got: {desc}"
        );
    }

    #[test]
    fn schema_coaches_plain_keywords_over_fts_syntax() {
        let def = recall_tool_definition();
        let desc = def["function"]["description"].as_str().unwrap();
        assert!(desc.contains("plain keywords"), "got: {desc}");
        assert!(
            desc.contains("not boolean operators or quotes"),
            "got: {desc}"
        );
    }

    #[test]
    fn schema_shape_query_required_limit_optional() {
        let def = recall_tool_definition();
        assert_eq!(def["function"]["name"], "recall");
        let required: Vec<&str> = def["function"]["parameters"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, vec!["query"]);
        let props = &def["function"]["parameters"]["properties"];
        assert!(props["query"].is_object());
        assert_eq!(props["limit"]["type"], "integer");
    }

    // -- dispatch ------------------------------------------------------------

    #[test]
    fn happy_path_formats_short_id_title_seq_and_markers() {
        let source = MockSource {
            hits: vec![
                hit(
                    "1748563200123-aaaa-bbbb",
                    "fixing the sanitizer",
                    4,
                    "…the >>>sanitizer<<< drops dangling\noperators…",
                ),
                hit("1748563200456-cccc-dddd", "  ", 2, ">>>sanitizer<<< port"),
            ],
            ..Default::default()
        };
        let out = execute_recall(
            &serde_json::json!({"query": "sanitizer"}),
            &source,
            false,
            20,
        );
        assert!(
            out.starts_with("2 match(es) in past conversations (best first):"),
            "got: {out}"
        );
        // Short id (12 chars), title, seq — one header per hit.
        assert!(
            out.contains("174856320012  fixing the sanitizer  ·  seq 4"),
            "got: {out}"
        );
        // Markers converted per the 17.4 convention; whitespace flattened.
        assert!(
            out.contains("«sanitizer» drops dangling operators"),
            "got: {out}"
        );
        assert!(!out.contains(">>>"), "raw FTS5 markers leaked: {out}");
        // Empty title falls back, hit still rendered.
        assert!(out.contains("(untitled)  ·  seq 2"), "got: {out}");
        // Default limit reached the source.
        assert_eq!(
            *source.calls.lock().unwrap(),
            vec![("sanitizer".to_string(), 5)]
        );
    }

    #[test]
    fn limit_is_defaulted_and_clamped() {
        let source = MockSource::default();
        // Absent → default 5 (asserted above too); oversized → max 10; zero → 1.
        execute_recall(
            &serde_json::json!({"query": "x", "limit": 25}),
            &source,
            false,
            20,
        );
        execute_recall(
            &serde_json::json!({"query": "x", "limit": 0}),
            &source,
            false,
            20,
        );
        execute_recall(&serde_json::json!({"query": "x"}), &source, false, 20);
        let limits: Vec<usize> = source.calls.lock().unwrap().iter().map(|c| c.1).collect();
        assert_eq!(limits, vec![10, 1, 5]);
    }

    #[test]
    fn sanitizer_rejected_query_is_coaching_not_error() {
        let source = MockSource::default();
        let out = execute_recall(&serde_json::json!({"query": "AND (*)"}), &source, false, 20);
        assert!(out.contains("no searchable terms"), "got: {out}");
        assert!(out.contains("plain keywords"), "got: {out}");
        assert!(!out.starts_with("error:"), "must not abort-shape: {out}");
        assert!(
            source.calls.lock().unwrap().is_empty(),
            "rejected query must never reach the backend"
        );
    }

    #[test]
    fn zero_hits_says_no_matches_never_empty() {
        let source = MockSource::default();
        let out = execute_recall(
            &serde_json::json!({"query": "quetzalcoatl"}),
            &source,
            false,
            20,
        );
        assert!(out.contains("no matches"), "got: {out}");
        assert!(!out.is_empty());
    }

    #[test]
    fn missing_query_is_a_clear_error() {
        let source = MockSource::default();
        let out = execute_recall(&serde_json::json!({}), &source, false, 20);
        assert!(out.contains("requires `query`"), "got: {out}");
        assert!(source.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn backend_failure_surfaces_as_tool_error_text() {
        let source = MockSource {
            fail_with: Some("database is on fire".to_string()),
            ..Default::default()
        };
        let out = execute_recall(
            &serde_json::json!({"query": "anything"}),
            &source,
            false,
            20,
        );
        assert_eq!(out, "error: database is on fire");
    }
}

//! Experiential memory — the `experiential` context feature (Step 26.6a, #585).
//!
//! A session ledger of TASK OUTCOMES (what was attempted, how it turned out, and
//! the lesson) kept ACROSS tasks: unlike the scratchpad it SURVIVES `/new`, so a
//! later task can reuse a repair recipe or steer clear of a known dead end. A
//! quality WRITE-GATE keeps the ledger high-signal — only substantive,
//! outcome-bearing records are stored. Relevant experiences are retrieved by
//! keyword overlap and injected as an `<experience>` block at the head of the
//! turn (gated by the feature, like 26.3/26.4/26.5).
//!
//! Pure in-memory + deterministic (a `Vec`, keyword overlap — no clock, uuid, or
//! embeddings), so the whole feature unit-tests with zero network/fs. Mirrors
//! `scratchpad.rs`: a `&self` interior-mutability trait + an in-memory impl.

use super::display::{print_tool_call, print_tool_output};
use std::sync::Mutex;

/// One recorded experience: a task, its outcome, and the lesson learned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Experience {
    pub task: String,
    pub outcome: String,
    pub lesson: String,
}

/// How many experiences to inject / recall per query (the keyword overlap is
/// cheap, so a small fixed top_k needs no config knob).
pub const EXPERIENCE_TOP_K: usize = 5;
/// Minimum lesson length (chars) for the write-gate — a one-word "lesson" is
/// noise that would dilute the injected block.
pub(crate) const MIN_LESSON_CHARS: usize = 12;
/// Per-field render cap (task/outcome) and lesson cap in the `<experience>` block.
pub(crate) const EXPERIENCE_FIELD_CAP: usize = 240;
pub(crate) const EXPERIENCE_LESSON_CAP: usize = 600;
/// Whole-block char cap so a long ledger can never blow the send budget.
pub(crate) const EXPERIENCE_TOTAL_CAP: usize = 4_000;

/// Whether a candidate experience clears the quality write-gate (Step 26.6a): a
/// non-empty task, a non-empty outcome, and a lesson with real substance
/// (≥ [`MIN_LESSON_CHARS`]). Keeps the ledger worth its tokens.
pub(crate) fn passes_write_gate(task: &str, outcome: &str, lesson: &str) -> bool {
    !task.trim().is_empty()
        && !outcome.trim().is_empty()
        && lesson.trim().chars().count() >= MIN_LESSON_CHARS
}

/// Tokenize for keyword-overlap relevance: alphanumeric runs of ≥3 chars,
/// lowercased. Deterministic, allocation-light.
fn terms(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect()
}

/// Count distinct query terms that appear in `text` (term-overlap score).
fn overlap(query_terms: &[String], text: &str) -> usize {
    let tterms = terms(text);
    query_terms
        .iter()
        .filter(|q| tterms.iter().any(|t| t == *q))
        .count()
}

/// A session store for recorded experiences (Step 26.6a). `&self` methods
/// (interior mutability) so one shared `&dyn ExperienceStore` serves both the
/// per-turn injection and the record/recall tools. SURVIVES `/new` (cross-task).
pub trait ExperienceStore: Send + Sync {
    /// Store an experience IFF it clears the write-gate; returns whether it did.
    fn record(&self, task: &str, outcome: &str, lesson: &str) -> bool;
    /// The `top_k` experiences most relevant to `query` by keyword overlap, ties
    /// broken by recency (most-recent first). Empty when nothing overlaps.
    fn relevant(&self, query: &str, top_k: usize) -> Vec<Experience>;
    /// Number of experiences held (for `/context stats`).
    fn count(&self) -> u64;
    /// Total chars across all stored fields (for `/context stats`).
    fn total_chars(&self) -> u64;
    /// Drop all entries (session end; NOT called on `/new`).
    fn clear(&self);
}

/// In-memory, session-scoped [`ExperienceStore`] — pure (no fs). A `Vec` in
/// insertion order; relevance is computed on read, so the store stays a simple
/// deterministic log (no clock/uuid) for stable tests.
#[derive(Default)]
pub struct SessionExperienceStore {
    entries: Mutex<Vec<Experience>>,
}

impl ExperienceStore for SessionExperienceStore {
    fn record(&self, task: &str, outcome: &str, lesson: &str) -> bool {
        if !passes_write_gate(task, outcome, lesson) {
            return false;
        }
        self.entries.lock().unwrap().push(Experience {
            task: task.trim().to_string(),
            outcome: outcome.trim().to_string(),
            lesson: lesson.trim().to_string(),
        });
        true
    }

    fn relevant(&self, query: &str, top_k: usize) -> Vec<Experience> {
        let qterms = terms(query);
        if qterms.is_empty() || top_k == 0 {
            return Vec::new();
        }
        let entries = self.entries.lock().unwrap();
        // Score by overlap with the task + lesson; keep only positives.
        let mut scored: Vec<(usize, usize, &Experience)> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (overlap(&qterms, &format!("{} {}", e.task, e.lesson)), i, e))
            .filter(|(s, _, _)| *s > 0)
            .collect();
        // Highest overlap first; ties → most-recent (higher index) first.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        scored
            .into_iter()
            .take(top_k)
            .map(|(_, _, e)| e.clone())
            .collect()
    }

    fn count(&self) -> u64 {
        self.entries.lock().unwrap().len() as u64
    }

    fn total_chars(&self) -> u64 {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .map(|e| {
                (e.task.chars().count() + e.outcome.chars().count() + e.lesson.chars().count())
                    as u64
            })
            .sum()
    }

    fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

fn truncate(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let head: String = s.chars().take(cap).collect();
        format!("{head}[…]")
    }
}

/// Render the `<experience>` block of the relevant past experiences (Step
/// 26.6a). `None` when none are relevant — the OFF/empty bit-for-bit guarantee
/// (mirror `scratchpad::build_state_block`). Per-field and whole-block caps are
/// applied here at injection.
pub(crate) fn build_experience_block(
    store: &dyn ExperienceStore,
    query: &str,
    top_k: usize,
    total_cap: usize,
) -> Option<String> {
    let hits = store.relevant(query, top_k);
    if hits.is_empty() {
        return None;
    }
    let mut body = String::from("<experience>\n");
    for e in &hits {
        let piece = format!(
            "- task: {}\n  outcome: {}\n  lesson: {}\n",
            truncate(&e.task, EXPERIENCE_FIELD_CAP),
            truncate(&e.outcome, EXPERIENCE_FIELD_CAP),
            truncate(&e.lesson, EXPERIENCE_LESSON_CAP),
        );
        if body.chars().count() + piece.chars().count() + "</experience>".len() > total_cap {
            body.push_str("[… more experience omitted to fit the budget …]\n");
            break;
        }
        body.push_str(&piece);
    }
    body.push_str("</experience>");
    Some(body)
}

/// TUI-facing entry: the relevant `<experience>` for `query` with the default
/// budget cap (Step 26.6a). `None` when nothing is relevant.
pub fn experience_block(store: &dyn ExperienceStore, query: &str, top_k: usize) -> Option<String> {
    build_experience_block(store, query, top_k, EXPERIENCE_TOTAL_CAP)
}

// ---------------------------------------------------------------------------
// Tool schemas (advertised only when the feature is on + a store is present)
// ---------------------------------------------------------------------------

/// `experience_record` tool definition.
pub fn experience_record_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "experience_record",
            "description": "Record a reusable lesson from a finished task — a repair recipe \
                            that worked, a dead end to avoid, a gotcha. It persists ACROSS \
                            tasks this session (it survives /new) and the relevant ones are \
                            shown to you automatically on similar future tasks. Only \
                            substantive lessons are kept (a real outcome + a lesson of a \
                            sentence or so).",
            "parameters": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "What was being attempted (a short phrase)." },
                    "outcome": { "type": "string", "description": "How it turned out, e.g. 'fixed' / 'failed' / 'worked but slow'." },
                    "lesson": { "type": "string", "description": "The reusable takeaway — specific enough to act on next time." }
                },
                "required": ["task", "outcome", "lesson"]
            }
        }
    })
}

/// `experience_recall` tool definition.
pub fn experience_recall_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "experience_recall",
            "description": "Search your recorded experiences for ones relevant to a topic \
                            (by keyword) — use it to check 'have I dealt with this before?' \
                            when the auto-injected block doesn't already cover it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The topic / task to recall experience for." }
                },
                "required": ["query"]
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Executors (every branch returns a tool-result String, never a loop abort)
// ---------------------------------------------------------------------------

/// Execute an `experience_record` call (Step 26.6a) — write-gated.
pub(crate) fn execute_experience_record(
    args: &serde_json::Value,
    store: &dyn ExperienceStore,
    color: bool,
    tool_output_lines: usize,
) -> String {
    let task = args["task"].as_str().unwrap_or("").trim();
    let outcome = args["outcome"].as_str().unwrap_or("").trim();
    let lesson = args["lesson"].as_str().unwrap_or("").trim();
    print_tool_call("experience_record", task, color);
    let out = if store.record(task, outcome, lesson) {
        "recorded experience".to_string()
    } else {
        format!(
            "not recorded — needs a non-empty task + outcome and a lesson of ≥{MIN_LESSON_CHARS} chars"
        )
    };
    print_tool_output(&out, tool_output_lines, color);
    out
}

/// Execute an `experience_recall` call (Step 26.6a).
pub(crate) fn execute_experience_recall(
    args: &serde_json::Value,
    store: &dyn ExperienceStore,
    top_k: usize,
    color: bool,
    tool_output_lines: usize,
) -> String {
    let query = args["query"].as_str().unwrap_or("").trim();
    print_tool_call("experience_recall", query, color);
    if query.is_empty() {
        return "error: experience_recall requires a non-empty `query`".to_string();
    }
    let out = build_experience_block(store, query, top_k, EXPERIENCE_TOTAL_CAP)
        .unwrap_or_else(|| "no relevant experience recorded yet".to_string());
    print_tool_output(&out, tool_output_lines, color);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_gate_filters_trivial_records() {
        assert!(passes_write_gate(
            "fix lints",
            "fixed",
            "run cargo fmt before clippy"
        ));
        assert!(
            !passes_write_gate("", "fixed", "a real lesson here"),
            "empty task"
        );
        assert!(
            !passes_write_gate("t", "", "a real lesson here"),
            "empty outcome"
        );
        assert!(
            !passes_write_gate("t", "ok", "too short"),
            "lesson < 12 chars"
        );
    }

    #[test]
    fn store_records_only_gated_and_tracks_stats() {
        let s = SessionExperienceStore::default();
        let (task, outcome, lesson) = ("parser", "fixed", "memoize the tokenizer here");
        assert!(s.record(task, outcome, lesson));
        assert!(!s.record("x", "y", "short"), "trivial rejected");
        assert_eq!(s.count(), 1, "only the gated record stored");
        // exact char accounting (chars().count(), so multibyte-safe)
        let expected =
            (task.chars().count() + outcome.chars().count() + lesson.chars().count()) as u64;
        assert_eq!(s.total_chars(), expected);
        s.clear();
        assert_eq!(s.count(), 0);
        assert_eq!(s.total_chars(), 0);
    }

    #[test]
    fn relevant_ranks_by_overlap_then_recency() {
        let s = SessionExperienceStore::default();
        s.record(
            "tokenizer perf",
            "fixed",
            "memoize tokenizer lookups for speed",
        );
        s.record(
            "network retry",
            "fixed",
            "exponential backoff avoids thundering herd",
        );
        s.record(
            "tokenizer crash",
            "fixed",
            "guard the tokenizer against empty input",
        );
        // a tokenizer query → the two tokenizer entries, recent first; not network
        let hits = s.relevant("tokenizer problems", 5);
        assert_eq!(hits.len(), 2, "only the two overlapping entries");
        assert_eq!(
            hits[0].task, "tokenizer crash",
            "recency breaks the overlap tie"
        );
        assert!(hits.iter().all(|e| e.task != "network retry"));
        // no overlap → empty; top_k 0 → empty
        assert!(s.relevant("kubernetes yaml", 5).is_empty());
        assert!(s.relevant("tokenizer", 0).is_empty());
    }

    #[test]
    fn build_block_none_when_irrelevant_and_caps() {
        let s = SessionExperienceStore::default();
        assert_eq!(
            build_experience_block(&s, "anything", 5, 4000),
            None,
            "empty store"
        );
        s.record(
            "indexing",
            "ok",
            "build the index lazily on first use, then cache it",
        );
        // relevant → a rendered block
        let block = build_experience_block(&s, "indexing strategy", 5, 4000).unwrap();
        assert!(block.starts_with("<experience>\n") && block.ends_with("</experience>"));
        assert!(block.contains("task: indexing") && block.contains("lesson: build the index"));
        // irrelevant query → None even with entries
        assert_eq!(build_experience_block(&s, "zzz", 5, 4000), None);
        // total cap trips the omitted marker
        for i in 0..40 {
            s.record(
                &format!("indexing task {i}"),
                "ok",
                &"index lessons ".repeat(20),
            );
        }
        let capped = build_experience_block(&s, "indexing", 50, 300).unwrap();
        assert!(
            capped.chars().count() <= 300 + 80,
            "total cap bounds the block"
        );
        assert!(capped.contains("omitted to fit"));
    }

    #[test]
    fn executors_record_gate_and_recall() {
        let s = SessionExperienceStore::default();
        // record: gated accept / reject coaching
        assert_eq!(
            execute_experience_record(
                &serde_json::json!({"task": "ci flake", "outcome": "fixed", "lesson": "pin the seed for the fuzz test"}),
                &s,
                false,
                20
            ),
            "recorded experience"
        );
        assert!(execute_experience_record(
            &serde_json::json!({"task": "x", "outcome": "y", "lesson": "short"}),
            &s,
            false,
            20
        )
        .starts_with("not recorded"));
        // recall: hit / miss / empty-query coaching
        assert!(execute_experience_recall(
            &serde_json::json!({"query": "ci flake seed"}),
            &s,
            5,
            false,
            20
        )
        .contains("<experience>"));
        assert!(execute_experience_recall(
            &serde_json::json!({"query": "unrelated topic"}),
            &s,
            5,
            false,
            20
        )
        .contains("no relevant experience"));
        assert!(
            execute_experience_recall(&serde_json::json!({}), &s, 5, false, 20)
                .starts_with("error:")
        );
    }

    #[test]
    fn tool_definitions_shape() {
        assert_eq!(
            experience_record_tool_definition()["function"]["name"],
            "experience_record"
        );
        assert_eq!(
            experience_recall_tool_definition()["function"]["name"],
            "experience_recall"
        );
    }
}

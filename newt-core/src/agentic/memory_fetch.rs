//! The `memory_fetch` tool seam — model-driven pull of an ADDRESSED memory
//! item (progressive-disclosure memory, Workstream A MVP, #319).
//!
//! Mirrors the `recall` architecture ([`super::recall`]) exactly: the loop
//! cannot name [`crate::store::ConversationStore`] / [`crate::notes::NoteStore`]
//! directly without dragging persistence wiring into `newt-core::agentic`, so
//! the seam is a minimal trait — the TUI injects [`StoreMemorySource`]
//! (workspace-fenced by the store itself) and passes it through `ChatCtx` as
//! `Option<&dyn MemorySource>`. `None` ⇒ the tool is not advertised and the
//! loop never fetches — eval/headless/ACP callers are unaffected, bit-for-bit.
//!
//! Where `recall` returns ranked *snippets* over PAST conversations,
//! `memory_fetch` returns the *full verbatim body* of one item the model
//! already saw an address for (a note id in the memory index, a `seq` in a
//! recall hit). It is `use_skill` for memory: index in the prompt, body on the
//! tool call.
//!
//! Design lineage (the recall lesson): coaching schema text tuned for small
//! local models — the three address forms shown by example, and when to reach
//! for a fetch vs. a re-read vs. a recall. Every dispatch branch returns a
//! tool *result* (found / not-found / malformed address → friendly coaching
//! text), never a loop-aborting error.
//!
//! ## MVP scope (#319, the design note's §9 MVP)
//!
//! Resolves only `note:<id>` (a [`crate::notes::NoteStore`] body) and
//! `turn:<conv>#<seq>` (a [`crate::store::ConversationStore`] turn) — both read
//! EXISTING surfaces, **no new persistence**. The `compaction:<id>` address and
//! its retention store are deliberately DEFERRED to the follow-up PR (they
//! carry the secret-retention adversarial review); a `compaction:` address here
//! resolves to coaching that names it as unsupported in this build, never a
//! crash.

use super::display::{print_tool_call, print_tool_output};

/// A parsed, tagged memory address. The MVP resolves [`Self::Note`] and
/// [`Self::Turn`]; [`Self::Compaction`] is recognized (so the executor can
/// coach precisely) but resolution is deferred to the follow-up PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemAddr {
    /// `note:<id>` — the verbatim body of one [`crate::notes::NoteStore`]
    /// entry, addressed by the 1-based id the memory index renders.
    Note { id: String },
    /// `turn:<conv>#<seq>` — one past [`crate::store::ConversationStore`]
    /// turn's verbatim user/assistant text, §6-ordered by `seq` (never
    /// re-sorted by clock).
    Turn { conversation: String, seq: i64 },
    /// `compaction:<id>` — DEFERRED (the follow-up PR's retention surface).
    /// Parsed so the executor can name it precisely as unsupported here.
    Compaction { id: String },
}

impl MemAddr {
    /// Parse a tagged address. Returns `None` for anything that does not match
    /// `note:…`, `turn:…#…`, or `compaction:…` — the executor turns a `None`
    /// into coaching, never an error.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if let Some(id) = raw.strip_prefix("note:") {
            let id = id.trim();
            if id.is_empty() {
                return None;
            }
            return Some(Self::Note { id: id.to_string() });
        }
        if let Some(rest) = raw.strip_prefix("turn:") {
            // `turn:<conv>#<seq>` — the `#` separates the conversation id from
            // the §6 seq tick. Both halves must be present and non-empty.
            let (conv, seq) = rest.rsplit_once('#')?;
            let conv = conv.trim();
            let seq: i64 = seq.trim().parse().ok()?;
            if conv.is_empty() {
                return None;
            }
            return Some(Self::Turn {
                conversation: conv.to_string(),
                seq,
            });
        }
        if let Some(id) = raw.strip_prefix("compaction:") {
            let id = id.trim();
            if id.is_empty() {
                return None;
            }
            return Some(Self::Compaction { id: id.to_string() });
        }
        None
    }
}

/// The verbatim payload one [`MemorySource::fetch`] returns, or labelled
/// absence. Never an empty string — every variant carries text the executor
/// renders back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemPayload {
    /// The item was found: its full verbatim body.
    Found(String),
    /// The address was well-formed but no such item exists (unknown note id,
    /// unknown conversation, no turn at that seq, or — in this MVP — a
    /// `compaction:` address whose retention surface is not yet built).
    /// `reason` is a short human-readable why, shown to the model as coaching.
    NotFound { reason: String },
}

/// Read-only pull of an ADDRESSED memory item behind the `memory_fetch` tool.
///
/// Object-safe and shareable (the loop holds `&dyn MemorySource`; `Sync`
/// because the borrow crosses `.await` points) — the exact bounds and seam as
/// [`super::recall::RecallSource`]. Implementations MUST be scoped to the
/// active workspace: a `turn:`/`compaction:` address from another workspace
/// resolves to [`MemPayload::NotFound`], never a cross-workspace leak (§7).
pub trait MemorySource: Send + Sync {
    /// Resolve `addr` to its verbatim body or labelled absence. A genuine
    /// backend failure (e.g. a corrupt store row) is an `Err`; an unknown but
    /// well-formed address is `Ok(MemPayload::NotFound)` — the executor
    /// distinguishes the two (a backend error surfaces as `error:` verbatim;
    /// absence surfaces as coaching).
    fn fetch(&self, addr: &MemAddr) -> anyhow::Result<MemPayload>;
}

/// The canonical [`MemorySource`]: a [`crate::notes::NoteStore`] for `note:`
/// bodies and a [`crate::store::ConversationStore`] for `turn:` bodies (already
/// fenced to its workspace key). The TUI constructs one per turn next to its
/// `NoteSink` / `StoreRecallSource`.
///
/// Both surfaces already exist (the MVP adds no persistence): `note:` reads the
/// live `NoteStore` entries, `turn:` reads a single past turn by `(conv, seq)`.
pub struct StoreMemorySource<'a> {
    notes: &'a crate::notes::NoteStore,
    store: &'a crate::store::ConversationStore,
}

impl<'a> StoreMemorySource<'a> {
    pub fn new(
        notes: &'a crate::notes::NoteStore,
        store: &'a crate::store::ConversationStore,
    ) -> Self {
        Self { notes, store }
    }
}

impl MemorySource for StoreMemorySource<'_> {
    fn fetch(&self, addr: &MemAddr) -> anyhow::Result<MemPayload> {
        match addr {
            MemAddr::Note { id } => match self.notes.body_by_id(id) {
                Some(body) => Ok(MemPayload::Found(body.to_string())),
                None => Ok(MemPayload::NotFound {
                    reason: format!(
                        "no note with id {id:?} — copy a `note:<id>` from the memory index"
                    ),
                }),
            },
            MemAddr::Turn { conversation, seq } => {
                // Workspace-fenced + §6-ordered by the store (load_turn joins
                // on workspace_key and keys on the seq tick; nothing here
                // re-sorts by clock).
                match self.store.load_turn(conversation, *seq)? {
                    Some(turn) => Ok(MemPayload::Found(render_turn(&turn))),
                    None => Ok(MemPayload::NotFound {
                        reason: format!(
                            "no turn at seq {seq} in conversation {conversation:?} \
                             (or it is in another workspace)"
                        ),
                    }),
                }
            }
            // DEFERRED to the follow-up PR (the retention surface + its
            // secret-retention review). Recognized so the model gets a precise
            // answer instead of a "malformed address" misdirection.
            MemAddr::Compaction { id } => Ok(MemPayload::NotFound {
                reason: format!(
                    "compaction spans are not fetchable in this build (id {id:?}); \
                     re-read the file the breadcrumb names, or `recall` the topic"
                ),
            }),
        }
    }
}

/// Render one past turn's verbatim user/assistant text for the tool result.
/// A reply-less turn (a restored compaction record) shows only its user side.
fn render_turn(turn: &crate::ConversationTurn) -> String {
    let mut out = String::new();
    if !turn.user.is_empty() {
        out.push_str("user: ");
        out.push_str(&turn.user);
    }
    if !turn.assistant.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("assistant: ");
        out.push_str(&turn.assistant);
    }
    out
}

// ---------------------------------------------------------------------------
// Tool schema
// ---------------------------------------------------------------------------

/// The model-facing contract for `memory_fetch` — coaching text for a small
/// local LLM (the recall lesson, the design note's §8.4): the three address
/// forms shown BY EXAMPLE, and when to reach for a fetch vs. a re-read vs. a
/// recall. Kept tight: schema tokens ride in every request.
const MEMORY_FETCH_DESCRIPTION: &str =
    "Fetch the FULL verbatim body of one memory item by its address — a note, \
     or one past turn. Use this to pull back an exact body you only have an \
     INDEX line or a recall snippet for, instead of guessing its content. \
     Addresses look like `note:3` (a numbered note from the memory index) or \
     `turn:<conversation-id>#<seq>` (one past turn, e.g. \
     `turn:174856320012#7` — copy the id and `seq N` from a recall hit). \
     Reach for memory_fetch when you have an address but not the body; \
     re-read the file with read_file if the content is a file still on disk; \
     use recall to SEARCH past conversations when you don't have an address \
     yet. One item per call; copy the address exactly as it was shown to you.";

/// The `memory_fetch` tool definition. NOT part of [`super::tool_definitions`]:
/// the loop advertises it only when a [`MemorySource`] is present, so headless
/// / eval / ACP callers (which pass `memory_source: None`) never see it — the
/// exact gating discipline as `recall` and `save_note`.
pub fn memory_fetch_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "memory_fetch",
            "description": MEMORY_FETCH_DESCRIPTION,
            "parameters": {
                "type": "object",
                "properties": {
                    "address": {
                        "type": "string",
                        "description": "The tagged address to fetch, e.g. \
                                        'note:3' or 'turn:174856320012#7' — \
                                        copy it exactly as the index or a recall \
                                        hit showed it"
                    }
                },
                "required": ["address"]
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Execute one `memory_fetch` call against the source and return the result
/// text fed back to the model.
///
/// Outcome contract (every branch is a *tool result*, never a loop abort —
/// the `execute_recall` template):
/// - missing/empty `address` → coaching naming the address forms;
/// - a malformed address (no recognized tag) → coaching naming the three
///   forms by example;
/// - a well-formed but unknown address → "no such memory item …" (the
///   labelled-absence the #319 design turns on — never an empty string);
/// - a real backend failure → `error: …`, verbatim, like every other tool;
/// - found → the verbatim body.
pub(crate) fn execute_memory_fetch(
    args: &serde_json::Value,
    source: &dyn MemorySource,
    color: bool,
    tool_output_lines: usize,
) -> String {
    let address = args["address"].as_str().unwrap_or("").trim();

    print_tool_call("memory_fetch", address, color);

    if address.is_empty() {
        return "error: memory_fetch requires `address` — e.g. `note:3` or \
                `turn:<conversation-id>#<seq>`"
            .to_string();
    }

    let Some(addr) = MemAddr::parse(address) else {
        let out = format!(
            "{address:?} is not a memory address — they look like `note:3` \
             (a numbered note) or `turn:174856320012#7` (a conversation id \
             and `seq` from a recall hit). Copy one exactly as it was shown."
        );
        print_tool_output(&out, tool_output_lines, color);
        return out;
    };

    let payload = match source.fetch(&addr) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };

    let out = match payload {
        MemPayload::Found(body) => body,
        MemPayload::NotFound { reason } => format!("no such memory item: {reason}"),
    };
    print_tool_output(&out, tool_output_lines, color);
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Open a real `ConversationStore` for a test — `root` and `workspace`
    /// are separate dirs (`new(root, workspace, max)`), the idiom the store's
    /// own tests use.
    fn test_store(dir: &tempfile::TempDir) -> crate::store::ConversationStore {
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        crate::store::ConversationStore::new(dir.path().join("root"), &ws, 10).unwrap()
    }

    /// Scriptable mock: records every fetched address, serves a canned payload
    /// keyed on the address form, or a canned error. Shared with the dispatch
    /// tests in `agentic::tools` (the `recall::MockSource` pattern).
    #[derive(Default)]
    pub(crate) struct MockSource {
        pub calls: Mutex<Vec<MemAddr>>,
        /// Body returned for any `Note`/`Turn` (None ⇒ NotFound).
        pub body: Option<String>,
        pub fail_with: Option<String>,
    }

    impl MemorySource for MockSource {
        fn fetch(&self, addr: &MemAddr) -> anyhow::Result<MemPayload> {
            self.calls.lock().unwrap().push(addr.clone());
            if let Some(e) = &self.fail_with {
                return Err(anyhow::anyhow!("{e}"));
            }
            match &self.body {
                Some(b) => Ok(MemPayload::Found(b.clone())),
                None => Ok(MemPayload::NotFound {
                    reason: "mock has no such item".to_string(),
                }),
            }
        }
    }

    // -- address parsing -----------------------------------------------------

    #[test]
    fn parse_note_turn_and_compaction_forms() {
        assert_eq!(
            MemAddr::parse("note:3"),
            Some(MemAddr::Note { id: "3".into() })
        );
        assert_eq!(
            MemAddr::parse("  turn:174856320012#7 "),
            Some(MemAddr::Turn {
                conversation: "174856320012".into(),
                seq: 7
            })
        );
        assert_eq!(
            MemAddr::parse("compaction:abc"),
            Some(MemAddr::Compaction { id: "abc".into() })
        );
    }

    #[test]
    fn parse_rejects_malformed_addresses() {
        // No tag, empty body, missing seq, non-numeric seq, empty conversation.
        assert_eq!(MemAddr::parse("just some text"), None);
        assert_eq!(MemAddr::parse("note:"), None);
        assert_eq!(MemAddr::parse("turn:174856320012"), None);
        assert_eq!(MemAddr::parse("turn:174856320012#notanumber"), None);
        assert_eq!(MemAddr::parse("turn:#7"), None);
        assert_eq!(MemAddr::parse("compaction:"), None);
    }

    #[test]
    fn parse_turn_takes_last_hash_so_conversation_ids_may_contain_hash() {
        // rsplit on '#' — the seq is always the final segment.
        assert_eq!(
            MemAddr::parse("turn:conv#with#hash#42"),
            Some(MemAddr::Turn {
                conversation: "conv#with#hash".into(),
                seq: 42
            })
        );
    }

    // -- schema text: the model-facing coaching ------------------------------

    #[test]
    fn schema_shows_the_address_forms_by_example() {
        let def = memory_fetch_tool_definition();
        let desc = def["function"]["description"].as_str().unwrap();
        assert!(desc.contains("note:3"), "got: {desc}");
        assert!(desc.contains("turn:174856320012#7"), "got: {desc}");
    }

    #[test]
    fn schema_distinguishes_fetch_from_reread_and_recall() {
        let def = memory_fetch_tool_definition();
        let desc = def["function"]["description"].as_str().unwrap();
        assert!(desc.contains("re-read the file"), "got: {desc}");
        assert!(desc.contains("read_file"), "got: {desc}");
        assert!(desc.contains("use recall to SEARCH"), "got: {desc}");
    }

    #[test]
    fn schema_shape_address_required() {
        let def = memory_fetch_tool_definition();
        assert_eq!(def["function"]["name"], "memory_fetch");
        let required: Vec<&str> = def["function"]["parameters"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, vec!["address"]);
        assert!(def["function"]["parameters"]["properties"]["address"].is_object());
    }

    // -- dispatch ------------------------------------------------------------

    #[test]
    fn found_returns_the_verbatim_body() {
        let source = MockSource {
            body: Some("the exact bytes".to_string()),
            ..Default::default()
        };
        let out = execute_memory_fetch(
            &serde_json::json!({"address": "note:2"}),
            &source,
            false,
            20,
        );
        assert_eq!(out, "the exact bytes");
        assert_eq!(
            *source.calls.lock().unwrap(),
            vec![MemAddr::Note { id: "2".into() }]
        );
    }

    #[test]
    fn not_found_is_labelled_absence_never_empty() {
        let source = MockSource::default(); // body None ⇒ NotFound
        let out = execute_memory_fetch(
            &serde_json::json!({"address": "note:99"}),
            &source,
            false,
            20,
        );
        assert!(out.starts_with("no such memory item:"), "got: {out}");
        assert!(!out.is_empty());
        assert!(!out.starts_with("error:"), "must not abort-shape: {out}");
    }

    #[test]
    fn malformed_address_is_coaching_not_error_and_never_hits_backend() {
        let source = MockSource::default();
        let out = execute_memory_fetch(
            &serde_json::json!({"address": "give me the api signatures"}),
            &source,
            false,
            20,
        );
        assert!(out.contains("is not a memory address"), "got: {out}");
        assert!(out.contains("note:3"), "coaching shows the forms: {out}");
        assert!(!out.starts_with("error:"), "must not abort-shape: {out}");
        assert!(
            source.calls.lock().unwrap().is_empty(),
            "a malformed address must never reach the backend"
        );
    }

    #[test]
    fn missing_address_is_a_clear_error() {
        let source = MockSource::default();
        let out = execute_memory_fetch(&serde_json::json!({}), &source, false, 20);
        assert!(out.contains("requires `address`"), "got: {out}");
        assert!(source.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn backend_failure_surfaces_as_tool_error_text() {
        let source = MockSource {
            fail_with: Some("store is on fire".to_string()),
            ..Default::default()
        };
        let out = execute_memory_fetch(
            &serde_json::json!({"address": "turn:abc#1"}),
            &source,
            false,
            20,
        );
        assert_eq!(out, "error: store is on fire");
    }

    #[test]
    fn compaction_address_is_recognized_but_unsupported_in_mvp() {
        // The real StoreMemorySource returns a precise NotFound for
        // `compaction:` (deferred surface) — never a malformed-address answer.
        let dir = tempfile::tempdir().unwrap();
        let notes = crate::notes::NoteStore::new(dir.path().join("NOTES.md"), 2_200);
        let store = test_store(&dir);
        let source = StoreMemorySource::new(&notes, &store);
        let out = execute_memory_fetch(
            &serde_json::json!({"address": "compaction:xyz#1"}),
            &source,
            false,
            20,
        );
        assert!(out.starts_with("no such memory item:"), "got: {out}");
        assert!(out.contains("not fetchable in this build"), "got: {out}");
    }

    // -- StoreMemorySource against real surfaces (tempdir) -------------------

    #[tokio::test]
    async fn resolves_a_real_note_body() {
        use crate::memory::{MemoryProvider, SessionContext};
        let dir = tempfile::tempdir().unwrap();
        let mut notes = crate::notes::NoteStore::new(dir.path().join("NOTES.md"), 2_200);
        notes
            .initialize(&SessionContext {
                workspace: dir.path().to_string_lossy().into(),
                session_id: "s".into(),
            })
            .await
            .unwrap();
        notes.add("first note body").unwrap();
        notes.add("second\nmulti-line note").unwrap();
        let store = test_store(&dir);
        let source = StoreMemorySource::new(&notes, &store);

        // note:2 → the second entry's verbatim body.
        let out = execute_memory_fetch(
            &serde_json::json!({"address": "note:2"}),
            &source,
            false,
            20,
        );
        assert_eq!(out, "second\nmulti-line note");
        // note:9 → labelled absence.
        let miss = execute_memory_fetch(
            &serde_json::json!({"address": "note:9"}),
            &source,
            false,
            20,
        );
        assert!(miss.starts_with("no such memory item:"), "got: {miss}");
    }

    #[test]
    fn resolves_a_real_turn_by_conv_and_seq() {
        let dir = tempfile::tempdir().unwrap();
        let notes = crate::notes::NoteStore::new(dir.path().join("NOTES.md"), 2_200);
        let store = test_store(&dir);
        let conv = store.create("a conversation", None).unwrap();
        store
            .append_turn(
                &conv,
                "what is the connect signature?",
                "fn connect(addr: &str)",
            )
            .unwrap();
        // The seq the model would have seen in a recall hit: the matching
        // turn's §6 tick. Read the record back to learn the real seq.
        let hits = store.search("connect", 5).unwrap();
        assert!(!hits.is_empty(), "the turn must be searchable");
        let seq = hits[0].seq;

        let source = StoreMemorySource::new(&notes, &store);
        let out = execute_memory_fetch(
            &serde_json::json!({"address": format!("turn:{conv}#{seq}")}),
            &source,
            false,
            20,
        );
        assert!(
            out.contains("user: what is the connect signature?"),
            "got: {out}"
        );
        assert!(
            out.contains("assistant: fn connect(addr: &str)"),
            "got: {out}"
        );

        // A seq that doesn't exist → labelled absence, never an error.
        let miss = execute_memory_fetch(
            &serde_json::json!({"address": format!("turn:{conv}#999999")}),
            &source,
            false,
            20,
        );
        assert!(miss.starts_with("no such memory item:"), "got: {miss}");
        assert!(!miss.starts_with("error:"), "got: {miss}");
    }

    #[test]
    fn turn_from_another_conversation_id_is_absence_not_leak() {
        let dir = tempfile::tempdir().unwrap();
        let notes = crate::notes::NoteStore::new(dir.path().join("NOTES.md"), 2_200);
        let store = test_store(&dir);
        let source = StoreMemorySource::new(&notes, &store);
        // An unknown conversation id resolves to absence, never an error.
        let out = execute_memory_fetch(
            &serde_json::json!({"address": "turn:nonexistent-conv#1"}),
            &source,
            false,
            20,
        );
        assert!(out.starts_with("no such memory item:"), "got: {out}");
    }
}

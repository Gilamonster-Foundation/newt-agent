//! Structural prune — zero-LLM context compression over the wire-shape
//! message list (Step 18.3, issue #247).
//!
//! Pure functions over the `serde_json::Value` message shape the TUI loop
//! sends to Ollama / OpenAI-compatible backends: objects with `role` /
//! `content` / `tool_calls`, and `role: "tool"` results (with a
//! `tool_call_id` on the OpenAI path, positional on the Ollama path).
//!
//! Three passes, in the hermes-proven order (see
//! `docs/design/context-memory-hermes-learnings.md`, gap-matrix row
//! "Structural pruning before LLM summary"):
//!
//! 1. [`collapse_duplicate_tool_results`] — identical oversized tool results
//!    are collapsed: later occurrences become a one-liner referencing the
//!    first.
//! 2. [`summarize_aged_tool_results`] — tool results outside the protected
//!    tail are replaced with informative per-tool one-liners
//!    (`[run_command] ran 'npm test' -> ok, 47 lines output`).
//! 3. [`shrink_tool_call_args`] — oversized `function.arguments` are parsed,
//!    truncated *inside* the JSON structure (long string values), and
//!    reserialized, so the output is always valid JSON. Hermes learned the
//!    hard way that naive byte-slicing causes provider 400-loops
//!    (hermes #11762).
//!
//! Invariants (enforced by construction, verified by the property tests):
//! no message is ever added or removed, roles and `tool_call_id`s are never
//! touched, the last [`PruneConfig::keep_last`] messages are byte-identical,
//! a replacement is only applied when it is strictly shorter (serialized),
//! and `prune(prune(x)) == prune(x)`.
//!
//! Nothing calls this module yet — Step 18.4 wires it into the compression
//! pipeline after Step 9.7 lands the `agentic` module. Hashing uses `blake3`,
//! which is already in newt-core's build graph via `agent-mesh-protocol`.

use serde_json::Value;
use std::collections::HashMap;

/// Floor for [`PruneConfig::arg_string_cap`]. The truncation marker
/// (`… [+N chars]`) needs at most 31 chars even for a `usize::MAX` count, so
/// any cap >= 32 guarantees truncated strings land at or under the cap —
/// which is what makes [`shrink_tool_call_args`] idempotent.
const MIN_ARG_STRING_CAP: usize = 32;

/// Thresholds for the three prune passes. All sizes are in characters.
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Protected tail: the last `keep_last` messages are never modified
    /// (byte-identical in the output). "Aged" means anything before them.
    pub keep_last: usize,
    /// Pass 1 only collapses tool results strictly longer than this.
    pub dedupe_min_chars: usize,
    /// Pass 2 only summarizes tool results strictly longer than this.
    pub summarize_min_chars: usize,
    /// Pass 3 only rewrites `function.arguments` whose serialized form is
    /// strictly longer than this.
    pub args_max_chars: usize,
    /// Per-string cap applied *inside* parsed argument structures by pass 3.
    /// Values below [`MIN_ARG_STRING_CAP`] are raised to it.
    pub arg_string_cap: usize,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            keep_last: 10,
            dedupe_min_chars: 200,
            summarize_min_chars: 200,
            args_max_chars: 500,
            arg_string_cap: 200,
        }
    }
}

impl PruneConfig {
    fn effective_arg_string_cap(&self) -> usize {
        self.arg_string_cap.max(MIN_ARG_STRING_CAP)
    }

    /// Index of the first protected-tail message; `[0, aged)` may be edited.
    fn aged_len(&self, total: usize) -> usize {
        total.saturating_sub(self.keep_last)
    }
}

/// Result of [`prune`]: the rewritten message list plus exact accounting of
/// how many serialized characters the three passes reclaimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneOutcome {
    /// The pruned message list (same length and order as the input).
    pub messages: Vec<Value>,
    /// Exactly `serialized_len(input) - serialized_len(output)`, where
    /// `serialized_len` is the summed `serde_json::to_string` length of each
    /// message. Never negative: passes only apply strictly-shorter rewrites.
    pub chars_reclaimed: usize,
}

/// Run all three structural-prune passes in order.
///
/// ```
/// use newt_core::prune::{prune, PruneConfig};
/// use serde_json::json;
///
/// let mut messages = vec![
///     json!({"role": "user", "content": "fix the bug"}),
///     json!({"role": "assistant", "content": "",
///            "tool_calls": [{"function": {"name": "read_file",
///                                         "arguments": {"path": "src/lib.rs"}}}]}),
///     json!({"role": "tool", "content": "x".repeat(500)}),
/// ];
/// // Pad so the tool result falls outside the protected tail.
/// for i in 0..10 {
///     messages.push(json!({"role": "user", "content": format!("turn {i}")}));
/// }
///
/// let outcome = prune(&messages, &PruneConfig::default());
/// assert_eq!(
///     outcome.messages[2]["content"].as_str().unwrap(),
///     "[read_file] src/lib.rs lines 1-1 (page map; re-read any span with offset/limit): 1",
/// );
/// assert!(outcome.chars_reclaimed > 400);
/// ```
pub fn prune(messages: &[Value], cfg: &PruneConfig) -> PruneOutcome {
    let before = serialized_len(messages);
    let out = collapse_duplicate_tool_results(messages, cfg);
    let out = summarize_aged_tool_results(&out, cfg);
    let out = shrink_tool_call_args(&out, cfg);
    let after = serialized_len(&out);
    PruneOutcome {
        messages: out,
        chars_reclaimed: before.saturating_sub(after),
    }
}

/// Summed `serde_json::to_string` length of each message — the currency of
/// [`PruneOutcome::chars_reclaimed`].
pub fn serialized_len(messages: &[Value]) -> usize {
    messages.iter().map(json_len).sum()
}

/// Pass 1: collapse duplicate tool results.
///
/// Identical `role:"tool"` contents longer than
/// [`PruneConfig::dedupe_min_chars`] are hashed (blake3); aged later
/// occurrences are replaced with a one-liner referencing the first
/// occurrence's message index. The first occurrence and anything in the
/// protected tail are left untouched.
pub fn collapse_duplicate_tool_results(messages: &[Value], cfg: &PruneConfig) -> Vec<Value> {
    let mut out = messages.to_vec();
    let aged = cfg.aged_len(out.len());
    let paired = pair_tool_results(&out);
    let mut first_seen: HashMap<[u8; 32], usize> = HashMap::new();
    let mut replacements: Vec<(usize, String)> = Vec::new();

    for (i, msg) in out.iter().enumerate() {
        if msg["role"].as_str() != Some("tool") {
            continue;
        }
        let Some(content) = msg["content"].as_str() else {
            continue;
        };
        if content.chars().count() <= cfg.dedupe_min_chars
            || already_pruned(content, paired_name(&paired, i))
        {
            continue;
        }
        let key = *blake3::hash(content.as_bytes()).as_bytes();
        match first_seen.get(&key) {
            None => {
                first_seen.insert(key, i);
            }
            // Hash hit: confirm true equality before collapsing (paranoia
            // against collisions — the first occurrence is never rewritten,
            // so comparing against `out[first]` is comparing originals).
            Some(&first) if i < aged && out[first]["content"].as_str() == Some(content) => {
                let n = content.chars().count();
                let line = format!(
                    "[duplicate of message {first}: identical {n}-char tool result elided]"
                );
                if json_str_len(&line) < json_str_len(content) {
                    replacements.push((i, line));
                }
            }
            Some(_) => {}
        }
    }
    for (i, line) in replacements {
        out[i]["content"] = Value::String(line);
    }
    out
}

/// Pass 2: replace aged oversized tool results with per-tool one-liners.
///
/// Each `role:"tool"` message outside the protected tail whose content is
/// longer than [`PruneConfig::summarize_min_chars`] is paired with the
/// `tool_calls` entry that produced it (by `tool_call_id` when present,
/// positionally otherwise — matching both wire dialects the TUI loop uses)
/// and rewritten as e.g. `[run_command] ran 'npm test' -> ok, 47 lines
/// output`. Unpairable (orphaned) results fall back to a generic `[tool]`
/// one-liner.
pub fn summarize_aged_tool_results(messages: &[Value], cfg: &PruneConfig) -> Vec<Value> {
    let mut out = messages.to_vec();
    let aged = cfg.aged_len(out.len());
    let paired = pair_tool_results(&out);

    for (i, msg) in out.iter_mut().enumerate().take(aged) {
        if msg["role"].as_str() != Some("tool") {
            continue;
        }
        let line = {
            let Some(content) = msg["content"].as_str() else {
                continue;
            };
            if content.chars().count() <= cfg.summarize_min_chars
                || already_pruned(content, paired_name(&paired, i))
            {
                continue;
            }
            let (name, args) = match &paired[i] {
                Some(p) => (p.name.as_str(), Some(&p.args)),
                None => ("tool", None),
            };
            let line = one_line_summary(name, args, content);
            if json_str_len(&line) >= json_str_len(content) {
                continue;
            }
            line
        };
        msg["content"] = Value::String(line);
    }
    out
}

/// Pass 3: JSON-aware shrinking of oversized tool-call arguments.
///
/// For aged assistant messages, any `function.arguments` whose serialized
/// form exceeds [`PruneConfig::args_max_chars`] is parsed, long string
/// values are truncated *inside* the structure (recursively, to
/// [`PruneConfig::arg_string_cap`] chars), and the result is reserialized —
/// so the output always parses, and string-typed arguments stay strings
/// while object-typed arguments stay objects. An oversized arguments string
/// that does not parse is replaced with a small valid-JSON placeholder.
pub fn shrink_tool_call_args(messages: &[Value], cfg: &PruneConfig) -> Vec<Value> {
    let mut out = messages.to_vec();
    let aged = cfg.aged_len(out.len());
    let cap = cfg.effective_arg_string_cap();

    for msg in out.iter_mut().take(aged) {
        if msg["role"].as_str() != Some("assistant") {
            continue;
        }
        let Some(tcs) = msg.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for tc in tcs {
            let Some(arguments) = tc.get_mut("function").and_then(|f| f.get_mut("arguments"))
            else {
                continue;
            };
            shrink_arguments(arguments, cfg.args_max_chars, cap);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// A tool call paired to its result message: the tool name plus its parsed
/// arguments (string-encoded arguments are parsed; unparseable ones → Null).
struct PairedCall {
    name: String,
    args: Value,
}

/// For each message index, the tool call that produced it (None for
/// non-results and orphans). Results match their assistant's `tool_calls`
/// by `tool_call_id` when present (OpenAI dialect) or positionally (Ollama
/// dialect, which emits `role:"tool"` results without ids).
fn pair_tool_results(messages: &[Value]) -> Vec<Option<PairedCall>> {
    let mut paired = Vec::with_capacity(messages.len());
    let mut pending: Vec<(String, PairedCall)> = Vec::new();

    for msg in messages {
        let role = msg["role"].as_str().unwrap_or("");
        if role == "tool" {
            let id = msg["tool_call_id"].as_str().unwrap_or("");
            let pos = if id.is_empty() {
                (!pending.is_empty()).then_some(0)
            } else {
                pending.iter().position(|(pid, _)| pid == id)
            };
            paired.push(pos.map(|p| pending.remove(p).1));
            continue;
        }
        pending.clear();
        if role == "assistant" {
            if let Some(tcs) = msg["tool_calls"].as_array() {
                for tc in tcs {
                    let id = tc["id"].as_str().unwrap_or("").to_string();
                    let name = tc["function"]["name"]
                        .as_str()
                        .unwrap_or("tool")
                        .to_string();
                    let args = match &tc["function"]["arguments"] {
                        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                        v => v.clone(),
                    };
                    pending.push((id, PairedCall { name, args }));
                }
            }
        }
        paired.push(None);
    }
    paired
}

fn paired_name(paired: &[Option<PairedCall>], i: usize) -> &str {
    paired[i].as_ref().map_or("tool", |p| p.name.as_str())
}

/// True when `content` is already a pass-1 or pass-2 marker — keeps both
/// passes idempotent even under tiny thresholds.
fn already_pruned(content: &str, paired_name: &str) -> bool {
    content.starts_with("[duplicate of message ")
        || content.starts_with(&format!("[{paired_name}] "))
}

/// Per-tool one-line summary of a tool result (the pass-2 rewrite).
fn one_line_summary(name: &str, args: Option<&Value>, content: &str) -> String {
    let lines = content.lines().count();
    let chars = content.chars().count();
    let status = if looks_like_error(content) {
        "error"
    } else {
        "ok"
    };
    let arg = |key: &str| -> String {
        args.and_then(|a| a.get(key))
            .and_then(Value::as_str)
            .map_or_else(|| "?".to_string(), |s| excerpt(s, 80))
    };
    match name {
        // Note: newt's `(exit N)` empty-output results are always shorter
        // than any one-liner, so the strictly-shorter rule keeps them as-is —
        // no exit-code special case is reachable here.
        "run_command" => {
            format!(
                "[run_command] ran '{}' -> {status}, {lines} lines output",
                arg("command")
            )
        }
        "read_file" => {
            let path = arg("path");
            // Outline/page-map paths only ever read a SOURCE page. An error
            // result (a refusal, a missing-file message) is not source and
            // must stay `-> error, N lines` (#2638 finding 1) — otherwise a
            // sufficiently long error body could be misread as a page map.
            if status == "ok" {
                #[cfg(feature = "ast")]
                if let Some(outline) = rust_outline_summary(&path, args, content) {
                    return outline;
                }
                if let Some(page_map) = read_file_page_map(&path, args, content) {
                    return page_map;
                }
            }
            format!("[read_file] read '{path}' -> {status}, {lines} lines ({chars} chars)",)
        }
        "write_file" => {
            format!(
                "[write_file] wrote '{}' -> {status}, {lines} lines result",
                arg("path")
            )
        }
        "edit_file" => {
            format!(
                "[edit_file] edited '{}' -> {status}, {lines} lines result",
                arg("path")
            )
        }
        "list_dir" => format!(
            "[list_dir] listed '{}' -> {status}, {lines} entries",
            arg("path")
        ),
        "search" => {
            let q = args
                .and_then(|a| a.get("query").or_else(|| a.get("pattern")))
                .and_then(Value::as_str)
                .map_or_else(|| "?".to_string(), |s| excerpt(s, 80));
            format!("[search] searched '{q}' -> {status}, {lines} matching lines")
        }
        "web_fetch" => {
            format!(
                "[web_fetch] fetched '{}' -> {status}, {lines} lines ({chars} chars)",
                arg("url")
            )
        }
        _ => format!("[{name}] result elided -> {status}, {lines} lines ({chars} chars)"),
    }
}

/// A `read_file` one-liner replacement: an outline of the read range instead
/// of a line count (#2557). `None` falls back to the plain one-liner — for a
/// non-`.rs` path, a fragment with no definitions, or the `ast` feature off.
#[cfg(feature = "ast")]
fn rust_outline_summary(path: &str, args: Option<&Value>, content: &str) -> Option<String> {
    if !path.ends_with(".rs") {
        return None;
    }
    let first_line = args
        .and_then(|a| a.get("offset"))
        .and_then(Value::as_u64)
        .filter(|&o| o > 0)
        .unwrap_or(1) as usize;
    // An INCOMING `char_offset` means this page's first line is itself a
    // mid-line fragment (the model resumed a previous page's mid-line cut) —
    // the real lexical prefix of that line isn't in `content` at all, so
    // tree-sitter would tag whatever token happens to start the fragment as
    // if it began the line. Unknown starting lexical boundary, decline
    // unconditionally (#2638 finding 2, round 3).
    let char_offset_in = incoming_char_offset(args);
    if char_offset_in > 0 {
        return None;
    }
    // A paginated/truncated read_file result is `body + "\n\n[<footer>]"`
    // (`output_budget::paginate_read_from`) — the footer is tool-message
    // metadata, not source, so it must not be parsed or counted as lines.
    let (source, footer) = strip_read_footer(content, path, first_line, char_offset_in);
    // Untrusted footer (no tag or bad tag) → fall back to generic (#2638 r4).
    // Trusted footer → proceed: the footer came from this process's producer.
    if footer.as_ref().is_some_and(|f| !f.trusted) {
        return None;
    }
    let entries = crate::ast_outline::outline_rust(source, first_line)?;
    if entries.is_empty() {
        return None;
    }
    let last_line = first_line + source.lines().count().saturating_sub(1);
    let mut body = crate::ast_outline::render_outline(&entries);
    // The page may have been cut mid-definition; error recovery can still
    // tag a partial node ending at the page's last line, which is NOT the
    // definition's real end. Flag it instead of asserting a complete span.
    if footer.is_some() && entries.last().is_some_and(|e| e.end_line >= last_line) {
        body.push_str("\n    (last entry may continue past this page — re-read to confirm)");
    }
    let outline = format!(
        "[read_file] {path} lines {first_line}-{last_line} — outline (re-read any span with \
         offset/limit):\n{body}",
    );
    (json_str_len(&outline) < json_str_len(content)).then_some(outline)
}

/// The literal substring every footer `output_budget::paginate_read_from`
/// emits contains, and no ordinary source tail organically ending in a
/// bracketed line does (#2638 finding 1). All three footer shapes end by
/// naming the exact next call: `call read_file with offset=<n>` (with or
/// without a trailing `char_offset=<n>`).
const FOOTER_MARKER: &str = "call read_file with offset=";

/// What a validated pagination footer says about the page it closes: the
/// producer's own exact `(offset, char_offset)` coordinates for the NEXT
/// `read_file` call — not just a boolean (#2638 finding 2, round 3).
#[derive(Clone)]
struct ReadFooter {
    /// The next call's `offset` (every footer names one).
    next_offset: usize,
    /// The next call's `char_offset` — set only when the cut lands INSIDE a
    /// line, not on a line boundary (#2638 finding 3).
    next_char_offset: Option<usize>,
    /// The 32-hex MAC tag from the footer, if present.
    tag: Option<String>,
    /// Set after tag verification: this footer came from THIS process's
    /// own producer (not a source tail that happens to match the grammar).
    trusted: bool,
}

/// The `char_offset` the model passed on THIS call, if any and nonzero — the
/// value it read off a previous page's own footer to resume a mid-line cut.
/// Not part of `read_file`'s public schema; only ever correct when echoed
/// from a footer (`output_budget::paginate_read_from`'s doc comment). Its
/// presence means the page's first line is itself a mid-line fragment.
fn incoming_char_offset(args: Option<&Value>) -> usize {
    args.and_then(|a| a.get("char_offset"))
        .and_then(Value::as_u64)
        .filter(|&c| c > 0)
        .map(|c| c as usize)
        .unwrap_or(0)
}

/// Strip the `\n\n[<footer>]` pagination/truncation notice a `read_file`
/// result carries (`output_budget::paginate_read_from`), if present. Returns
/// the body with the candidate stripped and the parsed footer when the
/// trailing bracketed line matches the producer's exact footer grammar. Sets
/// `footer.trusted` by verifying the MAC tag against `path`, `first_line`,
/// `char_offset_in`, and `body` — trusted means this footer came from THIS
/// process's own producer. Untrusted (no tag, bad tag, or a lookalike with
/// no tag) gives the arithmetic-checked footer struct but with `trusted=false`;
/// callers fall back to the r4 generic summary for untrusted footers.
fn strip_read_footer<'a>(
    content: &'a str,
    path: &str,
    first_line: usize,
    char_offset_in: usize,
) -> (&'a str, Option<ReadFooter>) {
    match content.rsplit_once("\n\n[") {
        Some((body, tail)) => match tail.strip_suffix(']') {
            Some(f) if !f.contains('\n') => match parse_read_footer(f) {
                Some(mut footer) if footer_matches_body(&footer, first_line, body) => {
                    footer.trusted = footer.tag.as_deref().is_some_and(|tag| {
                        crate::agentic::tools::output_budget::verify_page_tag(
                            path,
                            first_line,
                            char_offset_in,
                            body,
                            footer.next_offset,
                            footer.next_char_offset.unwrap_or(0),
                            tag,
                        )
                    });
                    (body, Some(footer))
                }
                _ => (content, None),
            },
            _ => (content, None),
        },
        None => (content, None),
    }
}

/// #2638 review round 6: does `content`'s OWN final line look like a
/// pagination footer for `path`, at `first_line=1`/`char_offset_in=0` (the
/// whole-file passthrough's fixed coordinates), with a tag that VERIFIES?
///
/// Called from `output_budget::paginate_read_from`'s whole-file passthrough
/// to catch the replay `verify_page_tag`'s own doc names: a genuine
/// paginated page (body + its real, verifying footer) saved back to the
/// SAME path and then read WHOLE reproduces every MAC input, so the tag
/// verifies even though this footer is now real source, not this call's
/// metadata. `strip_read_footer` already considers only the content's own
/// final line (`rsplit_once("\n\n[")` finds the last such split, and the
/// candidate must run to the content's end with `strip_suffix(']')` and no
/// embedded newline) — this wraps that same check, it does not re-implement
/// footer parsing.
pub(crate) fn final_line_is_a_verified_pagination_footer(content: &str, path: &str) -> bool {
    strip_read_footer(content, path, 1, 0)
        .1
        .is_some_and(|f| f.trusted)
}

/// Parse a bracketed footer line's exact tail:
/// `…call read_file with offset=<n>[ char_offset=<m>] to continue[ page=<32hex>]`
/// with `<n>`/`<m>` valid `usize` and the 32-hex tag optional. `None` for
/// anything that merely contains [`FOOTER_MARKER`] without the rest of the
/// producer's exact grammar.
fn parse_read_footer(f: &str) -> Option<ReadFooter> {
    let after_marker = f.rsplit_once(FOOTER_MARKER)?.1;
    // Strip optional trailing ` page=<32hex>` before the core parse.
    let (core, tag) = match after_marker.rsplit_once(" page=") {
        Some((rest, t)) if t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit()) => {
            (rest, Some(t.to_string()))
        }
        _ => (after_marker, None),
    };
    let core = core.strip_suffix(" to continue")?;
    match core.split_once(" char_offset=") {
        Some((offset_s, char_offset_s)) => Some(ReadFooter {
            next_offset: offset_s.parse().ok()?,
            next_char_offset: Some(char_offset_s.parse().ok()?),
            tag,
            trusted: false,
        }),
        None => Some(ReadFooter {
            next_offset: core.parse().ok()?,
            next_char_offset: None,
            tag,
            trusted: false,
        }),
    }
}

/// Does the footer's claimed next-call coordinate match what the candidate
/// `body` and the page's own `first_line` actually imply? A non-mid-line
/// footer always names one past the last line `body` shows; a mid-line
/// footer always names the SAME line `body`'s single (partial) line is —
/// see `output_budget::paginate_read_from`'s `whole_through`/`mid_line`
/// arithmetic, which this mirrors.
fn footer_matches_body(footer: &ReadFooter, first_line: usize, body: &str) -> bool {
    let body_lines = body.lines().count();
    let expected = if footer.next_char_offset.is_some() {
        first_line + body_lines.saturating_sub(1)
    } else {
        first_line + body_lines
    };
    footer.next_offset == expected
}

/// Language-neutral `read_file` fallback (#2638): when there is no outline
/// engine for this page (a non-`.rs` path, or a `.rs` fragment with no
/// tagged definitions), keep a page map of fixed-size line spans instead of
/// a bare line count — no parsing, no regex, just the real source page's
/// line accounting (the same [`strip_read_footer`] split the outline path
/// uses), so the model still knows where to re-read. Default-on: unlike
/// [`rust_outline_summary`] this needs no grammar. `None` falls back to the
/// plain one-liner (empty page, or the map isn't strictly shorter).
fn read_file_page_map(path: &str, args: Option<&Value>, content: &str) -> Option<String> {
    const CHUNK: usize = 400;
    let first_line = args
        .and_then(|a| a.get("offset"))
        .and_then(Value::as_u64)
        .filter(|&o| o > 0)
        .unwrap_or(1) as usize;
    let char_offset_in = incoming_char_offset(args);
    let (source, footer) = strip_read_footer(content, path, first_line, char_offset_in);
    // Untrusted footer (no tag or bad tag) → fall back to generic (#2638 r4).
    // Trusted footer → proceed with exact coordinates (#2638 r5).
    if footer.as_ref().is_some_and(|f| !f.trusted) {
        return None;
    }
    let total_lines = source.lines().count();
    if total_lines == 0 {
        return None;
    }
    let last_line = first_line + total_lines - 1;
    let spans = (first_line..=last_line)
        .step_by(CHUNK)
        .map(|start| {
            let end = (start + CHUNK - 1).min(last_line);
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    // A mid-line cut has no exact resume point in offset/limit terms — say so
    // WITH the producer's own exact coordinates, rather than let the
    // offset/limit advice imply the last line is whole, or name the caveat
    // without the value needed to act on it (#2638 finding 2, round 3: the
    // prior caveat text named no coordinate at all).
    let mid_line_note = match footer.and_then(|f| f.next_char_offset.map(|co| (f.next_offset, co)))
    {
        Some((offset, char_offset)) => format!(
            " (line {last_line} continues mid-line; re-read with offset={offset} \
             char_offset={char_offset})"
        ),
        None => String::new(),
    };
    let page_map = format!(
        "[read_file] {path} lines {first_line}-{last_line} (page map; re-read any span with \
         offset/limit): {spans}{mid_line_note}",
    );
    (json_str_len(&page_map) < json_str_len(content)).then_some(page_map)
}

/// Does `content` look like a [`one_line_summary`] this module produced?
///
/// **Lives beside the builder deliberately (#1992).** The digest fold needs to
/// know whether a tool result has ALREADY been one-lined — possibly rounds
/// ago, so it cannot be told by a return value and must read the text. A
/// recognizer written at the consumer would be a second definition of this
/// module's output shape, free to drift from the emitter the moment an arm
/// changes wording. One grammar, one file, and a round-trip test that drives
/// every arm of the builder through this function.
///
/// The grammar every arm shares: a `[name]` tag, then a clause, then
/// `-> ok,` or `-> error,`. Deliberately conservative — a raw tool result that
/// happened to open with a bracket still has to carry the arrow-status clause
/// to be mistaken for a summary, and a false positive here folds away a
/// verbatim result.
#[must_use]
pub fn is_one_line_summary(content: &str) -> bool {
    let t = content.trim_start();
    if !t.starts_with('[') {
        return false;
    }
    let Some(rest) = t.split_once("] ").map(|(_, rest)| rest) else {
        return false;
    };
    // One line only: a summary is a summary.
    if rest.contains('\n') {
        return false;
    }
    // The page-map fallback (#2638) carries no `-> ok,`/`-> error,` clause —
    // it has its own literal marker, which only `read_file_page_map` emits.
    // The multiline Rust outline is deliberately NOT recognized here: it is
    // filtered out by the single-line check above, and stays already-pruned
    // via its `[read_file] ` prefix (`already_pruned`) rather than folding.
    const PAGE_MAP_MARKER: &str = " (page map; re-read any span with offset/limit): ";
    rest.contains(" -> ok, ") || rest.contains(" -> error, ") || rest.contains(PAGE_MAP_MARKER)
}

/// First `max_chars` chars with newlines flattened, `…`-terminated if cut.
fn excerpt(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= max_chars {
        cleaned
    } else {
        let head: String = cleaned.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

/// Matches the error shapes `execute_tool` produces (`error…`,
/// `capability denied…`).
fn looks_like_error(content: &str) -> bool {
    let t = content.trim_start();
    t.starts_with("error") || t.starts_with("Error") || t.starts_with("capability denied")
}

/// Shrink one `function.arguments` value in place (pass-3 core). String-typed
/// arguments are parsed/shrunk/reserialized and stay strings; structured
/// arguments are shrunk directly. A rewrite is only applied when strictly
/// shorter serialized.
fn shrink_arguments(arguments: &mut Value, max_chars: usize, cap: usize) {
    match arguments {
        Value::String(s) => {
            if s.chars().count() <= max_chars {
                return;
            }
            let shrunk = match serde_json::from_str::<Value>(s) {
                Ok(mut v) => {
                    shrink_value(&mut v, cap);
                    v
                }
                Err(_) => {
                    // Not JSON to begin with — substitute a small valid-JSON
                    // placeholder rather than slicing bytes (hermes #11762).
                    let n = s.chars().count();
                    let mut ph = serde_json::json!({
                        "truncated": format!("original arguments were not valid JSON ({n} chars elided)"),
                    });
                    shrink_value(&mut ph, cap);
                    ph
                }
            };
            let new = serde_json::to_string(&shrunk).unwrap_or_else(|_| "{}".to_string());
            if json_str_len(&new) < json_str_len(s) {
                *arguments = Value::String(new);
            }
        }
        other => {
            if json_len(other) <= max_chars {
                return;
            }
            let mut v = other.clone();
            if shrink_value(&mut v, cap) && json_len(&v) < json_len(other) {
                *other = v;
            }
        }
    }
}

/// Recursively truncate long string values inside a parsed JSON structure.
/// Returns true when anything changed.
fn shrink_value(v: &mut Value, cap: usize) -> bool {
    match v {
        Value::String(s) => match truncate_chars(s, cap) {
            Some(t) => {
                *s = t;
                true
            }
            None => false,
        },
        // Explicit loops, not `.any()`: every nested value must be visited
        // (shrink_value mutates), and `.any()` would short-circuit after the
        // first hit, leaving later strings unshrunk.
        Value::Array(items) => {
            let mut changed = false;
            for it in items.iter_mut() {
                changed |= shrink_value(it, cap);
            }
            changed
        }
        Value::Object(map) => {
            let mut changed = false;
            for it in map.values_mut() {
                changed |= shrink_value(it, cap);
            }
            changed
        }
        _ => false,
    }
}

/// Char-boundary-safe truncation to at most `cap` chars including the
/// `… [+N chars]` marker (None when already within `cap`). Reserves marker
/// space using the *total* count, whose digit count bounds the omitted
/// count's — so the result never exceeds `cap`, which keeps repeated
/// applications stable.
fn truncate_chars(s: &str, cap: usize) -> Option<String> {
    let total = s.chars().count();
    if total <= cap {
        return None;
    }
    let marker_reserve = format!("… [+{total} chars]").chars().count();
    let keep = cap.saturating_sub(marker_reserve);
    let head: String = s.chars().take(keep).collect();
    let omitted = total - keep;
    Some(format!("{head}… [+{omitted} chars]"))
}

/// Serialized length of one JSON value.
fn json_len(v: &Value) -> usize {
    serde_json::to_string(v).map_or(0, |s| s.len())
}

/// Serialized length of a string as a JSON string (quotes + escapes) — the
/// exact cost of a `content` value inside its message.
fn json_str_len(s: &str) -> usize {
    json_len(&Value::String(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- builders ----------------------------------------------------------

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    fn assistant_text(text: &str) -> Value {
        json!({"role": "assistant", "content": text})
    }

    /// Ollama dialect: no ids, `arguments` is a JSON object.
    fn assistant_calls(calls: &[(&str, Value)]) -> Value {
        let tcs: Vec<Value> = calls
            .iter()
            .map(|(name, args)| json!({"function": {"name": name, "arguments": args}}))
            .collect();
        json!({"role": "assistant", "content": "", "tool_calls": tcs})
    }

    /// OpenAI dialect: ids, `arguments` is a serialized JSON string.
    fn assistant_calls_openai(calls: &[(&str, &str, Value)]) -> Value {
        let tcs: Vec<Value> = calls
            .iter()
            .map(|(id, name, args)| {
                json!({"id": id, "type": "function",
                       "function": {"name": name, "arguments": args.to_string()}})
            })
            .collect();
        json!({"role": "assistant", "content": "", "tool_calls": tcs})
    }

    fn tool_result(content: &str) -> Value {
        json!({"role": "tool", "content": content})
    }

    fn tool_result_id(id: &str, content: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": content})
    }

    /// `n` distinct lines, ~13 chars each.
    fn text_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("test line {i:03}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pad_tail(msgs: &mut Vec<Value>, n: usize) {
        for i in 0..n {
            msgs.push(user(&format!("tail filler {i}")));
        }
    }

    fn content_of(msg: &Value) -> &str {
        msg["content"].as_str().unwrap()
    }

    // -- pass 1: duplicate collapse -----------------------------------------

    #[test]
    fn dedupe_collapses_later_identical_aged_result() {
        let big = text_lines(40); // ~560 chars
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("run_command", json!({"command": "cargo test"}))]),
            tool_result(&big),
            assistant_calls(&[("run_command", json!({"command": "cargo test"}))]),
            tool_result(&big),
        ];
        pad_tail(&mut msgs, 10);
        let out = collapse_duplicate_tool_results(&msgs, &PruneConfig::default());
        // First occurrence untouched; later one collapsed with a reference.
        assert_eq!(content_of(&out[2]), big);
        assert_eq!(
            content_of(&out[4]),
            format!(
                "[duplicate of message 2: identical {}-char tool result elided]",
                big.chars().count()
            ),
        );
    }

    #[test]
    fn dedupe_ignores_short_results_and_non_tool_roles() {
        let small = "short duplicate";
        let big = text_lines(40);
        let mut msgs = vec![
            user(&big), // non-tool role with big duplicate content
            tool_result(small),
            tool_result(small),
            user(&big),
        ];
        pad_tail(&mut msgs, 4);
        let cfg = PruneConfig {
            keep_last: 4,
            ..PruneConfig::default()
        };
        let out = collapse_duplicate_tool_results(&msgs, &cfg);
        assert_eq!(out, msgs);
    }

    #[test]
    fn dedupe_never_touches_protected_tail() {
        let big = text_lines(40);
        let mut msgs = vec![user("task"), tool_result(&big)];
        pad_tail(&mut msgs, 3);
        msgs.push(tool_result(&big)); // duplicate, but inside the tail
        let cfg = PruneConfig {
            keep_last: 2,
            ..PruneConfig::default()
        };
        let out = collapse_duplicate_tool_results(&msgs, &cfg);
        assert_eq!(content_of(out.last().unwrap()), big);
    }

    #[test]
    fn dedupe_leaves_distinct_results_alone() {
        let mut msgs = vec![
            user("task"),
            tool_result(&text_lines(40)),
            tool_result(&text_lines(41)),
        ];
        pad_tail(&mut msgs, 10);
        let out = collapse_duplicate_tool_results(&msgs, &PruneConfig::default());
        assert_eq!(out, msgs);
    }

    // -- pass 2: per-tool one-liners ----------------------------------------

    /// Build `[user, assistant_call, tool_result, …tail]`, run pass 2, and
    /// return the rewritten result content.
    fn summarize_one(name: &str, args: Value, content: &str) -> String {
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[(name, args)]),
            tool_result(content),
        ];
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &PruneConfig::default());
        content_of(&out[2]).to_string()
    }

    #[test]
    fn one_liner_run_command() {
        let line = summarize_one(
            "run_command",
            json!({"command": "npm test"}),
            &text_lines(47),
        );
        assert_eq!(line, "[run_command] ran 'npm test' -> ok, 47 lines output");
    }

    #[test]
    fn one_liner_run_command_error() {
        let content = format!("error: build failed\n{}", text_lines(30));
        let line = summarize_one("run_command", json!({"command": "cargo build"}), &content);
        assert_eq!(
            line,
            "[run_command] ran 'cargo build' -> error, 31 lines output"
        );
    }

    #[test]
    fn one_liner_never_replaces_with_something_longer() {
        // `(exit 0)` (run_command's empty-output result) is shorter than any
        // one-liner — the strictly-shorter rule must leave it alone even when
        // the threshold would otherwise fire.
        let cfg = PruneConfig {
            summarize_min_chars: 4,
            ..PruneConfig::default()
        };
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("run_command", json!({"command": "true"}))]),
            tool_result("(exit 0)"),
        ];
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &cfg);
        assert_eq!(content_of(&out[2]), "(exit 0)");
    }

    /// #2638: `read_file`'s default fallback is the page map (not the plain
    /// line-count one-liner) once content clears the shrink threshold — even
    /// for a `.rs` path, when the content isn't real parseable Rust (or the
    /// `ast` feature is off) so the outline path declines.
    #[test]
    fn one_liner_read_file() {
        let content = text_lines(120);
        let line = summarize_one("read_file", json!({"path": "src/main.rs"}), &content);
        assert_eq!(
            line,
            "[read_file] src/main.rs lines 1-120 (page map; re-read any span with \
             offset/limit): 1-120"
        );
    }

    /// #2557 regression: an aged `read_file` result on a real `.rs` file
    /// becomes a tree-sitter outline whose spans match the source — not a
    /// bare line count. Would have failed before `rust_outline_summary`
    /// existed (the old code always produced the `-> ok, N lines` one-liner).
    #[cfg(feature = "ast")]
    #[test]
    fn aged_rust_read_becomes_an_outline_with_matching_spans() {
        let source = "\
fn compact_responses_input(x: u32) -> u32 {
    // a real read would carry many lines of body here — pad past the
    // pass-2 threshold so the rewrite actually fires.
    let mut y = x;
    y += 1;
    y += 1;
    y
}

pub(crate) enum ResponsesCompaction {
    Kept,
    Dropped,
}
";
        let line = summarize_one("read_file", json!({"path": "src/agentic/mod.rs"}), source);
        assert!(
            line.starts_with("[read_file] src/agentic/mod.rs lines 1-13 — outline"),
            "got: {line}"
        );
        assert!(line.contains("    1-8\tfn compact_responses_input(x: u32) -> u32"));
        assert!(line.contains("    10-13\tpub(crate) enum ResponsesCompaction"));
    }

    /// #2638 fix, case 1 (round 5): a REAL paginated Rust page whose footer's
    /// MAC tag verifies against the exact path the model's call used — the
    /// producer and the caller agree on `path`, so the tag authenticates
    /// origin and the outline fires with the page's own exact coordinates.
    /// Round 4's generic fallback only applied to UNVERIFIABLE footers; this
    /// is the trusted-footer counterpart. Red before round 5: this test
    /// fails against the round-4 code (which declines every footer
    /// unconditionally) with a generic summary instead of an outline.
    ///
    /// (`[showing lines 3-10 of 18; call read_file with offset=11 to
    /// continue page=<tag>]` at first_line=3, 8-line body → expected offset
    /// = 3+8=11 ✓, and the tag was minted for THIS path/body/coordinates.)
    #[cfg(feature = "ast")]
    #[test]
    fn a_trusted_paginated_rust_page_gets_an_outline_with_exact_coordinates() {
        let full_source = "\
mod header;

fn first(x: u32) -> u32 {
    x + 1
}

fn second(y: u32) -> u32 {
    // pad this body so pass 2's rewrite threshold is cleared once
    // paginated down to just this page.
    let mut z = y;
    z += 1;
    z += 1;
    z
}

fn third(z: u32) -> u32 {
    z
}
";
        let path = "src/lib.rs";
        // offset=3, limit=12 -> real page is lines 3-14; footer offset=15.
        // fn second spans lines 7-14 so limit must be at least 12 for the
        // AST to see the complete closing brace.
        let page = crate::agentic::tools::output_budget::paginate_read_from(
            path,
            full_source,
            Some(3),
            Some(12),
            0,
            None,
        );
        assert!(
            page.contains("[showing lines 3-14 of 18"),
            "test fixture didn't paginate as expected: {page}"
        );

        let line = summarize_one("read_file", json!({"path": path, "offset": 3}), &page);
        assert!(
            line.starts_with("[read_file] src/lib.rs lines 3-14 — outline"),
            "trusted footer (matching path) must produce an outline with exact \
             coordinates, got: {line}"
        );
        // `fn second` starts inside this page (line 7) but its closing brace
        // is past the page's last line (10) — the page cuts it mid-function,
        // so tree-sitter's error recovery does not tag an incomplete
        // definition. Only the one COMPLETE definition inside the page
        // (`fn first`, lines 3-5) is entry-worthy; this is real,
        // page-boundary-correct behavior, not a test artifact.
        assert!(
            line.contains("3-5\tfn first(x: u32) -> u32"),
            "outline must cover the page's real, complete content: {line}"
        );
    }

    /// #2638 round 4 (still holds under round 5's trust check): a paginated
    /// page whose footer has NO tag at all (a hand-built or pre-#2638
    /// footer shape) is untrusted regardless of arithmetic — generic
    /// summary.
    #[cfg(feature = "ast")]
    #[test]
    fn a_footer_with_no_tag_is_untrusted_and_gives_generic_summary() {
        let full_source = "\
mod header;

fn first(x: u32) -> u32 {
    x + 1
}

fn second(y: u32) -> u32 {
    // pad this body so pass 2's rewrite threshold is cleared once
    // paginated down to just this page.
    let mut z = y;
    z += 1;
    z += 1;
    z
}

fn third(z: u32) -> u32 {
    z
}
";
        // Hand-built footer: valid grammar, valid arithmetic, no `page=` tag.
        let page = format!(
            "{}\n\n[showing lines 3-10 of 18; call read_file with offset=11 to continue]",
            full_source
                .lines()
                .skip(2)
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        );

        let line = summarize_one(
            "read_file",
            json!({"path": "src/lib.rs", "offset": 3}),
            &page,
        );
        assert!(
            line.starts_with("[read_file] read 'src/lib.rs' ->"),
            "an untagged footer must give a generic summary, got: {line}"
        );
        assert!(
            !line.contains("— outline"),
            "must not produce an outline: {line}"
        );
        assert!(
            !line.contains("page map"),
            "must not produce a page map: {line}"
        );
    }

    /// #2638 round 5: a footer carrying a FORGED tag (right shape, wrong
    /// value) must be untrusted — the MAC, not the grammar, is what
    /// authenticates origin.
    #[cfg(feature = "ast")]
    #[test]
    fn a_footer_with_a_forged_tag_is_untrusted_and_gives_generic_summary() {
        let full_source = "\
mod header;

fn first(x: u32) -> u32 {
    x + 1
}

fn second(y: u32) -> u32 {
    let mut z = y;
    z += 1;
    z += 1;
    z
}

fn third(z: u32) -> u32 {
    z
}
";
        let path = "src/lib.rs";
        let real_page = crate::agentic::tools::output_budget::paginate_read_from(
            path,
            full_source,
            Some(3),
            Some(8),
            0,
            None,
        );
        // Forgery: replace the entire 32-hex tag with a fixed, definitely-wrong
        // one of the same length/shape (same grammar, wrong MAC value).
        let (before, tag_and_after) = real_page.rsplit_once("page=").expect("footer has a tag");
        let after = &tag_and_after[32..];
        let forged_page = format!("{before}page={}{after}", "0".repeat(32));
        assert_ne!(
            forged_page, real_page,
            "the forged page must differ from the real one"
        );

        let line = summarize_one(
            "read_file",
            json!({"path": path, "offset": 3}),
            &forged_page,
        );
        assert!(
            line.starts_with("[read_file] read 'src/lib.rs' ->"),
            "a forged tag must give a generic summary, got: {line}"
        );
        assert!(
            !line.contains("— outline"),
            "must not produce an outline: {line}"
        );
    }

    /// #2638 round 5: a REAL footer moved onto a DIFFERENT body — the tag
    /// binds to the body bytes, so pasting a genuine footer after unrelated
    /// content must not verify.
    #[cfg(feature = "ast")]
    #[test]
    fn a_real_footer_moved_onto_a_different_body_is_untrusted() {
        let full_source = "\
mod header;

fn first(x: u32) -> u32 {
    x + 1
}

fn second(y: u32) -> u32 {
    let mut z = y;
    z += 1;
    z += 1;
    z
}

fn third(z: u32) -> u32 {
    z
}
";
        let path = "src/lib.rs";
        let real_page = crate::agentic::tools::output_budget::paginate_read_from(
            path,
            full_source,
            Some(3),
            Some(8),
            0,
            None,
        );
        let (_, footer_bracket) = real_page.rsplit_once("\n\n[").expect("has a footer");
        let different_body: String = (1..=8)
            .map(|i| format!("// unrelated line {i}\n"))
            .collect();
        let different_body = different_body.trim_end_matches('\n');
        assert_eq!(
            different_body.lines().count(),
            8,
            "the swapped body must have the same line count as the real page's body \
             so only the CONTENT differs, not the arithmetic"
        );
        let spliced_page = format!("{different_body}\n\n[{footer_bracket}");

        let line = summarize_one(
            "read_file",
            json!({"path": path, "offset": 3}),
            &spliced_page,
        );
        assert!(
            line.starts_with("[read_file] read 'src/lib.rs' ->"),
            "a real footer glued onto a different body must give a generic summary, got: {line}"
        );
        assert!(
            !line.contains("— outline"),
            "must not produce an outline: {line}"
        );
    }

    /// #2638 review round 6 (P2): the replay the reviewer's counterexample
    /// names — a genuine paginated page (body + its real, MAC-verifying
    /// footer) SAVED BACK to the same path, then read WHOLE in the same
    /// process. Every MAC input reproduces exactly, so `verify_page_tag`
    /// verifies the tag — but this footer is now real source, not live
    /// pagination metadata. `paginate_read_from`'s whole-file passthrough
    /// must append the plain disambiguator line rather than let `prune.rs`
    /// reinterpret the bracketed line as a footer to strip.
    #[test]
    fn a_saved_paginated_page_replayed_whole_is_not_mistaken_for_a_live_footer() {
        let path = "notes.md";
        let full_source = text_lines(300);
        // Paginate lines 1-200 at offset 1 — a real page with a real footer.
        let saved_page = crate::agentic::tools::output_budget::paginate_read_from(
            path,
            &full_source,
            Some(1),
            Some(200),
            0,
            None,
        );
        assert!(
            saved_page.contains("call read_file with offset=201"),
            "fixture must paginate as expected: {saved_page}"
        );
        let saved_page_lines = saved_page.lines().count(); // 200 real + 1 footer line

        // "Save that exact page back to the SAME path, then read it whole in
        // the same process" — the saved page IS now the file's full content,
        // and it fits under budget (no cap, no offset) so this hits the
        // whole-file passthrough.
        let replayed = crate::agentic::tools::output_budget::paginate_read_from(
            path,
            &saved_page,
            None,
            None,
            0,
            None,
        );
        assert!(
            replayed.ends_with("[end of file: the bracketed line above is part of the file]"),
            "a whole-file replay of a saved page must append the disambiguator: {replayed}"
        );

        let line = summarize_one("read_file", json!({"path": path}), &replayed);
        // Every line — the 200 real lines, the replayed footer line, AND the
        // appended disambiguator — must count as real source.
        let expected_total = saved_page_lines + 1;
        assert!(
            line.contains(&format!("lines 1-{expected_total}")),
            "must count every source line, including the replayed footer and the \
             appended disambiguator: {line}"
        );
        assert!(
            !line.contains(FOOTER_MARKER),
            "a replayed footer must never be reinterpreted as live pagination metadata: {line}"
        );
    }

    /// #2638 review round 6: `strip_read_footer` must consider ONLY the
    /// result's OWN FINAL line as a candidate footer — confirmed and tested
    /// per the review, not just asserted in a doc comment. A real,
    /// MAC-verifying footer line followed by MORE real content is not the
    /// final line, so it must stay source: `tail.strip_suffix(']')` fails
    /// once anything follows the closing bracket.
    #[test]
    fn a_verified_footer_shape_not_at_the_end_is_left_as_source() {
        let path = "notes.md";
        let full_source = text_lines(300);
        let page = crate::agentic::tools::output_budget::paginate_read_from(
            path,
            &full_source,
            Some(1),
            Some(200),
            0,
            None,
        );
        // A real, tag-verifying footer line — but NOT the final line, because
        // more real content follows it.
        let content = format!("{page}\ntest line 900\ntest line 901");
        let real_lines = content.lines().count();
        let line = summarize_one("read_file", json!({"path": path, "offset": 1}), &content);
        assert!(
            line.contains(&format!("lines 1-{real_lines}")),
            "a footer-shaped line that is not the final line must stay source: {line}"
        );
        assert!(
            !line.contains(FOOTER_MARKER),
            "must not be treated as live pagination metadata: {line}"
        );
    }

    /// #2638 review round 6, positive control: the disambiguator only fires
    /// on the whole-file passthrough path. A genuine paginated read (the
    /// SAME source, same path, same starting coordinates as the replay test
    /// above, but NOT saved-and-reread) still gets the precise, trusted page
    /// map for exactly the page it covers.
    #[test]
    fn genuine_pagination_of_the_same_source_still_gets_a_precise_page_map() {
        let path = "notes.md";
        let full_source = text_lines(300);
        let page = crate::agentic::tools::output_budget::paginate_read_from(
            path,
            &full_source,
            Some(1),
            Some(200),
            0,
            None,
        );
        let line = summarize_one("read_file", json!({"path": path, "offset": 1}), &page);
        assert!(
            line.contains("lines 1-200 (page map"),
            "genuine pagination must still produce a precise, trusted page map: {line}"
        );
    }

    /// #2638 fix (round 5 update): a hand-built footer that passes the
    /// arithmetic cross-check but carries NO MAC tag still falls back to the
    /// generic summary — the tag, not the arithmetic, is what authenticates
    /// origin.
    #[cfg(feature = "ast")]
    #[test]
    fn paginated_page_with_valid_arithmetic_footer_gives_generic_summary() {
        // `[showing lines 5-5 of 40; call read_file with offset=6 to continue]`
        // parses: next_offset=6, no char_offset. first_line=5, body has 1 line
        // → expected = 5+1 = 6 ✓. Arithmetic passes; still ambiguous.
        let padding = "x".repeat(220);
        let page = format!(
            "mod header; // {padding}\n\n\
             [showing lines 5-5 of 40; call read_file with offset=6 to continue]"
        );
        let line = summarize_one(
            "read_file",
            json!({"path": "src/lib.rs", "offset": 5}),
            &page,
        );
        assert!(
            line.starts_with("[read_file] read 'src/lib.rs' ->"),
            "paginated page with valid-arithmetic footer must give generic summary, got: {line}"
        );
        assert!(
            !line.contains("— outline"),
            "must not produce an outline: {line}"
        );
    }

    /// #2638, page-map fallback: a non-Rust path has no outline engine, so
    /// an aged read keeps a page map of fixed-size spans instead of a bare
    /// line count — default-on, no `ast` feature needed.
    #[test]
    fn aged_non_rust_read_becomes_a_page_map() {
        let content = text_lines(900); // 900 lines, no trailing newline
        let line = summarize_one("read_file", json!({"path": "notes.md"}), &content);
        assert_eq!(
            line,
            "[read_file] notes.md lines 1-900 (page map; re-read any span with \
             offset/limit): 1-400, 401-800, 801-900"
        );
    }

    /// #2638 review finding 1: a real (non-Rust) source file ending in a
    /// blank line then a bracketed final section — `\n\n[section]` — must
    /// NOT be mistaken for `output_budget`'s pagination footer. Before the
    /// fix, `strip_read_footer` treated ANY trailing single-line bracket as
    /// a footer and dropped that real final section from the page's line
    /// count and page map.
    #[test]
    fn a_bracketed_source_tail_is_not_mistaken_for_a_pagination_footer() {
        let mut content = text_lines(900);
        content.push_str("\n\n[unrelated section header]");
        let real_lines = content.lines().count();
        let line = summarize_one("read_file", json!({"path": "notes.md"}), &content);
        assert!(
            line.contains(&format!("lines 1-{real_lines}")),
            "the real bracketed final line was dropped from the page's line count: {line}"
        );
    }

    /// #2638 round 3, finding 1: a source tail that carries the producer's
    /// exact MARKER TEXT (`call read_file with offset=N to continue`) but
    /// whose number is arithmetically wrong for the real page — the
    /// realistic accidental case, not a deliberately byte-identical forgery
    /// — must still not be mistaken for a real footer. Round 2's fix
    /// (`FOOTER_MARKER` substring + grammar) alone accepts this: `offset=42`
    /// parses fine and the line matches the grammar exactly. Only the round
    /// 3 cross-check (the footer's claimed next `offset` must equal
    /// `first_line + body's own line count`) catches it — 900 real lines at
    /// `first_line=1` implies `offset=901`, not `42`.
    #[test]
    fn a_footer_shaped_tail_with_the_wrong_arithmetic_is_not_mistaken_for_a_real_footer() {
        let mut content = text_lines(900);
        content.push_str("\n\n[call read_file with offset=42 to continue]");
        let real_lines = content.lines().count();
        let line = summarize_one("read_file", json!({"path": "notes.md"}), &content);
        assert!(
            line.contains(&format!("lines 1-{real_lines}")),
            "a footer-shaped tail with the wrong coordinate was still stripped as metadata: {line}"
        );
    }

    /// #2638 round 5 update: a source tail that passes BOTH the grammar
    /// check AND the arithmetic cross-check but carries no MAC tag — the
    /// hardest ambiguous case without authentication — must still give the
    /// generic summary. `text_lines(200)` + a footer naming `offset=201`
    /// passes the arithmetic: first_line=1, body.lines()=200, expected=201.
    #[test]
    fn an_ambiguous_tail_passing_arithmetic_gives_generic_summary() {
        let mut content = text_lines(200);
        // Exact arithmetic match: first_line=1, body=200 lines → expected=201.
        content.push_str("\n\n[call read_file with offset=201 to continue]");
        let line = summarize_one("read_file", json!({"path": "notes.md"}), &content);
        assert!(
            line.starts_with("[read_file] read 'notes.md' ->"),
            "ambiguous arithmetic-matching footer must give generic summary: {line}"
        );
        assert!(
            !line.contains("page map"),
            "must not produce a page map: {line}"
        );
    }

    /// #2638 review finding 1: a long read-error result must stay
    /// `-> error, N lines` and never be reinterpreted as a source page map
    /// (or outline) once it clears the shrink threshold.
    #[test]
    fn a_long_read_error_is_never_reinterpreted_as_a_page_map() {
        let content = format!("error: permission denied\n{}", text_lines(900));
        let line = summarize_one("read_file", json!({"path": "notes.md"}), &content);
        assert!(
            line.starts_with("[read_file] read 'notes.md' -> error, "),
            "an error result was folded into a page map instead of staying an error one-liner: \
             {line}"
        );
    }

    /// #2638 review finding 2: the digest fold's `is_one_line_summary`
    /// predicate must recognize the page-map form the builder actually
    /// emits for a non-Rust (or unparseable) read — not just the old
    /// `-> ok,`/`-> error,` grammar — or aged page-map rounds never
    /// qualify for folding.
    #[test]
    fn digest_fold_recognizes_the_real_page_map_form() {
        let content = text_lines(900);
        let line = summarize_one("read_file", json!({"path": "notes.md"}), &content);
        assert!(
            is_one_line_summary(&line),
            "the page map the builder emits is not recognized by the digest fold: {line:?}"
        );
    }

    /// #2638 fix, case 2 (round 5): an INITIAL long-line truncation — the
    /// real producer (`output_budget::paginate_read_from`) cutting a single
    /// line too long for the budget, with a TRUSTED footer (matching path).
    /// The page-map path's mid-line caveat must carry the exact resume
    /// coordinates the producer minted — offset=1, char_offset=300 for
    /// budget=100 at 3 c/t. Red before round 5 (r4 code): generic summary,
    /// no coordinates.
    #[test]
    fn a_trusted_initial_long_line_truncation_keeps_its_exact_char_offset_coordinates() {
        let long_line = "y".repeat(2_000);
        let path = "notes.md";
        // 100 tokens × 3 chars/token = 300 char cap → mid-line at char 300.
        let page = crate::agentic::tools::output_budget::paginate_read_from(
            path, &long_line, None, None, 100, None,
        );
        // Verify exact fixture coordinates before feeding to the summarizer.
        let (footer_offset, footer_char_offset) = parse_offset_and_char_offset(&page);
        assert_eq!(footer_offset, Some(1), "fixture offset mismatch: {page}");
        assert_eq!(
            footer_char_offset,
            Some(300),
            "fixture char_offset mismatch: {page}"
        );
        let line = summarize_one("read_file", json!({"path": path}), &page);
        assert!(
            line.contains("re-read with offset=1 char_offset=300"),
            "trusted footer must carry the producer's exact resume coordinates: {line}"
        );
    }

    /// #2638 round 5: a RESUMED `char_offset` page — the model's second call,
    /// echoing back the first page's footer coordinates, with a TRUSTED
    /// footer (matching path). The page map keeps its exact resumed
    /// coordinates (offset=1, char_offset=600 for budget=100 at 3 c/t: first
    /// cut at 300, second cut at 300 more) — trust restores round-3's
    /// behavior. The AST outline guard (kept from round 4) still declines
    /// unconditionally on any nonzero incoming `char_offset`, regardless of
    /// trust: the page's first line is a lexical fragment either way.
    #[test]
    fn a_trusted_resumed_char_offset_page_keeps_its_page_map_and_still_declines_the_outline() {
        let long_line = "z".repeat(2_000);
        let path = "notes.md";
        let first = crate::agentic::tools::output_budget::paginate_read_from(
            path, &long_line, None, None, 100, None,
        );
        let (first_offset, first_char_offset) = parse_offset_and_char_offset(&first);
        assert_eq!(first_offset, Some(1), "first page offset mismatch: {first}");
        assert_eq!(
            first_char_offset,
            Some(300),
            "first page char_offset mismatch: {first}"
        );
        let resumed = crate::agentic::tools::output_budget::paginate_read_from(
            path,
            &long_line,
            Some(1),
            None,
            100,
            Some(300),
        );
        // Verify exact resumed-page footer coordinates.
        let (res_offset, res_char_offset) = parse_offset_and_char_offset(&resumed);
        assert_eq!(
            res_offset,
            Some(1),
            "resumed page offset mismatch: {resumed}"
        );
        assert_eq!(
            res_char_offset,
            Some(600),
            "resumed page char_offset mismatch: {resumed}"
        );
        let args = json!({"path": path, "offset": 1, "char_offset": 300_u64});
        let line = summarize_one("read_file", args, &resumed);
        assert!(
            line.contains("re-read with offset=1 char_offset=600"),
            "trusted resumed footer must keep its exact page-map coordinates: {line}"
        );
        assert!(
            !line.contains("— outline"),
            "the incoming char_offset guard must still decline the outline: {line}"
        );
    }

    /// #2638 round 4, finding 2: a FINAL no-footer fragment — the model
    /// resumes with an incoming `char_offset` and this time the remainder
    /// fits under budget, so the real producer returns it with NO footer.
    /// The outline must decline: the fragment's first line is a mid-line
    /// continuation even though nothing in `content` says so — only the
    /// incoming call argument (`char_offset`) does.
    ///
    /// Non-vacuous: the fragment is padded so identical content DOES outline
    /// without `char_offset` (outline < content) and DECLINES with it.
    /// Red check: removing the guard at `incoming_char_offset(args).is_some()`
    /// causes the with-offset call to outline, failing the second assertion.
    #[cfg(feature = "ast")]
    #[test]
    fn incoming_char_offset_guard_non_vacuously_declines_outline() {
        // 100 comment lines (~1100 chars) + 20 functions (~280 chars) = ~1380
        // chars of content; outline of 20 entries is ~250 chars → outline IS
        // shorter, so the length gate at `json_str_len` doesn't discard it.
        let pad: String = (0..100).map(|i| format!("// line {i:03}\n")).collect();
        let fns: String = (0..20).map(|i| format!("fn f{i:02}() {{}}\n")).collect();
        let fragment = format!("{pad}{fns}");
        // Without char_offset: outline fires (proving guard is not vacuous).
        let no_offset = summarize_one("read_file", json!({"path": "src/lib.rs"}), &fragment);
        assert!(
            no_offset.contains("— outline"),
            "fragment must outline without char_offset (guard must do real work): {no_offset}"
        );
        // With char_offset: guard fires, outline declined.
        let with_offset = summarize_one(
            "read_file",
            json!({"path": "src/lib.rs", "char_offset": 1_u64}),
            &fragment,
        );
        assert!(
            !with_offset.contains("— outline"),
            "incoming char_offset must decline the outline for a mid-line fragment: {with_offset}"
        );
    }

    /// #2638 round 5, case 6: the actual target case #2557 exists for — a
    /// REAL ~13k-line file (this workspace's own `agentic/mod.rs`), always
    /// paginated, through the real producer and then the real summarizer.
    /// `include_str!` embeds the file at compile time — no runtime fs read,
    /// keeping this a unit test per this repo's fully-mocked unit tier.
    #[cfg(feature = "ast")]
    #[test]
    fn a_real_13k_line_file_through_paginate_and_prune_gets_an_outline() {
        const MOD_RS: &str = include_str!("agentic/mod.rs");
        let total_lines = MOD_RS.lines().count();
        assert!(
            total_lines > 10_000,
            "fixture file is no longer ~13k lines ({total_lines}) — pick another large file"
        );
        let path = "newt-core/src/agentic/mod.rs";
        let page = crate::agentic::tools::output_budget::paginate_read_from(
            path,
            MOD_RS,
            Some(1),
            None, // default read limit
            0,    // no char cap — the line window alone triggers pagination
            None,
        );
        assert!(
            page.contains("call read_file with offset="),
            "a 13k-line file at the default line window must paginate: got a page of \
             {} chars with no footer",
            page.len()
        );
        let line = summarize_one("read_file", json!({"path": path, "offset": 1}), &page);
        assert!(
            line.contains("— outline") || line.contains("page map"),
            "the target case (#2557): a real paginated read of a real 13k-line file must \
             get an outline or page map, not the plain one-liner: {line}"
        );
    }

    /// Pulls `offset=` and `char_offset=` back out of a real page's footer,
    /// for tests that chain a second real `paginate_read_from` call the way
    /// the model would (round 3: drive the real producer, not a hand-built
    /// footer).
    fn parse_offset_and_char_offset(page: &str) -> (Option<usize>, Option<usize>) {
        let Some(footer) = page.rsplit_once("\n\n[").map(|(_, t)| t) else {
            return (None, None);
        };
        let mut offset = None;
        let mut char_offset = None;
        for tok in footer.split_whitespace() {
            if let Some(v) = tok.strip_prefix("offset=") {
                offset = v.parse().ok();
            } else if let Some(v) = tok.strip_prefix("char_offset=") {
                char_offset = v.trim_end_matches(']').parse().ok();
            }
        }
        (offset, char_offset)
    }

    #[test]
    fn one_liner_write_edit_list_search_fetch_and_generic() {
        let content = text_lines(20);
        let chars = content.chars().count();
        assert_eq!(
            summarize_one(
                "write_file",
                json!({"path": "a.rs", "content": "xx"}),
                &content
            ),
            "[write_file] wrote 'a.rs' -> ok, 20 lines result",
        );
        assert_eq!(
            summarize_one("edit_file", json!({"path": "b.rs"}), &content),
            "[edit_file] edited 'b.rs' -> ok, 20 lines result",
        );
        assert_eq!(
            summarize_one("list_dir", json!({"path": "src"}), &content),
            "[list_dir] listed 'src' -> ok, 20 entries",
        );
        assert_eq!(
            summarize_one("search", json!({"query": "fn main"}), &content),
            "[search] searched 'fn main' -> ok, 20 matching lines",
        );
        assert_eq!(
            summarize_one("search", json!({"pattern": "TODO"}), &content),
            "[search] searched 'TODO' -> ok, 20 matching lines",
        );
        assert_eq!(
            summarize_one(
                "web_fetch",
                json!({"url": "https://example.com/doc"}),
                &content
            ),
            format!(
                "[web_fetch] fetched 'https://example.com/doc' -> ok, 20 lines ({chars} chars)"
            ),
        );
        assert_eq!(
            summarize_one("gitea__issue_view", json!({"index": 1}), &content),
            format!("[gitea__issue_view] result elided -> ok, 20 lines ({chars} chars)"),
        );
    }

    #[test]
    fn one_liner_missing_args_uses_placeholder() {
        let line = summarize_one("read_file", json!(null), &text_lines(20));
        assert!(
            line.starts_with("[read_file] ? lines 1-20 (page map"),
            "{line}"
        );
    }

    #[test]
    fn one_liner_long_command_excerpted_and_newlines_flattened() {
        let cmd = format!("echo {}\nsecond", "x".repeat(100));
        let line = summarize_one("run_command", json!({"command": cmd}), &text_lines(20));
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains('…'), "{line}");
        assert!(
            line.chars().count() < 200,
            "one-liners stay under the default threshold"
        );
    }

    #[test]
    fn openai_results_pair_by_id_even_out_of_order() {
        let read = text_lines(50);
        let listing = text_lines(30);
        let mut msgs = vec![
            user("task"),
            assistant_calls_openai(&[
                ("call_a", "read_file", json!({"path": "x.rs"})),
                ("call_b", "list_dir", json!({"path": "src"})),
            ]),
            // results arrive swapped
            tool_result_id("call_b", &listing),
            tool_result_id("call_a", &read),
        ];
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &PruneConfig::default());
        assert!(
            content_of(&out[2]).starts_with("[list_dir] listed 'src'"),
            "{}",
            content_of(&out[2])
        );
        assert!(
            content_of(&out[3]).starts_with("[read_file] x.rs lines"),
            "{}",
            content_of(&out[3])
        );
    }

    #[test]
    fn ollama_results_pair_positionally() {
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[
                ("read_file", json!({"path": "x.rs"})),
                ("list_dir", json!({"path": "src"})),
            ]),
            tool_result(&text_lines(50)),
            tool_result(&text_lines(30)),
        ];
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &PruneConfig::default());
        assert!(content_of(&out[2]).starts_with("[read_file] x.rs lines"));
        assert!(content_of(&out[3]).starts_with("[list_dir] listed 'src'"));
    }

    #[test]
    fn orphan_tool_result_gets_generic_one_liner() {
        let content = text_lines(25);
        let chars = content.chars().count();
        let mut msgs = vec![user("task"), tool_result(&content)]; // no assistant before it
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &PruneConfig::default());
        assert_eq!(
            content_of(&out[1]),
            format!("[tool] result elided -> ok, 25 lines ({chars} chars)"),
        );
    }

    #[test]
    fn summarize_protects_tail_and_skips_existing_markers() {
        let big = text_lines(40);
        let marker = "[read_file] read 'x' -> ok, 3 lines (5 chars)";
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("read_file", json!({"path": "x"}))]),
            json!({"role": "tool", "content": marker}),
        ];
        pad_tail(&mut msgs, 2);
        msgs.push(tool_result(&big)); // inside tail
        let cfg = PruneConfig {
            keep_last: 1,
            summarize_min_chars: 10,
            ..PruneConfig::default()
        };
        let out = summarize_aged_tool_results(&msgs, &cfg);
        assert_eq!(content_of(&out[2]), marker, "existing marker not rewritten");
        assert_eq!(content_of(out.last().unwrap()), big, "tail untouched");
    }

    // -- pass 3: JSON-aware arg shrinking ------------------------------------

    #[test]
    fn shrink_truncates_inside_object_args() {
        let body = "fn main() {}\n".repeat(200); // 2600 chars
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[(
                "write_file",
                json!({"path": "src/main.rs", "content": body}),
            )]),
            tool_result("ok"),
        ];
        pad_tail(&mut msgs, 10);
        let out = shrink_tool_call_args(&msgs, &PruneConfig::default());
        let args = &out[1]["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_object(), "object args stay objects");
        assert_eq!(args["path"], "src/main.rs", "short values untouched");
        let content = args["content"].as_str().unwrap();
        assert!(content.chars().count() <= 200, "truncated to the cap");
        assert!(content.contains("… [+"), "{content}");
        assert!(content.ends_with("chars]"), "{content}");
    }

    #[test]
    fn shrink_keeps_string_args_as_valid_json_strings() {
        let args =
            json!({"path": "a.rs", "old_string": "x".repeat(900), "new_string": "y".repeat(900)});
        let mut msgs = vec![
            user("task"),
            assistant_calls_openai(&[("call_1", "edit_file", args)]),
            tool_result_id("call_1", "ok"),
        ];
        pad_tail(&mut msgs, 10);
        let before_len = msgs[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .len();
        let out = shrink_tool_call_args(&msgs, &PruneConfig::default());
        let s = out[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("still a string");
        let parsed: Value = serde_json::from_str(s).expect("still valid JSON");
        assert_eq!(parsed["path"], "a.rs");
        assert!(parsed["old_string"].as_str().unwrap().chars().count() <= 200);
        assert!(s.len() < before_len);
    }

    #[test]
    fn shrink_recurses_into_arrays_and_nested_objects() {
        let args = json!({"items": ["z".repeat(700), {"inner": "w".repeat(700)}], "n": 7});
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("custom", args)]),
            tool_result("ok"),
        ];
        pad_tail(&mut msgs, 10);
        let out = shrink_tool_call_args(&msgs, &PruneConfig::default());
        let a = &out[1]["tool_calls"][0]["function"]["arguments"];
        assert!(a["items"][0].as_str().unwrap().chars().count() <= 200);
        assert!(a["items"][1]["inner"].as_str().unwrap().chars().count() <= 200);
        assert_eq!(a["n"], 7);
    }

    #[test]
    fn shrink_replaces_unparseable_oversized_string_args() {
        let bad = format!("{{not json {}", "x".repeat(800));
        let mut msgs = vec![
            user("task"),
            json!({"role": "assistant", "content": "",
                   "tool_calls": [{"id": "c1", "type": "function",
                                   "function": {"name": "custom", "arguments": bad}}]}),
            tool_result_id("c1", "ok"),
        ];
        pad_tail(&mut msgs, 10);
        let out = shrink_tool_call_args(&msgs, &PruneConfig::default());
        let s = out[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let parsed: Value = serde_json::from_str(s).expect("placeholder is valid JSON");
        assert!(parsed["truncated"]
            .as_str()
            .unwrap()
            .contains("not valid JSON"));
    }

    #[test]
    fn shrink_leaves_small_args_and_tail_untouched() {
        let small = json!({"path": "a.rs"});
        let big = json!({"content": "q".repeat(2000)});
        let mut msgs = vec![user("task"), assistant_calls(&[("read_file", small)])];
        pad_tail(&mut msgs, 3);
        msgs.push(assistant_calls(&[("write_file", big)])); // inside tail
        let cfg = PruneConfig {
            keep_last: 2,
            ..PruneConfig::default()
        };
        let out = shrink_tool_call_args(&msgs, &cfg);
        assert_eq!(out, msgs);
    }

    #[test]
    fn shrink_cap_floor_prevents_marker_oscillation() {
        let cfg = PruneConfig {
            arg_string_cap: 0,
            ..PruneConfig::default()
        };
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("write_file", json!({"content": "r".repeat(3000)}))]),
            tool_result("ok"),
        ];
        pad_tail(&mut msgs, 10);
        let once = shrink_tool_call_args(&msgs, &cfg);
        let s = once[1]["tool_calls"][0]["function"]["arguments"]["content"]
            .as_str()
            .unwrap();
        assert!(
            s.chars().count() <= MIN_ARG_STRING_CAP,
            "floored cap respected: {s}"
        );
        let twice = shrink_tool_call_args(&once, &cfg);
        assert_eq!(twice, once);
    }

    #[test]
    fn truncate_chars_is_exact_and_stable() {
        assert_eq!(truncate_chars("short", 32), None);
        let t = truncate_chars(&"é".repeat(100), 32).unwrap(); // multibyte-safe
        assert!(t.chars().count() <= 32);
        assert_eq!(
            truncate_chars(&t, 32),
            None,
            "second application is a no-op"
        );
    }

    // -- top-level prune ------------------------------------------------------

    #[test]
    fn prune_empty_and_all_tail_are_noops() {
        let cfg = PruneConfig::default();
        let out = prune(&[], &cfg);
        assert!(out.messages.is_empty());
        assert_eq!(out.chars_reclaimed, 0);

        let msgs = vec![user("task"), tool_result(&text_lines(100))];
        let out = prune(&msgs, &cfg); // len <= keep_last → fully protected
        assert_eq!(out.messages, msgs);
        assert_eq!(out.chars_reclaimed, 0);
    }

    /// The realistic synthetic transcript: a coding session with a repeated
    /// failing `cargo test`, large file reads, and bulky write/edit args.
    /// Asserts every pass contributes, and prints the per-pass numbers.
    #[test]
    fn realistic_transcript_pass_by_pass_breakdown() {
        let cargo_fail = format!("error: test failed\n{}", text_lines(300));
        let lib_rs = text_lines(600);
        let cargo_pass = text_lines(60);
        let new_body = "fn lossy_op() { /* generated */ }\n".repeat(120);
        let msgs = vec![
            json!({"role": "system", "content": "You are newt, a coding agent."}),
            user("fix the failing test in newt-core"),
            assistant_calls(&[("read_file", json!({"path": "newt-core/src/lib.rs"}))]),
            tool_result(&lib_rs),
            assistant_calls(&[("run_command", json!({"command": "cargo test -p newt-core"}))]),
            tool_result(&cargo_fail),
            assistant_calls(&[(
                "edit_file",
                json!({
                    "path": "newt-core/src/lib.rs",
                    "old_string": text_lines(60),
                    "new_string": text_lines(62),
                }),
            )]),
            tool_result("edited newt-core/src/lib.rs (+2 lines)"),
            assistant_calls(&[("run_command", json!({"command": "cargo test -p newt-core"}))]),
            tool_result(&cargo_fail), // identical failure → dedupe fodder
            assistant_calls(&[(
                "write_file",
                json!({"path": "newt-core/src/fix.rs", "content": new_body}),
            )]),
            tool_result("wrote newt-core/src/fix.rs"),
            assistant_calls(&[("run_command", json!({"command": "cargo test -p newt-core"}))]),
            tool_result(&cargo_pass),
            assistant_text("All tests pass now. The bug was an off-by-one."),
            user("great — open the PR"),
        ];

        let cfg = PruneConfig {
            keep_last: 4,
            ..PruneConfig::default()
        };
        let s0 = serialized_len(&msgs);
        let p1 = collapse_duplicate_tool_results(&msgs, &cfg);
        let s1 = serialized_len(&p1);
        let p2 = summarize_aged_tool_results(&p1, &cfg);
        let s2 = serialized_len(&p2);
        let p3 = shrink_tool_call_args(&p2, &cfg);
        let s3 = serialized_len(&p3);
        println!("baseline: {s0} chars");
        println!("pass 1 (dedupe):      -{} chars -> {s1}", s0 - s1);
        println!("pass 2 (one-liners):  -{} chars -> {s2}", s1 - s2);
        println!("pass 3 (arg shrink):  -{} chars -> {s3}", s2 - s3);
        assert!(s1 < s0, "dedupe reclaims");
        assert!(s2 < s1, "one-liners reclaim");
        assert!(s3 < s2, "arg shrink reclaims");

        let outcome = prune(&msgs, &cfg);
        assert_eq!(outcome.messages, p3, "prune == the three passes in order");
        assert_eq!(
            outcome.chars_reclaimed,
            s0 - s3,
            "accounting matches the pass chain"
        );
        println!(
            "total: {} of {s0} chars reclaimed ({:.0}%)",
            outcome.chars_reclaimed,
            100.0 * outcome.chars_reclaimed as f64 / s0 as f64
        );
        // The last user message + final answer are inside keep_last=4.
        assert_eq!(outcome.messages[15], msgs[15]);
        assert_eq!(outcome.messages[14], msgs[14]);
    }

    // -- property tests (deterministic seeded loops) --------------------------

    /// xorshift64* — deterministic, no dev-dep.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Deterministic synthetic transcript: mixed dialects, duplicate-prone
    /// result payloads, oversized and small args, interleaved chatter.
    fn synth_transcript(seed: u64) -> Vec<Value> {
        let mut rng = Rng(seed.max(1));
        let tools = [
            "run_command",
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "search",
            "web_fetch",
            "use_skill",
        ];
        // Small payload pool → duplicates across rounds are likely.
        let pool: Vec<String> = (0..5).map(|k| text_lines(10 + k * 37)).collect();
        let mut msgs = vec![
            json!({"role": "system", "content": "You are newt."}),
            user("do the task"),
        ];
        for round in 0..(4 + rng.below(5)) {
            if rng.below(4) == 0 {
                msgs.push(assistant_text("thinking out loud"));
                msgs.push(user(&format!("continue ({round})")));
            }
            let openai = rng.below(2) == 0;
            let ncalls = 1 + rng.below(3);
            let calls: Vec<(String, String, Value)> = (0..ncalls)
                .map(|j| {
                    let name = tools[rng.below(tools.len())];
                    let key = match name {
                        "run_command" => "command",
                        "search" => "query",
                        "web_fetch" => "url",
                        _ => "path",
                    };
                    let mut args = json!({key: format!("target-{}-{}", round, j)});
                    if rng.below(3) == 0 {
                        args["content"] = Value::String("b".repeat(50 + rng.below(2000)));
                    }
                    (format!("call_{round}_{j}"), name.to_string(), args)
                })
                .collect();
            if openai {
                let refs: Vec<(&str, &str, Value)> = calls
                    .iter()
                    .map(|(id, n, a)| (id.as_str(), n.as_str(), a.clone()))
                    .collect();
                msgs.push(assistant_calls_openai(&refs));
                // Sometimes deliver results out of order (ids still pair).
                let mut order: Vec<usize> = (0..ncalls).collect();
                if ncalls > 1 && rng.below(2) == 0 {
                    order.swap(0, 1);
                }
                for &j in &order {
                    let content = if rng.below(2) == 0 {
                        pool[rng.below(pool.len())].clone()
                    } else {
                        text_lines(1 + rng.below(80))
                    };
                    msgs.push(tool_result_id(&calls[j].0, &content));
                }
            } else {
                let refs: Vec<(&str, Value)> = calls
                    .iter()
                    .map(|(_, n, a)| (n.as_str(), a.clone()))
                    .collect();
                msgs.push(assistant_calls(&refs));
                for _ in 0..ncalls {
                    let content = if rng.below(2) == 0 {
                        pool[rng.below(pool.len())].clone()
                    } else {
                        text_lines(1 + rng.below(80))
                    };
                    msgs.push(tool_result(&content));
                }
            }
        }
        msgs.push(assistant_text("done"));
        msgs.push(user("thanks"));
        msgs
    }

    fn property_configs() -> Vec<PruneConfig> {
        vec![
            PruneConfig::default(),
            PruneConfig {
                keep_last: 4,
                ..PruneConfig::default()
            },
            PruneConfig {
                keep_last: 0,
                dedupe_min_chars: 50,
                summarize_min_chars: 50,
                args_max_chars: 100,
                arg_string_cap: 0,
            },
        ]
    }

    #[test]
    fn property_output_tool_args_always_parse_and_keep_their_shape() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let out = prune(&msgs, &cfg).messages;
                for (i, msg) in out.iter().enumerate() {
                    let Some(tcs) = msg["tool_calls"].as_array() else {
                        continue;
                    };
                    for (j, tc) in tcs.iter().enumerate() {
                        let before = &msgs[i]["tool_calls"][j]["function"]["arguments"];
                        let after = &tc["function"]["arguments"];
                        match after {
                            Value::String(s) => {
                                assert!(
                                    before.is_string(),
                                    "seed {seed}: string args stay strings"
                                );
                                serde_json::from_str::<Value>(s).unwrap_or_else(|e| {
                                    panic!("seed {seed} msg {i} call {j}: invalid JSON ({e}): {s}")
                                });
                            }
                            v => assert!(
                                !before.is_string() && (v.is_object() || v.is_null()),
                                "seed {seed}: structured args stay structured"
                            ),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn property_tool_pairing_structure_is_preserved() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let out = prune(&msgs, &cfg).messages;
                assert_eq!(
                    out.len(),
                    msgs.len(),
                    "seed {seed}: no messages added or removed"
                );
                for (a, b) in msgs.iter().zip(&out) {
                    assert_eq!(a["role"], b["role"], "seed {seed}: roles preserved");
                    assert_eq!(
                        a["tool_call_id"], b["tool_call_id"],
                        "seed {seed}: result ids preserved"
                    );
                    let (atc, btc) = (a["tool_calls"].as_array(), b["tool_calls"].as_array());
                    assert_eq!(
                        atc.map(Vec::len),
                        btc.map(Vec::len),
                        "seed {seed}: tool_call counts preserved"
                    );
                    if let (Some(atc), Some(btc)) = (atc, btc) {
                        for (x, y) in atc.iter().zip(btc) {
                            assert_eq!(x["id"], y["id"], "seed {seed}: call ids preserved");
                            assert_eq!(
                                x["function"]["name"], y["function"]["name"],
                                "seed {seed}: call names preserved"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn property_prune_is_idempotent() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let once = prune(&msgs, &cfg);
                let twice = prune(&once.messages, &cfg);
                assert_eq!(
                    twice.messages, once.messages,
                    "seed {seed}: prune(prune(x)) == prune(x)"
                );
                assert_eq!(
                    twice.chars_reclaimed, 0,
                    "seed {seed}: second pass reclaims nothing"
                );
            }
        }
    }

    #[test]
    fn property_protected_tail_is_byte_identical() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let out = prune(&msgs, &cfg).messages;
                let tail_start = msgs.len().saturating_sub(cfg.keep_last);
                for i in tail_start..msgs.len() {
                    assert_eq!(
                        serde_json::to_string(&msgs[i]).unwrap(),
                        serde_json::to_string(&out[i]).unwrap(),
                        "seed {seed}: tail message {i} must be byte-identical"
                    );
                }
            }
        }
    }

    #[test]
    fn property_chars_reclaimed_accounting_is_exact() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let outcome = prune(&msgs, &cfg);
                let before = serialized_len(&msgs);
                let after = serialized_len(&outcome.messages);
                assert!(
                    after <= before,
                    "seed {seed}: prune never grows the transcript"
                );
                assert_eq!(
                    outcome.chars_reclaimed,
                    before - after,
                    "seed {seed}: exact accounting"
                );
            }
        }
    }

    /// **The anti-drift twin (#1992).** Every arm the builder can emit must be
    /// recognized. If someone rewords an arm and forgets the recognizer, the
    /// digest fold silently stops folding that tool — a regression that shows
    /// up as "the floor stopped falling" months later, not as a failure.
    #[test]
    fn every_one_liner_the_builder_emits_is_recognized() {
        let args = serde_json::json!({
            "command": "npm test", "path": "src/x.rs", "query": "needle",
            "pattern": "needle", "url": "https://example.invalid/x"
        });
        // Every named arm, plus an unnamed one for the `_ =>` default.
        for name in [
            "run_command",
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "search",
            "web_fetch",
            "some_mcp__tool",
        ] {
            for body in ["a\nb\nc", "error: it broke"] {
                let line = one_line_summary(name, Some(&args), body);
                assert!(
                    is_one_line_summary(&line),
                    "builder emitted a summary its own recognizer rejects: {line:?}"
                );
            }
        }
        // #2638: a body large enough to actually produce the page-map form
        // (the tiny fixtures above always fall back to the old
        // `-> ok,`/`-> error,` one-liner and never exercise this arm).
        let big_read = text_lines(900);
        let page_map = one_line_summary("read_file", Some(&args), &big_read);
        assert!(
            page_map.contains("(page map; re-read any span with offset/limit): "),
            "fixture didn't actually produce a page map: {page_map:?}"
        );
        assert!(
            is_one_line_summary(&page_map),
            "builder emitted a page map its own recognizer rejects: {page_map:?}"
        );
    }

    /// The twin that stops "recognizes everything". A false positive here folds
    /// a VERBATIM result away, which is unrecoverable in the wrong direction.
    #[test]
    fn raw_tool_output_is_not_mistaken_for_a_summary() {
        for raw in [
            "total 12\ndrwxr-xr-x  3 u u 4096 Jan  1 00:00 .",
            "[INFO] building project\n[INFO] done",
            "[warn] something -> ok, but over\nmultiple lines",
            "error: command exited 101\nerror[E0308]: mismatched types",
            "{\"ok\": true}",
            "",
            "[not a summary] no arrow clause here",
        ] {
            assert!(
                !is_one_line_summary(raw),
                "raw tool output was mistaken for a one-liner: {raw:?}"
            );
        }
    }
}

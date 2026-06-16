//! Separating a model's **reasoning** from its **content**.
//!
//! Thinking models (Nemotron with `detailed thinking on`, DeepSeek-R1, Qwen3, …)
//! emit their chain-of-thought *inline* in the content stream, wrapped in
//! `<think>…</think>` tags — distinct from the Ollama/OpenAI *separate* reasoning
//! field the loop already detects. Left in place, that reasoning leaks into the
//! reply shown to the user and into the content the agentic loop parses (issue
//! #385). This module strips it.
//!
//! It's a **split**, not just a strip: [`split_reasoning`] returns
//! `(content, reasoning)` so the reasoning can be *captured* (the `plan_mode`
//! technique, `docs/design/thinking-effort-and-plan-mode.md`) rather than thrown
//! away — today every caller discards the reasoning half, matching prior behavior.
//!
//! Two surfaces:
//! - [`split_reasoning`] — batch, for a fully-assembled reply (the non-streaming
//!   completion paths).
//! - [`ThinkFilter`] — incremental, for the streaming path, where a `<think>` block
//!   spans token boundaries (`<thi` / `nk>…</thi` / `nk>`) and must be suppressed
//!   live without ever printing a partial tag.

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// Split inline `<think>…</think>` reasoning out of `content`, returning
/// `(clean_content, reasoning)`.
///
/// Handles the shapes seen in the wild:
/// - paired `<think>R</think>A` (one or many, anywhere) → reasoning removed;
/// - a **lone leading** `R</think>A` (some templates start mid-thought, emitting
///   only the closer) → everything up to the first `</think>` is reasoning;
/// - an **unterminated** `A<think>R` (a truncated thinking block) → everything from
///   `<think>` on is reasoning.
///
/// **No-op fast path:** content with no `<think>`/`</think>` is returned unchanged
/// (not even trimmed), so ordinary replies are bit-for-bit untouched.
#[must_use]
pub fn split_reasoning(content: &str) -> (String, Option<String>) {
    // Lone leading closer (no opener) — only meaningful when there is no `<think>`.
    if !content.contains(OPEN) {
        return match content.find(CLOSE) {
            Some(close) => {
                let reasoning = content[..close].trim();
                let clean = content[close + CLOSE.len()..].trim();
                (clean.to_string(), non_empty(reasoning))
            }
            None => (content.to_string(), None), // nothing to strip — untouched
        };
    }

    let mut clean = String::new();
    let mut reasoning = String::new();
    let mut rest = content;
    while let Some(open) = rest.find(OPEN) {
        clean.push_str(&rest[..open]);
        let after = &rest[open + OPEN.len()..];
        match after.find(CLOSE) {
            Some(close) => {
                reasoning.push_str(&after[..close]);
                rest = &after[close + CLOSE.len()..];
            }
            None => {
                // Unterminated <think>: the remainder is all reasoning.
                reasoning.push_str(after);
                rest = "";
                break;
            }
        }
    }
    clean.push_str(rest);
    (clean.trim().to_string(), non_empty(reasoning.trim()))
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// An incremental filter that suppresses `<think>…</think>` spans from a *streamed*
/// content sequence, emitting only the clean (non-reasoning) text — even when a tag
/// is split across feeds.
///
/// Feed each streamed token through [`feed`](Self::feed) and print/accumulate what
/// it returns; call [`finish`](Self::finish) once at the end. A trailing partial tag
/// (`…<thi`) is held back rather than emitted, so a tag is never printed in pieces.
#[derive(Debug, Default)]
pub struct ThinkFilter {
    inside: bool,
    buf: String,
}

impl ThinkFilter {
    /// A fresh filter (outside any think block).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one streamed token; returns the clean text to emit now (may be empty).
    pub fn feed(&mut self, token: &str) -> String {
        self.feed_split(token).0
    }

    /// Feed one streamed token; returns `(clean, reasoning)` for this feed — the
    /// clean text to emit now AND the reasoning text suppressed from it (either
    /// may be empty). Lets a caller *render* the live reasoning (the cargo-style
    /// thinking spinner) instead of discarding it, while `feed` keeps the
    /// suppress-only behavior for everyone else.
    pub fn feed_split(&mut self, token: &str) -> (String, String) {
        self.buf.push_str(token);
        let mut clean = String::new();
        let mut reasoning = String::new();
        loop {
            if self.inside {
                match self.buf.find(CLOSE) {
                    Some(i) => {
                        reasoning.push_str(&self.buf[..i]);
                        self.buf.drain(..i + CLOSE.len());
                        self.inside = false; // continue: there may be clean text after
                    }
                    None => {
                        // Suppress reasoning, but keep a trailing partial `</think>`.
                        let cut = safe_len(&self.buf, CLOSE);
                        reasoning.push_str(&self.buf[..cut]);
                        self.buf.drain(..cut);
                        break;
                    }
                }
            } else {
                match self.buf.find(OPEN) {
                    Some(i) => {
                        clean.push_str(&self.buf[..i]);
                        self.buf.drain(..i + OPEN.len());
                        self.inside = true;
                    }
                    None => {
                        // Emit all but a trailing partial `<think>`.
                        let cut = safe_len(&self.buf, OPEN);
                        clean.push_str(&self.buf[..cut]);
                        self.buf.drain(..cut);
                        break;
                    }
                }
            }
        }
        (clean, reasoning)
    }

    /// Flush at end of stream: emits any buffered clean tail (an unterminated
    /// `<think>` leaves its reasoning suppressed).
    pub fn finish(&mut self) -> String {
        let out = if self.inside {
            String::new()
        } else {
            std::mem::take(&mut self.buf)
        };
        self.buf.clear();
        self.inside = false;
        out
    }
}

/// The length of `buf` that is safe to commit (emit *or* discard) without splitting
/// a possible `tag` — i.e. `buf` minus its longest suffix that is a proper prefix of
/// `tag`. (`"a<thi"`, tag `"<think>"` → `1`, holding back `"<thi"`.)
fn safe_len(buf: &str, tag: &str) -> usize {
    let max = buf.len().min(tag.len() - 1);
    for k in (1..=max).rev() {
        let start = buf.len() - k;
        if buf.is_char_boundary(start) && tag.starts_with(&buf[start..]) {
            return start;
        }
    }
    buf.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tags_is_untouched() {
        let (c, r) = split_reasoning("just a normal reply");
        assert_eq!(c, "just a normal reply");
        assert!(r.is_none());
    }

    #[test]
    fn strips_a_leading_paired_block() {
        let (c, r) = split_reasoning("<think>let me think</think>\n\nThe answer is 42.");
        assert_eq!(c, "The answer is 42.");
        assert_eq!(r.as_deref(), Some("let me think"));
    }

    #[test]
    fn strips_multiple_and_embedded_blocks() {
        let (c, r) = split_reasoning("A<think>r1</think>B<think>r2</think>C");
        assert_eq!(c, "ABC");
        assert_eq!(r.as_deref(), Some("r1r2"));
    }

    #[test]
    fn lone_leading_closer_is_reasoning() {
        let (c, r) = split_reasoning("reasoning with no opener</think>the answer");
        assert_eq!(c, "the answer");
        assert_eq!(r.as_deref(), Some("reasoning with no opener"));
    }

    #[test]
    fn unterminated_open_swallows_the_tail() {
        let (c, r) = split_reasoning("partial answer<think>cut off mid-thought");
        assert_eq!(c, "partial answer");
        assert_eq!(r.as_deref(), Some("cut off mid-thought"));
    }

    #[test]
    fn all_reasoning_yields_empty_content() {
        let (c, r) = split_reasoning("<think>only thinking, no answer</think>");
        assert_eq!(c, "");
        assert_eq!(r.as_deref(), Some("only thinking, no answer"));
    }

    /// Drive the streaming filter with an arbitrary token split and assert it equals
    /// the batch result's clean half.
    fn stream(content: &str, tokens: &[&str]) -> String {
        let mut f = ThinkFilter::new();
        let mut out = String::new();
        for t in tokens {
            out.push_str(&f.feed(t));
        }
        out.push_str(&f.finish());
        let _ = content;
        out
    }

    #[test]
    fn streaming_suppresses_a_block_split_across_tokens() {
        // The <think> tags are deliberately shredded across token boundaries.
        let tokens = [
            "Here",
            " <thi",
            "nk>my ",
            "reason",
            "ing</thi",
            "nk> is the ",
            "answer",
        ];
        assert_eq!(stream("", &tokens), "Here  is the answer");
    }

    #[test]
    fn streaming_char_by_char_matches_batch() {
        let content = "<think>step one\nstep two</think>Final line.";
        let toks: Vec<String> = content.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = toks.iter().map(String::as_str).collect();
        assert_eq!(stream(content, &refs), "Final line.");
    }

    #[test]
    fn streaming_plain_text_is_verbatim() {
        assert_eq!(stream("", &["no ", "tags ", "here"]), "no tags here");
    }

    #[test]
    fn feed_split_captures_reasoning_across_token_boundaries() {
        let tokens = [
            "Here",
            " <thi",
            "nk>my ",
            "reason",
            "ing</thi",
            "nk> is the ",
            "answer",
        ];
        let mut f = ThinkFilter::new();
        let (mut clean, mut reasoning) = (String::new(), String::new());
        for t in tokens {
            let (c, r) = f.feed_split(t);
            clean.push_str(&c);
            reasoning.push_str(&r);
        }
        clean.push_str(&f.finish());
        assert_eq!(clean, "Here  is the answer");
        assert_eq!(reasoning, "my reasoning");
        // `feed` still suppresses (delegates to feed_split's clean half).
        let mut g = ThinkFilter::new();
        assert_eq!(g.feed("a<think>b"), "a");
        assert_eq!(g.feed("c</think>d"), "d");
    }

    #[test]
    fn streaming_unterminated_block_is_dropped() {
        assert_eq!(
            stream("", &["answer ", "<think>still ", "thinking"]),
            "answer "
        );
    }
}

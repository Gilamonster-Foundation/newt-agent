//! The OpenAI-compatible chat-completions SSE wire.
//!
//! The parsing half of #123, kept separate from the loop for the same reason
//! [`super::anthropic_wire`] is: a wire is a pure function over bytes, and the
//! cheapest honest test for one is a unit test with no HTTP in it.
//!
//! # Why this is not the Anthropic accumulator with different names
//!
//! The two protocols agree on FRAMING and on nothing else. Both are
//! `text/event-stream`, both put one JSON payload per `data:` line, and both
//! can split a line across two `reqwest` chunks — so both need the same
//! rolling line buffer, and that part is deliberately identical.
//!
//! Everything above the frame differs:
//!
//! | | Anthropic | OpenAI-compatible |
//! |---|---|---|
//! | terminator | a `message_stop` EVENT | the literal `data: [DONE]`, which is not JSON |
//! | text | `delta.text` under `content_block_delta` | `choices[0].delta.content` |
//! | reasoning | `delta.thinking` | `delta.reasoning_content`, or `delta.reasoning` |
//! | usage | split across `message_start` and `message_delta` | one object on a chunk whose `choices` is EMPTY |
//!
//! `[DONE]` is the one that matters most: it is a sentinel, not a document,
//! and a parser that hands every `data:` payload to `serde_json` drops the end
//! of the stream on the floor. Nothing else in this repository has ever parsed
//! it — the Anthropic wire terminates on an event, and `newt-mcp-client`'s
//! parser is whole-body rather than incremental — so this is new code rather
//! than a copy wearing a new name.

/// One display-affecting thing a chunk produced.
///
/// Two arms and no more, because those are the two channels a turn shows: the
/// answer, and the thinking above it. A `finish_reason`, a role-only opening
/// delta and an empty string change nothing on screen and so produce nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamAction {
    /// A piece of the answer.
    TextDelta(String),
    /// A piece of the model's reasoning, for the trickle above the answer.
    ReasoningDelta(String),
}

/// What a completed streaming round amounts to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OpenAiStreamRound {
    /// The whole answer, concatenated — the same text the non-streaming wire
    /// would have returned in `message.content`.
    pub text: String,
    /// The whole reasoning body, concatenated. Empty when the model sent none.
    pub reasoning: String,
    /// Token usage, when the server sent it. Requires
    /// `stream_options.include_usage` on the request; a server that ignores
    /// that option leaves this `None` rather than inventing a number.
    pub usage: Option<crate::TokenUsage>,
    /// True once `data: [DONE]` arrived — the stream ENDED rather than being
    /// cut. A caller that streamed a partial answer and never saw this knows
    /// the difference between "the model finished" and "the socket did".
    pub done: bool,
}

/// Incremental parser over an OpenAI-compatible `text/event-stream` body.
///
/// Feed it raw chunks in arrival order; it returns the display actions each
/// chunk completed. It never blocks, never allocates per byte, and holds only
/// the bytes of one unterminated line.
#[derive(Debug, Default)]
pub struct SseAccumulator {
    /// The tail of a `data:` line that arrived without its newline. An SSE
    /// line routinely straddles a chunk boundary; a per-chunk `lines()` split
    /// silently drops the halves.
    line_buf: String,
    round: OpenAiStreamRound,
}

impl SseAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True once `data: [DONE]` has been seen.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.round.done
    }

    /// Feed one raw HTTP chunk; returns the display actions it completed.
    pub fn feed(&mut self, chunk: &str) -> Vec<StreamAction> {
        let mut actions = Vec::new();
        self.line_buf.push_str(chunk);
        while let Some(pos) = self.line_buf.find('\n') {
            let line: String = self.line_buf.drain(..=pos).collect();
            self.apply_line(line.trim_end_matches(['\n', '\r']), &mut actions);
        }
        actions
    }

    /// Flush an unterminated final line and take the round.
    ///
    /// A server that ends the body without a trailing newline still gets its
    /// last event applied, which is how a short reply can otherwise vanish.
    #[must_use]
    pub fn finish(mut self) -> OpenAiStreamRound {
        if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            let mut actions = Vec::new();
            self.apply_line(line.trim_end_matches(['\n', '\r']), &mut actions);
        }
        self.round
    }

    fn apply_line(&mut self, line: &str, actions: &mut Vec<StreamAction>) {
        // Blank lines separate events and `event:` lines are redundant here —
        // the payload carries everything. Anything that is not `data:` is not
        // ours to interpret.
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();

        // The sentinel, checked BEFORE parsing. `[DONE]` is not JSON, so a
        // parser that reaches for serde first throws away the one token that
        // says the stream ended on purpose.
        if data == "[DONE]" {
            self.round.done = true;
            return;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
            return;
        };

        // Usage rides its own chunk when `stream_options.include_usage` is set:
        // `choices` is empty and `usage` is populated. Read it before touching
        // choices so an empty-choices chunk is not mistaken for a dead event.
        if let Some(usage) = super::trim::openai_usage(&json["usage"]) {
            self.round.usage = Some(usage);
        }

        let delta = &json["choices"][0]["delta"];

        // Reasoning first: it precedes the answer, and two spellings are in the
        // wild — `reasoning_content` (DeepSeek, vLLM, llama.cpp) and
        // `reasoning` (several gateways). Neither is in the OpenAI spec, so
        // both are accepted and neither is required.
        for key in ["reasoning_content", "reasoning"] {
            if let Some(text) = delta[key].as_str() {
                if !text.is_empty() {
                    self.round.reasoning.push_str(text);
                    actions.push(StreamAction::ReasoningDelta(text.to_string()));
                }
            }
        }

        if let Some(text) = delta["content"].as_str() {
            // The opening delta is `{"role":"assistant"}` with no content, and
            // keep-alive chunks carry `content: ""`. Both are silence.
            if !text.is_empty() {
                self.round.text.push_str(text);
                actions.push(StreamAction::TextDelta(text.to_string()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame payloads the way a server does: one `data:` line per event,
    /// blank-line separated.
    fn body(frames: &[&str]) -> String {
        frames
            .iter()
            .map(|f| format!("data: {f}\n\n"))
            .collect::<String>()
    }

    fn feed_all(acc: &mut SseAccumulator, s: &str) -> Vec<StreamAction> {
        acc.feed(s)
    }

    #[test]
    fn deltas_accumulate_into_one_answer() {
        let mut acc = SseAccumulator::new();
        let actions = feed_all(
            &mut acc,
            &body(&[
                r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
                r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
                r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
                "[DONE]",
            ]),
        );
        assert_eq!(
            actions,
            vec![
                StreamAction::TextDelta("Hel".into()),
                StreamAction::TextDelta("lo".into())
            ],
            "the role-only opening delta must produce nothing"
        );
        assert!(acc.is_done());
        let round = acc.finish();
        assert_eq!(round.text, "Hello");
        assert!(round.done);
    }

    /// The reason this parser holds a buffer at all. `reqwest` splits where the
    /// socket did, not where the protocol did.
    #[test]
    fn an_event_split_across_two_chunks_is_not_lost() {
        let whole = body(&[r#"{"choices":[{"delta":{"content":"split me"}}]}"#]);
        let cut = whole.len() / 2;

        let mut acc = SseAccumulator::new();
        let first = acc.feed(&whole[..cut]);
        assert!(first.is_empty(), "half an event is not an event: {first:?}");
        let second = acc.feed(&whole[cut..]);
        assert_eq!(second, vec![StreamAction::TextDelta("split me".into())]);
        assert_eq!(acc.finish().text, "split me");
    }

    /// Byte-at-a-time is the adversarial form of the same property.
    #[test]
    fn one_byte_at_a_time_produces_the_same_answer() {
        let whole = body(&[
            r#"{"choices":[{"delta":{"content":"a"}}]}"#,
            r#"{"choices":[{"delta":{"content":"b"}}]}"#,
            "[DONE]",
        ]);
        let mut acc = SseAccumulator::new();
        let mut actions = Vec::new();
        for ch in whole.chars() {
            actions.extend(acc.feed(&ch.to_string()));
        }
        assert_eq!(
            actions,
            vec![
                StreamAction::TextDelta("a".into()),
                StreamAction::TextDelta("b".into())
            ]
        );
        assert!(acc.is_done());
    }

    /// `[DONE]` is a sentinel, not a document. A parser that hands it to serde
    /// first never learns the stream ended.
    #[test]
    fn the_done_sentinel_is_recognized_and_is_not_json() {
        assert!(
            serde_json::from_str::<serde_json::Value>("[DONE]").is_err(),
            "if this ever parses, the check order in apply_line stops mattering"
        );
        let mut acc = SseAccumulator::new();
        acc.feed("data: [DONE]\n\n");
        assert!(acc.is_done());
    }

    /// The usage chunk carries an EMPTY `choices` array. Reading choices first
    /// and bailing would drop the only token counts the wire ever sends.
    #[test]
    fn usage_arrives_on_a_chunk_with_no_choices() {
        let mut acc = SseAccumulator::new();
        acc.feed(&body(&[
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":22}}"#,
            "[DONE]",
        ]));
        let round = acc.finish();
        assert_eq!(round.text, "hi");
        let usage = round.usage.expect("usage chunk was sent");
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 22);
    }

    /// A server that ignores `stream_options` leaves usage absent. Absent is a
    /// fact; zero would be a fabrication.
    #[test]
    fn no_usage_chunk_leaves_usage_none() {
        let mut acc = SseAccumulator::new();
        acc.feed(&body(&[
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            "[DONE]",
        ]));
        assert!(acc.finish().usage.is_none());
    }

    #[test]
    fn both_reasoning_spellings_reach_the_trickle() {
        for key in ["reasoning_content", "reasoning"] {
            let mut acc = SseAccumulator::new();
            let actions = acc.feed(&body(&[&format!(
                r#"{{"choices":[{{"delta":{{"{key}":"because"}}}}]}}"#
            )]));
            assert_eq!(
                actions,
                vec![StreamAction::ReasoningDelta("because".into())],
                "spelling {key} was not recognized"
            );
            assert_eq!(acc.finish().reasoning, "because");
        }
    }

    /// Reasoning and answer are different channels and must not merge — the
    /// whole point of the trickle is that thinking renders above the answer.
    #[test]
    fn reasoning_never_lands_in_the_answer() {
        let mut acc = SseAccumulator::new();
        acc.feed(&body(&[
            r#"{"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#,
            r#"{"choices":[{"delta":{"content":"answer"}}]}"#,
            "[DONE]",
        ]));
        let round = acc.finish();
        assert_eq!(round.text, "answer");
        assert_eq!(round.reasoning, "thinking");
    }

    /// A body that ends without its final newline still gets its last event.
    #[test]
    fn an_unterminated_final_line_is_flushed_by_finish() {
        let mut acc = SseAccumulator::new();
        let actions = acc.feed(r#"data: {"choices":[{"delta":{"content":"tail"}}]}"#);
        assert!(actions.is_empty(), "no newline yet, so no completed line");
        assert_eq!(acc.finish().text, "tail");
    }

    /// Garbage does not panic and does not become the answer.
    #[test]
    fn unparseable_and_foreign_lines_are_ignored() {
        let mut acc = SseAccumulator::new();
        let actions = acc.feed(concat!(
            "event: message\n",
            ": a comment\n",
            "data: {not json\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        ));
        assert_eq!(actions, vec![StreamAction::TextDelta("ok".into())]);
        assert_eq!(acc.finish().text, "ok");
    }

    /// A cut stream is distinguishable from a finished one. The caller needs
    /// that to decide whether to fall back to the probe content.
    #[test]
    fn a_cut_stream_is_not_done() {
        let mut acc = SseAccumulator::new();
        acc.feed(&body(&[r#"{"choices":[{"delta":{"content":"partial"}}]}"#]));
        assert!(!acc.is_done());
        let round = acc.finish();
        assert_eq!(round.text, "partial");
        assert!(!round.done, "no [DONE] arrived, so the stream was cut");
    }
}

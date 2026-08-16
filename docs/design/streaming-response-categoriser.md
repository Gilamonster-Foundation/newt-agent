# Feature Proposal: Streaming Response Categoriser

> **Status (2026-08-16): Draft — partly superseded by existing code.** Incremental reasoning
> categorising already exists: `ThinkFilter` in `newt-core/src/reasoning.rs` and the
> `OutputStream` enum in `newt-core/src/session.rs:69` (`Stdout|Stderr|AgentThought|ToolCall|Diff…`).
> This is **not a crate** (`newt-response-tags` / `newt-stream-tags` naming is retired): it is the
> **response tag table** living in `newt-core` reasoning/session — roadmap step A1 makes
> `ThinkFilter` tag-table driven with per-provider overrides. Related: #1506 / #1014 / #860.
> See the reconciliation table in [companion-roadmap.md](companion-roadmap.md).

## Overview

A **Streaming Response Categoriser** — an incremental parser that extracts XML-like tags from LLM response streams in real-time, enabling:
- **TTS filtering**: Strip reasoning/thought tags before sending to speech synthesis
- **Reasoning separation**: Clean separation of `<think>`, `<reasoning>`, `<thought>` content from speech
- **Artifact extraction**: Pull structured data (code blocks, tool calls, citations) from streams
- **Provider-agnostic**: Works with any tag format the model emits

## Motivation

Current `newt-core::reasoning::split_reasoning`:
- Only handles hardcoded tags (`<think>`, `<thinking>`)
- Requires complete response (not streaming)
- No TTS filtering hook
- Coupled to specific provider formats

A better approach:
- Dynamic tag detection (any `<tag>content</tag>`)
- Incremental streaming with O(chunk) state machine
- `isSpeechAt(position)` + `filterToSpeech(text, position)` for TTS
- Periodic re-categorization fallback

## Design

### Core Types

```rust
// response tag table (newt-core reasoning/session) — sketch, formerly newt-response-tags/src/types.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResponseCategory {
    Speech,      // User-facing content (sent to TTS)
    Reasoning,   // Internal reasoning (filtered from TTS)
    ToolCall,    // Structured tool invocation
    Artifact,    // Code, citations, structured data
    Unknown,     // Unrecognized tag
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategorizedSegment {
    pub category: ResponseCategory,
    pub tag_name: String,           // Actual tag found: "think", "reasoning", "tool_call"
    pub content: String,            // Tag content (without tags)
    pub raw: String,                // Full tagged content including tags
    pub start_offset: usize,        // Byte offset in full response
    pub end_offset: usize,
    pub metadata: HashMap<String, serde_json::Value>, // Extracted attributes
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CategorizedResponse {
    pub segments: Vec<CategorizedSegment>,
    pub speech: String,             // Concatenated speech content
    pub reasoning: String,          // Concatenated reasoning content
    pub artifacts: Vec<Artifact>,   // Extracted structured artifacts
    pub raw: String,                // Original full response
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub content: String,
    pub language: Option<String>,   // For code blocks
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ArtifactKind {
    CodeBlock,
    ToolCall,
    Citation,
    FileReference,
    Custom(String),
}
```

### Streaming Categoriser

```rust
// response tag table (newt-core reasoning/session) — sketch, formerly newt-response-tags/src/streaming.rs
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct StreamingCategoriser {
    /// Incremental tag state machine
    state: TagStateMachine,
    /// Accumulated buffer
    buffer: String,
    /// Current categorization
    categorized: Option<CategorizedResponse>,
    /// Callback for completed segments
    on_segment: Option<Arc<dyn Fn(CategorizedSegment) + Send + Sync>>,
    /// Configuration
    config: CategoriserConfig,
}

#[derive(Debug, Clone)]
pub struct CategoriserConfig {
    /// Re-categorize every N bytes (fallback)
    pub recategorize_interval: usize,
    /// Tag names to treat as reasoning (default: all unknown tags)
    pub reasoning_tags: Vec<String>,
    /// Tag names to treat as speech (explicit allowlist)
    pub speech_tags: Vec<String>,
    /// Extract code blocks as artifacts
    pub extract_code_blocks: bool,
    /// Maximum buffer size before forcing flush
    pub max_buffer_size: usize,
}

impl Default for CategoriserConfig {
    fn default() -> Self {
        Self {
            recategorize_interval: 1024,
            reasoning_tags: vec![
                "think".into(), "thinking".into(), "reasoning".into(),
                "thought".into(), "internal".into(), "scratchpad".into(),
            ],
            speech_tags: vec!["speech".into(), "say".into(), "tts".into()],
            extract_code_blocks: true,
            max_buffer_size: 64 * 1024,
        }
    }
}

/// Lightweight state machine for tag detection (O(chunk))
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagState {
    Outside,
    InOpeningTag,
    InContent,
    InClosingTag,
}

struct TagStateMachine {
    state: TagState,
    depth: usize,
    current_tag: Option<String>,
    tag_start: usize,
    content_start: usize,
}

impl StreamingCategoriser {
    pub fn new(config: CategoriserConfig) -> Self;
    
    pub fn with_segment_callback<F>(mut self, f: F) -> Self
    where
        F: Fn(CategorizedSegment) + Send + Sync + 'static;
    
    /// Consume a chunk of streaming response
    /// Returns speech-filtered text ready for TTS
    pub fn consume(&mut self, chunk: &str) -> Result<String, CategoriserError>;
    
    /// Check if position in current buffer is speech
    pub fn is_speech_at(&self, position: usize) -> bool;
    
    /// Filter text to speech-only (removes reasoning tags)
    pub fn filter_to_speech(&self, text: &str, start_position: usize) -> String;
    
    /// Get current categorization (may be incomplete)
    pub fn current(&self) -> Option<&CategorizedResponse>;
    
    /// Finalize and return complete categorization
    pub fn finalize(mut self) -> CategorizedResponse;
}
```

### Incremental Parsing Algorithm

```rust
// response tag table (newt-core reasoning/session) — sketch, formerly newt-response-tags/src/incremental.rs
impl TagStateMachine {
    /// Process chunk, return true if outermost tag just closed
    fn process_chunk(&mut self, chunk: &str, buffer_len: usize) -> Vec<TagEvent> {
        let mut events = Vec::new();
        let mut tag_buffer = String::new();
        
        for (i, ch) in chunk.char_indices() {
            match self.state {
                TagState::Outside => {
                    if ch == '<' {
                        // Check for closing tag
                        if chunk.get(i+1..i+2) == Some("/") {
                            self.state = TagState::InClosingTag;
                            tag_buffer.clear();
                        } else {
                            self.state = TagState::InOpeningTag;
                            tag_buffer.clear();
                        }
                    }
                }
                TagState::InOpeningTag => {
                    if ch == '>' {
                        // Tag name complete
                        self.current_tag = Some(tag_buffer.clone());
                        self.depth += 1;
                        self.content_start = buffer_len + i + 1;
                        self.state = TagState::InContent;
                        tag_buffer.clear();
                    } else if !ch.is_whitespace() {
                        tag_buffer.push(ch);
                    }
                }
                TagState::InContent => {
                    if ch == '<' && chunk.get(i+1..i+2) == Some("/") {
                        self.state = TagState::InClosingTag;
                    }
                }
                TagState::InClosingTag => {
                    if ch == '>' {
                        self.depth -= 1;
                        if self.depth == 0 {
                            // Outermost tag closed!
                            events.push(TagEvent::TagClosed {
                                tag_name: self.current_tag.take().unwrap(),
                                start: self.tag_start,
                                end: buffer_len + i + 1,
                            });
                            self.state = TagState::Outside;
                        } else {
                            self.state = TagState::InContent;
                        }
                    } else if !ch.is_whitespace() {
                        // Validate closing tag name matches
                    }
                }
            }
        }
        events
    }
}
```

### TTS Integration

```rust
// response tag table (newt-core reasoning/session) — sketch, formerly newt-response-tags/src/tts.rs
pub struct TtsFilter {
    categoriser: StreamingCategoriser,
    position: usize,
}

impl TtsFilter {
    pub fn new(config: CategoriserConfig) -> Self {
        Self {
            categoriser: StreamingCategoriser::new(config),
            position: 0,
        }
    }
    
    /// Process chunk, return speech-only text for TTS
    pub fn process(&mut self, chunk: &str) -> Result<String, CategoriserError> {
        let filtered = self.categoriser.consume(chunk)?;
        self.position += chunk.len();
        Ok(filtered)
    }
    
    /// Check if we're currently inside a reasoning tag
    pub fn is_in_reasoning(&self) -> bool {
        !self.categoriser.is_speech_at(self.position)
    }
    
    /// Get accumulated reasoning for display
    pub fn reasoning_so_far(&self) -> String {
        self.categoriser.current()
            .map(|c| c.reasoning.clone())
            .unwrap_or_default()
    }
}
```

### Integration with Newt Inference

```rust
// newt-inference/src/response_categoriser.rs
use newt_response_tags::{StreamingCategoriser, CategoriserConfig, TtsFilter};

pub struct CategorisedStream {
    inner: Box<dyn Stream<Item = Result<String, InferenceError>> + Send>,
    categoriser: StreamingCategoriser,
    tts_filter: Option<TtsFilter>,
}

impl Stream for CategorisedStream {
    type Item = Result<CategorisedChunk, InferenceError>;
    
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let speech = self.categoriser.consume(&chunk)?;
                let reasoning = self.categoriser.current()
                    .map(|c| c.reasoning.clone())
                    .unwrap_or_default();
                
                Poll::Ready(Some(Ok(CategorisedChunk {
                    speech,
                    reasoning,
                    raw: chunk,
                    is_complete: false,
                })))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                let final_cat = self.categoriser.finalize();
                Poll::Ready(Some(Ok(CategorisedChunk {
                    speech: final_cat.speech,
                    reasoning: final_cat.reasoning,
                    raw: final_cat.raw,
                    is_complete: true,
                })))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct CategorisedChunk {
    pub speech: String,      // Ready for TTS/display
    pub reasoning: String,   // Accumulated reasoning
    pub raw: String,         // Original chunk
    pub is_complete: bool,
}
```

## Implementation Phases

### Phase 1: Core Crate (`newt-response-tags`)
- `CategorizedSegment`, `CategorizedResponse`, `Artifact` types
- `StreamingCategoriser` with incremental state machine
- `categorize_response()` for complete responses (batch mode)
- Comprehensive tests with various tag formats

### Phase 2: TTS Filter
- `TtsFilter` wrapper
- `is_speech_at()` + `filter_to_speech()` APIs
- Incomplete tag handling (buffer until close or timeout)

### Phase 3: Inference Integration (`newt-inference`)
- `CategorisedStream` wrapper for any `Stream<String>`
- Provider-specific config presets (OpenAI, Anthropic, local)
- Replace `reasoning::split_reasoning` usage

### Phase 4: TUI/Web Display
- `newt-tui`: Reasoning panel with live updates
- `newt-web`/`gilamonster-web`: Streaming reasoning visualization
- Artifact extraction → panel widgets (code, citations)

### Phase 5: Advanced Features
- Custom tag handlers (plugin system)
- Structured tool call parsing from `<tool_call>` tags
- Citation/link extraction
- Metrics: reasoning token ratio, latency

## Configuration Examples

```toml
# newt-config.toml
[response_categoriser]
recategorize_interval = 1024
reasoning_tags = ["think", "thinking", "reasoning", "thought", "internal", "scratchpad", "analysis"]
speech_tags = ["speech", "say", "tts", "answer", "response"]
extract_code_blocks = true
max_buffer_size = 65536

# Provider-specific overrides
[response_categoriser.providers."anthropic"]
reasoning_tags = ["thinking"]  # Claude uses <thinking>

[response_categoriser.providers."openai"]
reasoning_tags = []  # o1 uses special tokens, not tags

[response_categoriser.providers."local-llama"]
reasoning_tags = ["think", "reasoning", "reflection"]
```

## Benefits

| Feature | Current (`split_reasoning`) | Streaming Categoriser |
|---------|----------------------------|----------------------|
| Tag detection | Hardcoded list | Dynamic (any XML-like tag) |
| Streaming | No (needs full response) | Yes (incremental) |
| TTS filtering | Manual post-processing | Built-in `filter_to_speech()` |
| Reasoning display | Post-hoc only | Live `reasoning_so_far()` |
| Artifacts | None | Code blocks, tool calls, citations |
| Provider configs | None | Per-provider tag presets |
| Performance | O(n) full parse | O(chunk) state machine |

## Known sketch defects

The code above is a sketch and does not compile / is not correct as written:

- `?` used inside `Poll` returns.
- `finalize(mut self)` on a pinned field.
- A `<` at a chunk boundary is mishandled (tag split across chunks is lost).
- Closing tag name is not validated against the opening tag.
- Unknown tag defaults to *reasoning*, swallowing generics (`Vec<T>`) and HTML in prose.
- `TagEvent::Text` is referenced by speech-pipeline.md but not defined here; define it as the
  plain-text (speech-eligible) event when A1 lands.

## Open Questions

1. **Malformed tags**: How to handle unclosed tags at stream end? (Current: treat as speech)
2. **Nested tags**: Support `<think><analysis>...</analysis></think>`? (State machine handles depth)
3. **Special tokens**: Some models use `<|thinking|>...<|end|>` — handle via preprocessor?
4. **Performance**: Benchmark vs regex-based extraction for high-throughput
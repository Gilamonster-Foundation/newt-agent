//! Bounded SSE text suffix detector; wire envelopes and tool arguments do not count.
pub(super) struct Watch {
    pending: Vec<u8>,
    text: Vec<u8>,
    finished: bool,
    repeats: usize,
}

impl Default for Watch {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            text: Vec::new(),
            finished: false,
            repeats: std::env::var("NEWT_GENERATION_REPEAT_LIMIT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8),
        }
    }
}

impl Watch {
    pub(super) fn observe(&mut self, chunk: &[u8]) -> anyhow::Result<()> {
        self.pending.extend_from_slice(chunk);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = self.pending.drain(..=end).collect();
            let Ok(line) = std::str::from_utf8(&line) else {
                continue;
            };
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            if data.trim() == "[DONE]" {
                self.finished = true;
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            if self.finished {
                continue;
            }
            let Some(choices) = value["choices"].as_array() else {
                continue;
            };
            for choice in choices {
                if choice["finish_reason"].as_str().is_some() {
                    self.finished = true;
                    continue;
                }
                for field in ["content", "reasoning_content", "reasoning"] {
                    if let Some(text) = choice["delta"][field].as_str() {
                        self.text.extend_from_slice(text.as_bytes());
                        if self.text.len() > 8192 {
                            self.text.drain(..self.text.len() - 8192);
                        }
                        if self.repeats >= 2 && repeated(&self.text, self.repeats) {
                            anyhow::bail!("repeated generation");
                        }
                    }
                }
            }
        }
        // Huge tool-argument lines aren't repetition evidence. The deadline still
        // applies; never retain an unbounded second copy of the response.
        if self.pending.len() > 1024 * 1024 {
            self.pending.clear();
        }
        Ok(())
    }
}

fn repeated(text: &[u8], repeats: usize) -> bool {
    (64..=256.min(text.len() / repeats)).any(|width| {
        let suffix = &text[text.len() - width..];
        (2..=repeats).all(|n| &text[text.len() - n * width..text.len() - (n - 1) * width] == suffix)
    })
}

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
                .unwrap_or(0),
        }
    }
}

impl Watch {
    pub(super) fn observe(&mut self, chunk: &[u8]) -> anyhow::Result<()> {
        if self.repeats < 2 {
            return Ok(());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::tools::disable_ocap_tests::EnvVar;

    fn repeated_frame() -> Vec<u8> {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices":[{"delta":{"content":"x".repeat(64)}}]})
        )
        .into_bytes()
    }

    /// #2824: legitimate repetitive output is allowed unless the operator opts in.
    #[test]
    fn repetition_is_disabled_without_operator_opt_in() {
        let _env = crate::process_env::lock();
        let _unset = EnvVar::unset("NEWT_GENERATION_REPEAT_LIMIT");
        let mut watch = Watch::default();
        for _ in 0..20 {
            watch
                .observe(&repeated_frame())
                .expect("heuristic must default off");
        }
    }

    #[test]
    fn explicit_repetition_threshold_retains_detection() {
        let _env = crate::process_env::lock();
        let _limit = EnvVar::set("NEWT_GENERATION_REPEAT_LIMIT", "3");
        let mut watch = Watch::default();
        watch.observe(&repeated_frame()).unwrap();
        watch.observe(&repeated_frame()).unwrap();
        assert!(watch.observe(&repeated_frame()).is_err());
    }
}

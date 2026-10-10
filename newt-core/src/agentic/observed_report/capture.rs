//! Capture the execution boundary, before display trimming or spilling.
use crate::agentic::display::ToolPresentation;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub(crate) struct Capture {
    pub(super) command: Option<(String, String)>,
    pub(super) exit: Option<i64>,
    pub(super) lines: Vec<String>,
    pub(super) truncated: bool,
}

pub(super) fn bounded(text: &str, limit: usize) -> String {
    crate::agentic::compress::redact_secrets(text)
        .chars()
        .take(limit)
        .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
        .collect()
}

impl Capture {
    fn select(&mut self, command: &str, cwd: &str) {
        // Inventory the retained source, never lifecycle resolution again. The
        // recorded exit is the entire command's exit, not a pipeline child's.
        let check = agent_bridle::inspect_shell(command)
            .ok()
            .is_some_and(|inspection| {
                inspection.commands.iter().any(|c| {
                    c.program.as_deref().is_some_and(|p| {
                        matches!(p.rsplit(['/', '\\']).next(), Some("cargo" | "cargo.exe"))
                    }) && c.argv.get(1).is_some_and(|v| {
                        matches!(
                            v.as_str(),
                            "check" | "test" | "clippy" | "build" | "nextest"
                        )
                    })
                })
            });
        *self = Self::default();
        // Refuse oversized identities instead of merging distinct truncated commands.
        if check && command.len() <= 4096 && cwd.len() <= 4096 {
            self.command = Some((bounded(command, 4096), bounded(cwd, 4096)));
        }
    }
    fn envelope(&mut self, envelope: &serde_json::Value) {
        if self.command.is_none() {
            return;
        }
        self.exit = envelope["exit_code"].as_i64();
        self.truncated = envelope["stdout_truncated"].as_bool() == Some(true)
            || envelope["stderr_truncated"].as_bool() == Some(true);
        self.lines.clear();
        for stream in ["stdout", "stderr"] {
            for line in envelope[stream].as_str().unwrap_or("").lines() {
                let plain = crate::tty::width::strip_ansi(line);
                let line = plain.trim();
                if line.starts_with("test result:")
                    || line.starts_with("Finished ")
                    || line.starts_with("error:")
                    || line.starts_with("Summary ")
                {
                    if self.lines.len() == 8 {
                        self.lines.remove(0);
                        self.truncated = true;
                    }
                    self.truncated |= line.chars().count() > 512;
                    self.lines.push(bounded(line, 512));
                }
            }
        }
    }
}

/// An observation tap on the existing result delivery seam. It never changes
/// presentation or interprets a model-visible string as an execution receipt.
pub(crate) struct Presentation<'a> {
    pub(crate) inner: &'a mut dyn ToolPresentation,
    pub(crate) capture: Option<&'a Mutex<Capture>>,
}
impl ToolPresentation for Presentation<'_> {
    fn preview(&mut self, output: &str, max_lines: usize) {
        self.inner.preview(output, max_lines);
    }
    fn document(&mut self, output: &str) {
        self.inner.document(output);
    }
    fn override_result(&mut self, output: String) {
        self.inner.override_result(output);
    }
    fn file_change(&mut self, change: Arc<crate::agentic::FileChangePresentation>) {
        self.inner.file_change(change);
    }
    fn display_source(&mut self, full: String) {
        self.inner.display_source(full);
    }
    fn execution_command(&mut self, command: &str, cwd: &str) {
        if let Some(capture) = self.capture {
            capture.lock().expect("report capture").select(command, cwd);
        }
        self.inner.execution_command(command, cwd);
    }
    fn execution_result(&mut self, envelope: &serde_json::Value) {
        if let Some(capture) = self.capture {
            capture.lock().expect("report capture").envelope(envelope);
        }
        self.inner.execution_result(envelope);
    }
}

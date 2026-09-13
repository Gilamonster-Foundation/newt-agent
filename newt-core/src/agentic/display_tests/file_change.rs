//! The actual ToolDisplay writer grounds the pure change projection, including
//! completed-view routing, text retention, and byte offsets before escaping.

use super::*;
use crate::agentic::{CompletedSpillRenderer, FileChangePresentation};
use std::sync::{Arc, Mutex};

fn fixture(prefix: &str, suffix: &str) -> (String, Arc<FileChangePresentation>) {
    let changes = newtui::diff::from_unified(
        "--- state.rs\n+++ state.rs\n@@ -1 +1 @@\n-let old = 1;\n+let new = 2;\n",
    )
    .unwrap();
    let receipt = format!("\n\nModified (+1 -1)\n\n{}", changes.to_markdown());
    let raw = format!("{prefix}{receipt}{suffix}");
    let range = prefix.len()..prefix.len() + receipt.len();
    (
        raw,
        Arc::new(FileChangePresentation::new(
            changes,
            "state.rs".into(),
            Some("let old = 1;\n".into()),
            Some("let new = 2;\n".into()),
            receipt,
            range,
        )),
    )
}

#[test]
fn a_rich_change_replaces_only_its_exact_receipt_and_is_consumed_once() {
    let (raw, hint) = fixture("wrote state.rs", "\nbuild check failed: keep this outcome");
    let mut display = ToolDisplay::new(Vec::new(), true, 80, 0, false);
    display.file_change(hint);
    display.result(&raw);
    display.result(&raw);
    let visible = String::from_utf8(display.into_inner()).unwrap();
    assert_eq!(
        visible.matches("Modified state.rs").count(),
        1,
        "one rich projection: {visible:?}"
    );
    assert_eq!(
        visible.matches("```diff").count(),
        1,
        "the next result uses its own plain path"
    );
    assert!(
        visible.contains("\x1b[48;"),
        "semantic background is painted"
    );
    assert_eq!(
        visible
            .matches("build check failed: keep this outcome")
            .count(),
        2
    );
    assert_eq!(visible.matches("wrote state.rs").count(), 2);
}

#[test]
fn plain_or_mismatched_hints_preserve_the_existing_full_text_path() {
    for (color, altered) in [(false, false), (true, true)] {
        let (raw, hint) = fixture("wrote state.rs", "\nverification failed");
        let shown = if altered {
            raw.replace("Modified", "Different")
        } else {
            raw
        };
        let mut expected = ToolDisplay::new(Vec::new(), color, 40, 5, false);
        expected.result(&shown);
        let mut actual = ToolDisplay::new(Vec::new(), color, 40, 5, false);
        actual.file_change(hint);
        actual.result(&shown);
        assert_eq!(actual.into_inner(), expected.into_inner());
    }
}

#[derive(Clone, Default)]
struct Terminal(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Terminal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Renderer {
    terminal: Terminal,
    retained: Mutex<Vec<String>>,
    rich: Mutex<Vec<String>>,
    seen: Mutex<Vec<String>>,
}
impl CompletedSpillRenderer for Renderer {
    fn retain_completed(&self, output: &str) -> Option<u64> {
        self.retained.lock().unwrap().push(output.into());
        Some(9)
    }
    fn render_completed(&self, _: &str, _: usize, _: usize) -> usize {
        0
    }
    fn render_file_change(
        &self,
        _: &str,
        output: &str,
        _: Arc<FileChangePresentation>,
        _: usize,
        _: usize,
    ) -> usize {
        self.rich.lock().unwrap().push(output.into());
        self.seen
            .lock()
            .unwrap()
            .push(String::from_utf8(self.terminal.0.lock().unwrap().clone()).unwrap());
        3
    }
    fn is_active(&self) -> bool {
        false
    }
    fn erase(&self) {}
}

#[test]
fn rich_static_rows_commit_before_live_render_while_retention_keeps_text() {
    let (raw, hint) = fixture("wrote state.rs", "\nbuild check failed");
    let renderer = Arc::new(Renderer::default());
    let mut display = ToolDisplay::new(renderer.terminal.clone(), true, 80, 0, false);
    display.set_completed_spill_renderer(renderer.clone());
    display.file_change(hint);
    display.result(&raw);
    assert_eq!(*renderer.retained.lock().unwrap(), vec![raw.clone()]);
    assert_eq!(*renderer.rich.lock().unwrap(), vec![raw]);
    let seen = renderer.seen.lock().unwrap();
    assert!(seen[0].contains("Modified state.rs"));
    assert!(seen[0].contains("build check failed"));
    assert!(!seen[0].contains("```diff"));
}

#[test]
fn raw_ranges_are_validated_before_a_longer_control_escaped_override() {
    let (raw, hint) = fixture("wrote é\u{1b}[31m\tstate.rs", "\nbuild check failed\r");
    let safe = crate::notes_scan::neutralize_for_display(&raw).replace('\t', "<U+0009>");
    let mut display = ToolDisplay::new(Vec::new(), true, 80, 0, false);
    display.override_result(safe);
    display.file_change(hint);
    display.result(&raw);
    let visible = String::from_utf8(display.into_inner()).unwrap();
    assert!(
        visible.contains("Modified state.rs"),
        "raw receipt range remains valid"
    );
    assert!(visible.contains("é<U+001B>[31m<U+0009>state.rs"));
    assert!(visible.contains("build check failed<U+000D>"));
    assert!(!visible.contains("\x1b[31m"));
    assert!(!visible.contains('\r'));
    assert!(!visible.contains("```diff"));
}

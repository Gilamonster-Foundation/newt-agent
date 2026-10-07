use super::*;

#[derive(Default)]
struct Retained(std::sync::Mutex<Vec<String>>);
impl CompletedSpillRenderer for Retained {
    fn retain_completed(&self, body: &str) -> Option<u64> {
        self.0.lock().unwrap().push(body.into());
        Some(7)
    }
    fn render_completed(&self, _: &str, _: usize, _: usize) -> usize {
        0
    }
    fn is_active(&self) -> bool {
        false
    }
    fn erase(&self) {}
    fn discard(&self) {}
}

/// PR #2799 finding 2: the actual emitter must budget wrapped terminal rows,
/// even when all prose is one paragraph, and retain every byte behind its fold.
#[test]
fn narration_2799_emitter_budgets_rendered_rows() {
    for text in [
        "One two three four five six seven eight nine ten eleven twelve.",
        "Short intro.\n\nOne two three four five six seven eight nine ten eleven twelve.\n\nFinal paragraph.",
    ] {
        let retain = Retained::default();
        let paragraphs = display::tool_round_prose(text);
        let rendered: Vec<_> = paragraphs.iter().flat_map(|p| crate::tty::wrap_line(p, 12)).collect();
        let mut output = Vec::new();
        emit_tool_round_narration(text, false, Some(&retain), 12, 2, |level, glyph, line| {
            if glyph == "▲" {
                assert_eq!(*retain.0.lock().unwrap(), vec![paragraphs.join("\n")]);
            }
            output.push((level, glyph.to_string(), line.to_string()));
        });
        assert_eq!(output.len(), 3, "two physical rows plus one fold: {output:?}");
        assert_eq!(output[0].1, "▹");
        assert_eq!(output[1].1, " ");
        assert_eq!(output[0].2, rendered[0]);
        assert_eq!(output[1].2, rendered[1]);
        assert_eq!(output[2].1, "▲");
        assert!(output[2].2.contains(&format!("{} lines hidden", rendered.len() - 2)), "{output:?}");
        assert!(output[2].2.contains("/spill open 7"));
    }
}

/// PR #2799: already-streamed/empty prose emits nothing; zero means unlimited.
#[test]
fn narration_2799_emitter_skip_and_unlimited() {
    for (content, streamed) in [("Readable prose", true), ("", false)] {
        emit_tool_round_narration(content, streamed, None, 12, 2, |_, _, _| {
            panic!("unexpected output")
        });
    }
    let retain = Retained::default();
    let text = "One two three four five six seven eight.";
    let mut output = Vec::new();
    emit_tool_round_narration(text, false, Some(&retain), 12, 0, |_, _, text| {
        output.push(text.to_string());
    });
    assert_eq!(output, crate::tty::wrap_line(text, 12));
    assert!(retain.0.lock().unwrap().is_empty());
}

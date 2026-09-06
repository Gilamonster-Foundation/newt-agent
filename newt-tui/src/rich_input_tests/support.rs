use super::*;

/// PRODUCTION source of all editor modules — see [`crate::production_source`]
/// for why the cut is at the test MODULE and why a missing marker panics.
pub(super) fn production() -> &'static str {
    concat!(
        include_str!("../rich_input/command.rs"),
        include_str!("../rich_input/geometry.rs"),
        include_str!("../rich_input/gutter.rs"),
        include_str!("../rich_input/mounted.rs"),
        include_str!("../rich_input.rs")
    )
    .split_once("\n#[cfg(test)]\n#[path = \"rich_input_tests/support.rs\"]\nmod test_support;")
    .expect("rich input must retain its exact test-support module boundary")
    .0
}

#[derive(Default)]
pub(super) struct RecordingSink {
    pub(super) batches: Vec<Vec<Line<'static>>>,
}

impl ScrollbackSink for RecordingSink {
    fn insert(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        self.batches.push(lines);
        Ok(())
    }
}

pub(super) fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

pub(super) fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

pub(super) fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

pub(super) fn special(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn vi_editor() -> Editor {
    Editor::new(Edit::Vi)
}

pub(super) fn emacs_editor() -> Editor {
    Editor::new(Edit::Emacs)
}

/// Drive a sequence of chars (in NORMAL-friendly contexts) and return lines.
pub(super) fn type_chars(ed: &mut Editor, ta: &mut TextArea, s: &str) {
    for c in s.chars() {
        ed.input(key(c), ta);
    }
}

pub(super) fn nano_editor() -> Editor {
    Editor::new(Edit::Nano)
}

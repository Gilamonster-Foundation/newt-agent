//! Copied report envelopes are model prose, never another source of facts.
use super::{CHECKS_HEADING, FILES_HEADING, NOTICES_HEADING, PUBLICATIONS_HEADING};

/// Remove only complete report-shaped envelopes outside quoted examples.
/// This is presentation cleanup, never verification of a displayed CID.
pub(in crate::agentic) fn model_prose(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut remaining = text;
    let mut fence = Fence::default();
    while !remaining.is_empty() {
        let end = remaining.find('\n').map_or(remaining.len(), |n| n + 1);
        let line = &remaining[..end];
        // Advance fences BEFORE recognizing a report or its closing delimiter.
        if !fence.advance(line) {
            if let Some((_, rest)) = split_report(remaining) {
                remaining = rest;
                continue;
            }
        }
        out.push_str(line);
        remaining = &remaining[end..];
    }
    out
}

/// The rendered reply is the operator's view. Project recognized legacy reports
/// out of assistant speech without treating displayed text as trusted evidence.
pub fn assistant_prose(text: &str) -> String {
    model_prose(text)
}

pub(crate) fn replay_messages(messages: &[crate::MemMessage]) -> Vec<crate::MemMessage> {
    messages
        .iter()
        .cloned()
        .map(|mut message| {
            if message.role == crate::Role::Assistant {
                message.content = assistant_prose(&message.content);
            }
            message
        })
        // A harness-only stop has no assistant speech. In particular, do not
        // send an empty assistant content block to providers that reject it.
        .filter(|message| message.role != crate::Role::Assistant || !message.content.is_empty())
        .collect()
}

/// One conservative recognizer for display artifacts, persistence and replay.
/// Any partial, fenced or extra prose inside the candidate makes us abstain.
pub(crate) fn split_report(text: &str) -> Option<(&str, &str)> {
    if !text.starts_with("## Observed\n\n") {
        return None;
    }
    let mut fence = Fence::default();
    let mut offset: usize = 0;
    for line in text.split_inclusive('\n') {
        let quoted = fence.advance(line);
        if !quoted && line == "## Model explanation\n" {
            let report_end = offset.checked_sub(1)?;
            let report = text.get(..report_end)?;
            let rest = text.get(offset + line.len()..)?.strip_prefix('\n')?;
            return complete_report(report).then_some((report, rest));
        }
        offset += line.len();
    }
    None
}

fn complete_report(report: &str) -> bool {
    let Some(report) = report.strip_suffix('\n') else {
        return false;
    };
    let mut parts: Vec<_> = report.split("\n\n").collect();
    if let Some(status) = parts.last().filter(|s| s.starts_with(NOTICES_HEADING)) {
        let Some(rows) = status
            .strip_prefix(NOTICES_HEADING)
            .and_then(|s| s.strip_prefix('\n'))
        else {
            return false;
        };
        if rows.is_empty()
            || !rows
                .lines()
                .all(|s| s.strip_prefix("- ").is_some_and(literal))
        {
            return false;
        }
        parts.pop();
    }
    if parts.len() == 2 && parts[0] == "## Observed" {
        return matches!(
            parts[1],
            "Facts unavailable: root retention limit reached."
                | "Facts unavailable: content encoding failed."
        );
    }
    if !(5..=7).contains(&parts.len()) || parts[0] != "## Observed" {
        return false;
    }
    let Some((id, root)) = parts[1]
        .strip_prefix("Report `")
        .and_then(|s| s.split_once("` — root "))
    else {
        return false;
    };
    if id.is_empty()
        || id.contains(['`', '\n'])
        || !root.strip_suffix('.').is_some_and(literal)
        || !section(parts[2], FILES_HEADING, false)
        || !section(parts[3], CHECKS_HEADING, true)
        || !section(parts[4], PUBLICATIONS_HEADING, false)
    {
        return false;
    }
    let mut optional = &parts[5..];
    if optional.first() == Some(&"Older observations omitted by retention limits.") {
        optional = &optional[1..];
    }
    if let Some(ambiguous) = optional
        .first()
        .and_then(|s| s.strip_prefix("Unverified ambiguous bare paths in model explanation: "))
    {
        if !ambiguous
            .strip_suffix('.')
            .is_some_and(|paths| paths.split(", ").all(literal))
        {
            return false;
        }
        optional = &optional[1..];
    }
    optional.is_empty()
}

fn section(text: &str, heading: &str, checks: bool) -> bool {
    let mut lines = text.lines();
    if lines.next() != Some(heading) {
        return false;
    }
    let mut rows = 0;
    for line in lines {
        if line
            .strip_prefix("- ")
            .is_some_and(|row| report_row(row, heading))
        {
            rows += 1;
        } else if !(checks
            && rows > 0
            && (line == "  Result excerpt incomplete; totals unavailable."
                || line.strip_prefix("  ").is_some_and(literal)))
        {
            return false;
        }
    }
    rows > 0
}

fn report_row(row: &str, heading: &str) -> bool {
    match heading {
        FILES_HEADING => {
            matches!(
                row,
                "No content changes observed."
                    | "File counts unavailable: incomplete or unauthorized snapshot."
            ) || row
                .strip_suffix(" additional file rows omitted.")
                .is_some_and(|n| n.parse::<usize>().is_ok())
                || after_literal(row)
                    .and_then(|s| s.strip_prefix(": "))
                    .and_then(|s| s.split_once(" → "))
                    .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())
        }
        CHECKS_HEADING => {
            row == "No check observation available."
                || after_literal(row)
                    .and_then(|s| s.strip_prefix(" in "))
                    .and_then(after_literal)
                    .and_then(|s| s.strip_prefix(": "))
                    .and_then(|s| s.strip_suffix('.'))
                    .and_then(|s| s.split_once("; exit "))
                    .is_some_and(|(outcome, exit)| {
                        !outcome.is_empty()
                            && outcome.chars().all(|c| c.is_ascii_alphabetic())
                            && (exit == "unavailable" || exit.parse::<i64>().is_ok())
                    })
        }
        PUBLICATIONS_HEADING => row == "No governed receipt available." || literal(row),
        _ => false,
    }
}

fn after_literal(text: &str) -> Option<&str> {
    let width = text.bytes().take_while(|b| *b == b'`').count();
    if width == 0 {
        return None;
    }
    let end = width + text[width..].find(&text[..width])? + width;
    literal(&text[..end]).then_some(&text[end..])
}

/// Match the variable-width inline code spans used by the report renderer.
fn literal(text: &str) -> bool {
    let width = text.bytes().take_while(|b| *b == b'`').count();
    width > 0
        && text.len() >= width * 2
        && !text.contains('\n')
        && text[width..]
            .strip_suffix(&text[..width])
            .is_some_and(|inner| inner.split(|c| c != '`').all(|run| run.len() < width))
}

#[derive(Default)]
struct Fence(Option<(u8, usize)>);
impl Fence {
    /// True for both boundary lines and every line inside the current fence.
    fn advance(&mut self, line: &str) -> bool {
        let was_open = self.0.is_some();
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() <= 3 {
            if let Some(marker @ (b'`' | b'~')) = trimmed.bytes().next() {
                let width = trimmed.bytes().take_while(|b| *b == marker).count();
                let tail = &trimmed[width..];
                match self.0 {
                    None if width >= 3 && (marker != b'`' || !tail.contains('`')) => {
                        self.0 = Some((marker, width));
                    }
                    Some((open, count))
                        if marker == open && width >= count && tail.trim().is_empty() =>
                    {
                        self.0 = None;
                    }
                    _ => {}
                }
            }
        }
        was_open || self.0.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn genuine_report() -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut state = super::super::State::default();
        state.bind(dir.path(), &crate::Scope::All);
        state.observe(
            dir.path(),
            &super::super::Capture {
                command: Some(("cargo check".into(), "project".into())),
                exit: Some(0),
                lines: vec!["## Model explanation".into(), "```fenced sample```".into()],
                truncated: true,
            },
            None,
            Some(&crate::git_staging::Outcome::PrCreated {
                url: "https://github.com/example/project/pull/7".into(),
            }),
        );
        format!(
            "{}Explanation.",
            state.render(dir.path(), &crate::Scope::All, "Explanation.")
        )
    }

    /// A complete real prefix still cannot claim a closing delimiter inside a
    /// nested example; blank lines in model prose are otherwise kept verbatim.
    #[test]
    fn observed_report_complete_prefix_with_fenced_delimiter_is_untouched() {
        let report = genuine_report();
        let (prefix, _) = split_report(&report).unwrap();
        for fence in ["````", "~~~~"] {
            let text = format!("{prefix}\n{fence}markdown\n```text\n## Model explanation\n\nExample prose\n```\n{fence}\nConclusion.");
            assert_eq!(model_prose(&text), text);
            assert_eq!(assistant_prose(&text), text);
            assert!(split_report(&text).is_none());
        }
        let text = format!("{prefix}\n## Model explanation\n\n\nExplanation.");
        assert_eq!(assistant_prose(&text), "\nExplanation.");
    }

    #[test]
    fn observed_report_known_unavailable_envelopes_are_complete() {
        for reason in ["root retention limit reached", "content encoding failed"] {
            let text = format!("## Observed\n\nFacts unavailable: {reason}.\n\n## Model explanation\n\nExplanation.");
            assert_eq!(assistant_prose(&text), "Explanation.");
        }
        let text =
            "## Observed\n\nFacts unavailable: some prose.\n\n## Model explanation\n\nExplanation.";
        assert_eq!(assistant_prose(text), text);
    }

    /// Cleanup must not eat an ordinary heading, partial report, or code sample.
    #[test]
    fn observed_report_cleanup_preserves_other_prose_and_code() {
        let report = genuine_report();
        for text in [
            "## Observed\n\nSome observations.\n\n## Model explanation\n\nText.".to_string(),
            "## Observed\n\nReport `incomplete`".to_string(),
            format!("```markdown\n{report}\n```\n"),
            format!("~~~~markdown\n{report}\n~~~~\n"),
        ] {
            assert_eq!(model_prose(&text), text);
        }
        assert_eq!(model_prose(&report), "Explanation.");
        assert_eq!(assistant_prose(&report), "Explanation.");
    }
    /// PR #2849: preserve prose when a report-like prefix is incomplete or quoted.
    #[test]
    fn observed_report_cleanup_abstains_reviewer() {
        let text = "## Observed\n\nReport `example` illustrates this format.\nFiles since objective/adoption snapshot is a heading.\n\n```markdown\n## Model explanation\n\nExample prose\n```\nConclusion.";
        assert_eq!(model_prose(text), text, "model copy cleanup");
        assert_eq!(assistant_prose(text), text, "persisted/replayed prose");
        assert!(split_report(text).is_none(), "not a complete envelope");
    }

    /// PR #2849: preserve prose when a report-like prefix is incomplete or quoted.
    #[test]
    fn observed_report_cleanup_abstains_partial() {
        let text = "## Observed\n\nReport `example` — root `project`.\n\nFiles since objective/adoption snapshot (LF counts; absence is 0):\n- No content changes observed.\n\nThis is genuine explanatory prose.\n\n## Model explanation\n\nAn unrelated section.";
        assert_eq!(model_prose(text), text, "model copy cleanup");
        assert_eq!(assistant_prose(text), text, "persisted/replayed prose");
        assert!(split_report(text).is_none(), "not a complete envelope");
    }

    /// PR #2849: preserve prose when a report-like prefix is incomplete or quoted.
    #[test]
    fn observed_report_cleanup_abstains_nested() {
        let text = "## Observed\n\nReport `example` illustrates this format.\nFiles since objective/adoption snapshot is a heading.\n\n````markdown\n```text\n## Model explanation\n\nNested example prose\n```\n````\nConclusion.";
        assert_eq!(model_prose(text), text, "model copy cleanup");
        assert_eq!(assistant_prose(text), text, "persisted/replayed prose");
        assert!(split_report(text).is_none(), "not a complete envelope");
    }
}

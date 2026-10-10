//! Copied report envelopes are model prose, never another source of facts.

/// Remove complete imitations of our report envelope before adding the actual
/// report. Keep surrounding explanation and quoted code examples verbatim.
/// This is presentation cleanup, not verification of a model-supplied CID.
pub(in crate::agentic) fn model_prose(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut remaining = text;
    let mut fence: Option<(u8, usize)> = None;
    while !remaining.is_empty() {
        if fence.is_none() && remaining.starts_with("## Observed\n\nReport `") {
            if let Some((report, rest)) = remaining.split_once("\n## Model explanation\n") {
                if report.contains("\nFiles since objective/adoption snapshot ") {
                    remaining = rest.trim_start_matches('\n');
                    continue;
                }
            }
        }
        let end = remaining.find('\n').map_or(remaining.len(), |n| n + 1);
        let line = &remaining[..end];
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() <= 3 {
            if let Some(marker @ (b'`' | b'~')) = trimmed.bytes().next() {
                let width = trimmed.bytes().take_while(|b| *b == marker).count();
                match fence {
                    None if width >= 3 => fence = Some((marker, width)),
                    Some((open, count))
                        if marker == open
                            && width >= count
                            && trimmed[width..].trim().is_empty() =>
                    {
                        fence = None;
                    }
                    _ => {}
                }
            }
        }
        out.push_str(line);
        remaining = &remaining[end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cleanup must not eat an ordinary heading, partial report, or code sample.
    #[test]
    fn observed_report_cleanup_preserves_other_prose_and_code() {
        let report = "## Observed\n\nReport `example` — root `project`.\n\nFiles since objective/adoption snapshot (LF counts; absence is 0):\n- example\n\n## Model explanation\n\nExplanation.";
        for text in [
            "## Observed\n\nSome observations.\n\n## Model explanation\n\nText.".to_string(),
            "## Observed\n\nReport `incomplete`".to_string(),
            format!("```markdown\n{report}\n```\n"),
            format!("~~~~markdown\n{report}\n~~~~\n"),
        ] {
            assert_eq!(model_prose(&text), text);
        }
        assert_eq!(model_prose(report), "Explanation.");
    }
}

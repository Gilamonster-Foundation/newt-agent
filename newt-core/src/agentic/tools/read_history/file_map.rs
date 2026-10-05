//! Bounded whole-file navigation, using the existing Rust parser.

use super::{Cursor, Range};

pub(super) fn render(
    path: &str,
    contents: &str,
    served: &[Range<Cursor>],
    tool_offload: bool,
) -> Option<String> {
    #[cfg(feature = "ast")]
    {
        if std::path::Path::new(path).extension()?.to_str()? != "rs" {
            return None;
        }
        let entries = crate::ast_outline::whole_file_outline(contents)?;
        if entries.is_empty() {
            return None;
        }
        let tokens = super::super::output_budget::read_page_tokens(tool_offload);
        let budget = if tokens == 0 {
            usize::MAX
        } else {
            super::super::output_budget::cap_estimator().chars_for_tokens(tokens)
        };
        let mut heading = format!("Whole-file outline ({} lines). Read this path with the offset shown.\nPages already served: ", contents.lines().count());
        for (i, range) in served.iter().enumerate() {
            let span = if range.start.1 == 0 && range.end.1 == 0 {
                format!("{}-{}", range.start.0, range.end.0 - 1)
            } else {
                format!(
                    "{}:{}..{}:{} (line:char, end exclusive)",
                    range.start.0, range.start.1, range.end.0, range.end.1
                )
            };
            if heading.len() + span.len() + 80 > budget / 2 {
                heading.push_str(&format!("; {} more served ranges", served.len() - i));
                break;
            }
            if i > 0 {
                heading.push_str(", ");
            }
            heading.push_str(&span);
        }
        heading.push('\n');
        // Sample across the WHOLE file when its item index cannot fit; never
        // emit just its head and pretend the remaining definitions don't exist.
        let mut stride = 1;
        loop {
            let mut map = heading.clone();
            if stride > 1 {
                map.push_str(&format!("Sampled index: {} top-level items; omitted items remain accessible with explicit offsets.\n", entries.len()));
            }
            for (i, entry) in entries.iter().enumerate() {
                if i % stride != 0 && i + 1 != entries.len() {
                    continue;
                }
                let header: String = entry.header.chars().take(120).collect();
                map.push_str(&format!(
                    "{}-{}\toffset={}\t{}\n",
                    entry.start_line, entry.end_line, entry.start_line, header
                ));
            }
            if map.len() <= budget {
                return Some(map);
            }
            if stride >= entries.len() {
                // Tiny operator budgets may not fit even two index entries.
                let mut end = budget.min(map.len());
                while !map.is_char_boundary(end) {
                    end -= 1;
                }
                map.truncate(end);
                return Some(map);
            }
            stride = stride.saturating_mul(2);
        }
    }
    #[cfg(not(feature = "ast"))]
    {
        let _ = (path, contents, served, tool_offload);
        None
    }
}

//! Complete-file item spans for read navigation; no fragment recovery.

use super::{parse_rust, OutlineEntry};

pub(crate) fn whole_file_outline(source: &str) -> Option<Vec<OutlineEntry>> {
    let tree = parse_rust(source).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut cursor = root.walk();
    Some(
        root.named_children(&mut cursor)
            .filter(|node| {
                (node.kind().ends_with("_item") && node.kind() != "attribute_item")
                    || matches!(node.kind(), "macro_definition" | "macro_invocation")
            })
            .map(|node| OutlineEntry {
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + usize::from(node.end_position().column > 0),
                header: source[node.byte_range()]
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .trim_end_matches(['{', ';'])
                    .trim()
                    .to_owned(),
                open_ended: false,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2745: complete-file maps include real outer spans, not nested methods
    /// or text in comments/strings; constants unsupported by tags still appear.
    #[test]
    fn map_spans_are_top_level_parser_nodes() {
        let source = "const NAME: &str = \"fn imaginary() {}\";\nimpl Thing {\n fn nested() {}\n}\nmod tests {\n fn test() {}\n}\n";
        let entries = whole_file_outline(source).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.start_line, e.end_line))
                .collect::<Vec<_>>(),
            [(1, 1), (2, 4), (5, 7)]
        );
        assert!(entries[0].header.starts_with("const NAME"));
        assert!(entries[1].header.starts_with("impl Thing"));
        assert!(whole_file_outline("fn broken( {").is_none());
    }
}

//! Pure full-file Rust extraction. Page outlines are never mutation spans.
use std::collections::BTreeSet;
use tree_sitter::Node;

pub(super) struct Extracted {
    pub source: String,
    pub child: String,
}

pub(super) fn extract(source: &str, names: &[String], module: &str) -> Result<Extracted, String> {
    let wanted: BTreeSet<_> = names.iter().map(String::as_str).collect();
    if wanted.is_empty() || wanted.len() != names.len() {
        return Err("move_from needs unique, nonempty item names".into());
    }
    if matches!(
        module,
        "self" | "super" | "crate" | "Self" | "_" | "async" | "await" | "dyn" | "gen" | "try"
    ) || module.is_empty()
        || !module
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err("destination must have a plain Rust module name".into());
    }
    let declaration = format!("mod {module};");
    let declaration_tree = crate::ast_outline::parse_rust(&declaration)?;
    if declaration_tree.root_node().has_error() {
        return Err("destination is not a valid Rust module name".into());
    }
    let tree = crate::ast_outline::parse_rust(source)?;
    let root = tree.root_node();
    if root.has_error() {
        return Err("source must parse as a complete Rust file".into());
    }
    let mut cursor = root.walk();
    let mut pending = Vec::new();
    let mut found = BTreeSet::new();
    let mut cuts = Vec::new();
    let mut child = String::from("#[allow(unused_imports)]\nuse super::*;\n\n");
    let mut wiring = format!("\nmod {module};\n");
    for node in root.named_children(&mut cursor) {
        let text = &source[node.byte_range()];
        if node.kind() == "attribute_item"
            || matches!(node.kind(), "line_comment" | "block_comment")
                && !text.starts_with("//!")
                && !text.starts_with("/*!")
        {
            pending.push(node);
            continue;
        }
        let name = node
            .child_by_field_name("name")
            .map(|n| &source[n.byte_range()]);
        if name == Some(module) {
            return Err("destination module name already exists in the source".into());
        }
        if name.is_some_and(|name| wanted.contains(name)) {
            let name = name.unwrap();
            if node.kind() != "function_item" || !found.insert(name) {
                return Err(format!(
                    "{name}: expected one unambiguous top-level free function"
                ));
            }
            validate_node(node, source)?;
            for attribute in &pending {
                validate_node(*attribute, source)?;
            }
            let visibility = node
                .named_children(&mut node.walk())
                .find(|n| n.kind() == "visibility_modifier")
                .map(|n| {
                    source[n.byte_range()]
                        .split_whitespace()
                        .collect::<String>()
                });
            let import_visibility = match visibility.as_deref() {
                None => "",
                Some("pub") => "pub ",
                Some("pub(crate)") => "pub(crate) ",
                _ => {
                    return Err(
                        "relative/restricted visibility is not supported by move_from".into(),
                    )
                }
            };
            let start = pending
                .first()
                .map_or(node.start_byte(), |n| n.start_byte());
            child.push_str(&source[start..node.start_byte()]);
            if visibility.is_none() {
                child.push_str("pub(super) ");
            }
            child.push_str(text);
            child.push_str("\n\n");
            wiring.push_str(&format!(
                "#[allow(unused_imports)]\n{import_visibility}use {module}::{name};\n"
            ));
            cuts.push(start..node.end_byte());
        }
        pending.clear();
    }
    if found != wanted {
        return Err("every move_from item must name an existing top-level free function".into());
    }
    let mut parent = source.to_owned();
    for range in cuts.into_iter().rev() {
        parent.replace_range(range, "");
    }
    parent.push_str(&wiring);
    // Verify the generated syntax as well as the original, before any write.
    for candidate in [&parent, &child] {
        if crate::ast_outline::parse_rust(candidate)?
            .root_node()
            .has_error()
        {
            return Err("generated extraction did not parse; nothing was changed".into());
        }
    }
    Ok(Extracted {
        source: parent,
        child,
    })
}

fn validate_node(node: Node<'_>, source: &str) -> Result<(), String> {
    match node.kind() {
        "macro_invocation" | "macro_definition" | "super" | "self" => {
            return Err("context-sensitive macros and self/super paths cannot be moved".into())
        }
        "attribute_item" => {
            let text = source[node.byte_range()]
                .trim_start_matches("#[")
                .trim_start();
            let name = text
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            if !matches!(
                name,
                "doc"
                    | "inline"
                    | "cold"
                    | "must_use"
                    | "allow"
                    | "warn"
                    | "deny"
                    | "forbid"
                    | "expect"
            ) {
                return Err("cfg, procedural and unknown attributes cannot be moved".into());
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        validate_node(child, source)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2724: copy the full item, docs and attributes without model retyping.
    #[test]
    fn preserves_items_and_wires_visibility() {
        let source = "//! Parent docs\nconst N: u8 = 4;\n/// Exact docs\n#[inline]\nfn first() -> u8 { N }\n\npub fn second() -> u8 { first() }\n";
        let moved = extract(source, &["first".into(), "second".into()], "helpers").unwrap();
        assert!(moved
            .child
            .contains("/// Exact docs\n#[inline]\npub(super) fn first() -> u8 { N }"));
        assert!(moved.child.contains("pub fn second() -> u8 { first() }"));
        assert!(moved
            .source
            .starts_with("//! Parent docs\nconst N: u8 = 4;"));
        assert!(!moved.source.contains("fn first"));
        assert!(moved.source.contains("mod helpers;"));
        assert!(moved.source.contains("use helpers::first;"));
        assert!(moved.source.contains("pub use helpers::second;"));
    }

    /// #2724: full-file byte spans preserve CRLF, Unicode and block docs exactly.
    #[test]
    fn preserves_bytes_beyond_display_page_limit() {
        let padding = "// padding\n".repeat(30_000);
        let item = "/** café */\r\n#[must_use]\r\npub(crate) fn f() -> &'static str { \"λ\" }";
        // A non-function item separates unrelated leading comments from f.
        let source = format!("{padding}const UNUSED: u8 = 0;\n{item}\r\n");
        let moved = extract(&source, &["f".into()], "child").unwrap();
        assert!(moved.child.contains(item));
        assert_eq!(moved.source, format!("{padding}const UNUSED: u8 = 0;\n\r\n\nmod child;\n#[allow(unused_imports)]\npub(crate) use child::f;\n"));
    }

    /// #2724: ambiguous boundaries and context changes must fail before writes.
    #[test]
    fn refuses_ambiguous_or_context_sensitive_items() {
        for source in [
            "fn f() {} fn f() {}",
            "#[cfg(test)] fn f() {}",
            "#[custom] fn f() {}",
            "fn f() { println!(\"x\"); }",
            "fn f() { super::g(); }",
            "fn f() { self::g(); }",
            "mod m { fn f() {} }",
            "pub(super) fn f() {}",
            "fn f() {",
            "struct f;",
        ] {
            assert!(extract(source, &["f".into()], "child").is_err(), "{source}");
        }
        assert!(extract("fn f() {}", &["f".into(), "f".into()], "child").is_err());
        assert!(extract("fn f() {}", &["f".into()], "self").is_err());
        assert!(extract("fn f() {} mod child;", &["f".into()], "child").is_err());
    }
}

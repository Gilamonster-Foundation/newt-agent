use super::*;

/// #2735: leading unmatched delimiters and a trailing cut block must not
/// cause error recovery to absorb the first complete function into doc text.
#[test]
fn leading_closers_and_trailing_cut_preserve_every_complete_item() {
    let source = "    ))\n}\n\n/// a (b-c).\n/// d (#1): e-f\nfn first() {}\nfn second() {}\nfn cut() {\n    if pending {\n        work();\n";
    for first_line in [1, 100] {
        let entries = outline_rust(source, first_line).unwrap();
        assert_eq!(
            entries,
            vec![
                OutlineEntry {
                    start_line: first_line + 5,
                    end_line: first_line + 5,
                    header: "fn first() {}".into(),
                    open_ended: false
                },
                OutlineEntry {
                    start_line: first_line + 6,
                    end_line: first_line + 6,
                    header: "fn second() {}".into(),
                    open_ended: false
                },
                OutlineEntry {
                    start_line: first_line + 7,
                    end_line: first_line + 9,
                    header: "fn cut()".into(),
                    open_ended: true
                },
            ]
        );
        // Removing the damaged prefix changes neither item identity nor spans.
        assert_eq!(
            entries,
            outline_rust(&source[source.find("///").unwrap()..], first_line + 3).unwrap()
        );
    }
}

/// #2735: delimiter recovery must not turn comments, strings, or macro input
/// into definitions, or weaken the existing conservative trailing recovery.
#[test]
fn leading_closers_do_not_invent_items_from_opaque_tokens() {
    for tail in [
        "/* fn imaginary() {} */\n",
        "/*\nfn imaginary() {\n",
        "let text = r#\"\nfn imaginary() {\n",
        "let marker = '\"'; let text = r#\"\nfn imaginary() {\n",
        "my_macro! {\n    fn imaginary() {}\n    let other = 1;\n",
        "cfg_if::cfg_if! { if #[cfg(unix)] { fn imaginary() {}\n",
    ] {
        let source = format!("))\n}}\nfn real() {{}}\n{tail}");
        let entries = outline_rust(&source, 50).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.header == "fn real() {}" && e.start_line == 52 && !e.open_ended),
            "{entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|e| e.header.contains("imaginary") || e.open_ended),
            "{tail}: {entries:?}"
        );
    }
}

/// #2735: all closing delimiter kinds and CRLF/Unicode whitespace keep the
/// original line coordinates; a fragment consisting only of a tail has no items.
#[test]
fn closing_delimiter_variants_keep_coordinates() {
    let body = "/// a (b-c).\n/// d (#1): e-f\nfn first() {}\nfn cut() {\n    if ready {\n";
    for prefix in [
        "))\n}\n",
        "]]\r\n}\r\n",
        "}\n",
        "\u{2003}))\n}\n",
        "));\n}\n",
    ] {
        let source = format!("{prefix}{body}");
        let first_line = 20 + prefix.matches('\n').count();
        assert_eq!(
            outline_rust(&source, 20),
            outline_rust(body, first_line),
            "prefix {prefix:?}"
        );
    }
    assert!(outline_rust("  ))\n}\n  ", 1).unwrap().is_empty());
}

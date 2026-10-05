use super::*;

fn large_rust() -> String {
    format!(
        "fn first() {{\n{} }}\nfn target() {{\n    target_body();\n}}\n",
        "    // padding\n".repeat(2100)
    )
}

/// #2745: the second plain read must expose definitions beyond page one.
#[test]
#[cfg(feature = "ast")]
fn repeated_plain_read_maps_the_whole_file() {
    let source = large_rust();
    let mut history = ReadHistory::default();
    let first = history.read("large.rs", &source, None, None, None, false);
    assert!(first.starts_with("fn first()"));
    assert!(!first.contains("fn target()"));
    let map = history.read("large.rs", &source, None, None, None, false);
    assert!(map.contains("Whole-file outline"), "{map}");
    assert!(map.contains("1-2102") && map.contains("2103-2105"), "{map}");
    assert!(
        map.contains("offset=2103") && map.contains("fn target()"),
        "{map}"
    );
    assert!(map.contains("Pages already served: 1-2000"), "{map}");
}

/// #2745: explicit requests remain byte-for-byte the existing paging output.
#[test]
fn explicit_offset_and_small_file_are_unchanged() {
    let source = large_rust();
    let mut history = ReadHistory::default();
    history.read("large.rs", &source, None, None, None, false);
    let explicit = history.read("large.rs", &source, Some(2103), None, None, false);
    assert_eq!(
        explicit,
        super::super::output_budget::read_file_page(
            "large.rs",
            &source,
            Some(2103),
            None,
            None,
            false
        )
    );
    for _ in 0..3 {
        assert_eq!(
            history.read("small.rs", "fn small() {}\n", None, None, None, false),
            "fn small() {}\n"
        );
    }
}

/// #2745: unsupported languages advance using the existing page cursor.
#[test]
fn repeated_non_rust_read_advances() {
    let source = (1..=2105)
        .map(|n| format!("line {n}\n"))
        .collect::<String>();
    let mut history = ReadHistory::default();
    history.read("large.txt", &source, None, None, None, false);
    let second = history.read("large.txt", &source, None, None, None, false);
    assert!(second.starts_with("line 2001\n"), "{second}");
}

/// #2745: history is scoped to both content and turn, not just the locator.
#[test]
fn changed_content_and_new_turn_start_at_page_one() {
    let source = large_rust();
    let mut history = ReadHistory::default();
    let first = history.read("large.rs", &source, None, None, None, false);
    history.read("large.rs", &source, None, None, None, false);
    let changed = source.replace("fn first", "fn changed");
    assert!(history
        .read("large.rs", &changed, None, None, None, false)
        .starts_with("fn changed"));
    assert_eq!(
        ReadHistory::default().read("large.rs", &source, None, None, None, false),
        first
    );
}

/// #2745: continuation uses character positions, not byte or line guesses.
#[test]
fn non_rust_mid_line_continuation_does_not_lose_unicode() {
    let source = format!("{}{}", "α".repeat(15000), "β".repeat(15000));
    let mut history = ReadHistory::default();
    let first = history.read("long.txt", &source, None, None, None, false);
    let cursor = crate::prune::read_page_continuation(&first, "long.txt", 1, 0).unwrap();
    let second = history.read("long.txt", &source, None, None, None, false);
    assert_eq!(
        second,
        super::super::output_budget::read_file_page(
            "long.txt",
            &source,
            Some(cursor.0),
            None,
            Some(cursor.1),
            false
        )
    );
    assert!(second.starts_with('β'));
    assert!(history
        .read("long.txt", &source, None, None, None, false)
        .starts_with("All file pages"));
}

/// #2745: explicit reads count as served, but do not claim the gaps were read.
#[test]
#[cfg(feature = "ast")]
fn map_reports_explicit_pages_without_marking_gaps_served() {
    let source = large_rust();
    let mut history = ReadHistory::default();
    history.read("large.rs", &source, Some(2103), None, None, false);
    let first = history.read("large.rs", &source, None, None, None, false);
    assert!(
        first.starts_with("fn first()"),
        "first plain read is unchanged"
    );
    let map = history.read("large.rs", &source, None, None, None, false);
    assert!(
        map.contains("Pages already served: 1-2000, 2103-2105"),
        "{map}"
    );
}

/// #2745: an oversized index remains bounded and represents the file's tail.
#[test]
#[cfg(feature = "ast")]
fn large_outline_fits_the_page_and_spill_budgets() {
    let source = (0..2000)
        .map(|i| format!("fn item_{i:04}() {{\n    // {}\n}}\n", "body".repeat(20)))
        .collect::<String>();
    for offload in [false, true] {
        let mut history = ReadHistory::default();
        history.read("many.rs", &source, None, None, None, offload);
        let map = history.read("many.rs", &source, None, None, None, offload);
        let budget = super::super::output_budget::cap_estimator()
            .chars_for_tokens(super::super::output_budget::read_page_tokens(offload));
        assert!(map.len() <= budget, "{} > {budget}", map.len());
        assert!(map.contains("Sampled index: 2000 top-level items"));
        assert!(map.contains("fn item_0000()") && map.contains("fn item_1999()"));
        assert!(map.contains("offset=5998"));
    }
}

/// #2745: builds without AST still make progress through plain Rust reads.
#[test]
#[cfg(not(feature = "ast"))]
fn rust_without_ast_advances_instead_of_repeating() {
    let source = large_rust();
    let mut history = ReadHistory::default();
    history.read("large.rs", &source, None, None, None, false);
    let second = history.read("large.rs", &source, None, None, None, false);
    assert!(second.contains("fn target()"));
    assert!(!second.starts_with("fn first()"));
}

/// Real-resource proof grounding the pure history tests: the actual dispatch
/// shares history but still authorizes every call before exposing a map (#2745).
#[tokio::test]
#[cfg(feature = "ast")]
#[ignore = "real filesystem dispatch proof; run explicitly"]
async fn native_dispatch_maps_only_authorized_reads() {
    use crate::agentic::{
        tools::{execute_tool_with_collaborators, ToolCollaborators},
        NoMcp,
    };
    use crate::caveats::{Caveats, Scope};
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("large.rs"), large_rust()).unwrap();
    let mut history = ReadHistory::default();
    for (allowed, expected) in [
        (true, "fn first()"),
        (false, "denied"),
        (true, "Whole-file outline"),
    ] {
        let caveats = Caveats {
            fs_read: if allowed { Scope::All } else { Scope::none() },
            exec: Scope::none(),
            net: Scope::none(),
            fs_write: Scope::none(),
            ..Caveats::top()
        };
        let result = execute_tool_with_collaborators(
            "read_file",
            &serde_json::json!({"path": "large.rs"}),
            ws.path().to_str().unwrap(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            ToolCollaborators {
                read_history: Some(&mut history),
                ..Default::default()
            },
            false,
            crate::agentic::prompt_intake::PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.contains(expected), "{result}");
        if !allowed {
            assert!(!result.contains("Whole-file outline"));
        }
    }
}

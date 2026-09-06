use super::*;

// ── #1258: the embedded `find` size column (pure, fs-free) ──────────────

/// A `FindOpts` for the finalize/parse tests: defaults except the fields a
/// test overrides.
fn find_opts(max_results: usize, show_size: bool, sort: FindSort) -> FindOpts<'static> {
    FindOpts {
        name: None,
        type_filter: FindType::Any,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results,
        respect_gitignore: true,
        case_sensitive: true,
        show_size,
        show_lines: false,
        sort,
    }
}

#[test]
fn find_opts_parses_size_column_options() {
    let sized = serde_json::json!({ "show_size": true, "sort": "size" });
    let opts = find_opts_from_args(&sized);
    assert!(opts.show_size);
    assert_eq!(opts.sort, FindSort::Size);
    // Defaults: no size column, name order.
    let empty = serde_json::json!({});
    let d = find_opts_from_args(&empty);
    assert!(!d.show_size);
    assert_eq!(d.sort, FindSort::Name);
    // An unknown sort value falls back to name (never errors).
    let bogus = serde_json::json!({ "sort": "bogus" });
    let bad = find_opts_from_args(&bogus);
    assert_eq!(bad.sort, FindSort::Name);
}

#[test]
fn find_opts_parses_line_count_options() {
    // #1387: line count is a first-class evidence measure, parsed like size.
    let lined = serde_json::json!({ "show_lines": true, "sort": "lines" });
    let opts = find_opts_from_args(&lined);
    assert!(opts.show_lines);
    assert_eq!(opts.sort, FindSort::Lines);
    // Default: no line column.
    let empty = serde_json::json!({});
    assert!(!find_opts_from_args(&empty).show_lines);
}

#[test]
fn find_opts_parse_harness_source_category_and_language() {
    let source = serde_json::json!({ "category": "source", "language": "C++" });
    let opts = find_opts_from_args(&source);

    assert_eq!(opts.category, FindCategory::Source);
    assert_eq!(opts.language, Some("C++"));
    let empty = serde_json::json!({});
    let defaults = find_opts_from_args(&empty);
    assert_eq!(defaults.category, FindCategory::Any);
    assert_eq!(defaults.language, None);
}

#[test]
fn finalize_find_line_sort_is_lines_descending_with_show_lines() {
    // The metric column carries line counts in line mode; ordering is
    // descending with a path tie-break — the "files with the most lines"
    // answer, no `wc -l`.
    let entries = vec![
        (12, "short.rs".to_string()),
        (4247, "huge.rs".to_string()),
        (300, "mid.rs".to_string()),
    ];
    let opts = FindOpts {
        show_lines: true,
        sort: FindSort::Lines,
        ..find_opts(1000, false, FindSort::Lines)
    };
    let (lines, _) = finalize_find(entries, &opts);
    assert_eq!(
        lines,
        vec!["4247\thuge.rs", "300\tmid.rs", "12\tshort.rs"],
        "line count descending, each line prefixed '<lines>\\t<path>'"
    );
}

#[test]
fn count_newlines_matches_wc_l_semantics() {
    // Newlines are counted (a trailing line without a newline is not),
    // mirroring `wc -l` — verified purely over bytes, no filesystem.
    assert_eq!(count_newlines(b"a\nb\nc\n"), 3);
    assert_eq!(
        count_newlines(b"a\nb"),
        1,
        "trailing partial line uncounted"
    );
    assert_eq!(count_newlines(b""), 0);
    assert_eq!(count_newlines(b"no newline at all"), 0);
}

#[test]
fn finalize_find_name_sort_is_paths_ascending() {
    let entries = vec![
        (10, "src/b.rs".to_string()),
        (99, "src/a.rs".to_string()),
        (1, "src/c.rs".to_string()),
    ];
    let (lines, truncated) = finalize_find(entries, &find_opts(1000, false, FindSort::Name));
    assert_eq!(lines, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    assert!(!truncated, "under the cap");
}

#[test]
fn finalize_find_size_sort_is_bytes_descending_with_show_size() {
    let entries = vec![
        (10, "small.rs".to_string()),
        (900, "big.rs".to_string()),
        (50, "mid.rs".to_string()),
    ];
    let (lines, _) = finalize_find(entries, &find_opts(1000, true, FindSort::Size));
    assert_eq!(
        lines,
        vec!["900\tbig.rs", "50\tmid.rs", "10\tsmall.rs"],
        "byte size descending, each line prefixed '<size>\\t<path>'"
    );
}

#[test]
fn finalize_find_size_ties_break_by_path_for_determinism() {
    let entries = vec![(42, "z.rs".to_string()), (42, "a.rs".to_string())];
    let (lines, _) = finalize_find(entries, &find_opts(1000, false, FindSort::Size));
    assert_eq!(lines, vec!["a.rs", "z.rs"], "equal sizes → path ascending");
}

#[test]
fn finalize_find_size_sort_truncates_to_true_top_n() {
    // The N largest, not the first-N-walked: order THEN truncate.
    let entries = vec![
        (1, "a".to_string()),
        (100, "b".to_string()),
        (50, "c".to_string()),
        (200, "d".to_string()),
    ];
    let (lines, truncated) = finalize_find(entries, &find_opts(2, true, FindSort::Size));
    assert_eq!(lines, vec!["200\td", "100\tb"]);
    assert!(truncated, "two matches dropped past the cap");
}

#[test]
fn finalize_find_dedups_by_path() {
    let entries = vec![
        (10, "dup.rs".to_string()),
        (10, "dup.rs".to_string()),
        (20, "other.rs".to_string()),
    ];
    let (lines, _) = finalize_find(entries, &find_opts(1000, false, FindSort::Name));
    assert_eq!(lines, vec!["dup.rs", "other.rs"]);
}

#[test]
fn find_detail_bare_path_has_no_filters() {
    let opts = FindOpts {
        name: None,
        type_filter: FindType::Any,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results: 1000,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: false,
        show_lines: false,
        sort: FindSort::Name,
    };
    assert_eq!(find_detail(".", &opts), ".");
}

#[test]
fn find_detail_shows_only_non_default_filters() {
    let opts = FindOpts {
        name: Some("*.rs"),
        type_filter: FindType::Files,
        category: FindCategory::Any,
        language: None,
        max_depth: Some(2),
        max_results: 50,
        respect_gitignore: false,
        case_sensitive: false,
        show_size: false,
        show_lines: false,
        sort: FindSort::Name,
    };
    assert_eq!(
        find_detail("src", &opts),
        "src (name=*.rs, type=f, depth=2, max=50, no-gitignore, icase)"
    );
}

#[test]
fn find_detail_omits_each_default_independently() {
    let opts = FindOpts {
        name: None,
        type_filter: FindType::Dirs,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results: 1000,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: false,
        show_lines: false,
        sort: FindSort::Name,
    };
    assert_eq!(find_detail(".", &opts), ". (type=d)");
}

#[test]
fn find_detail_notes_the_size_column_and_size_sort() {
    let opts = FindOpts {
        name: Some("*.rs"),
        type_filter: FindType::Files,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results: 10,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: true,
        show_lines: false,
        sort: FindSort::Size,
    };
    assert_eq!(
        find_detail(".", &opts),
        ". (name=*.rs, type=f, max=10, sort=size, size)"
    );
}

#[test]
fn find_detail_notes_the_line_column_and_line_sort() {
    let opts = FindOpts {
        name: Some("*.rs"),
        type_filter: FindType::Files,
        category: FindCategory::Any,
        language: None,
        max_depth: None,
        max_results: 10,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: false,
        show_lines: true,
        sort: FindSort::Lines,
    };
    assert_eq!(
        find_detail(".", &opts),
        ". (name=*.rs, type=f, max=10, sort=lines, lines)"
    );
}

#[test]
fn find_detail_notes_the_source_category_filter() {
    // #1406: the `code:true` boolean was replaced by the language-pack
    // `category=source` filter; find_detail now surfaces that instead.
    let opts = FindOpts {
        name: None,
        type_filter: FindType::Files,
        max_depth: None,
        max_results: 10,
        respect_gitignore: true,
        case_sensitive: true,
        show_size: false,
        show_lines: true,
        category: FindCategory::Source,
        language: None,
        sort: FindSort::Lines,
    };
    assert_eq!(
        find_detail(".", &opts),
        ". (type=f, category=source, max=10, sort=lines, lines)"
    );
}

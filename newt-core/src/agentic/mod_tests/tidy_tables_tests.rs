#[cfg(not(feature = "markdown-table-formatter"))]
#[test]
fn identity_without_the_feature() {
    // The default build (and the wyvern strip without the opt-in) leaves the
    // source untouched.
    let ragged = "| a | bb |\n|---|---|\n| ccc | d |";
    assert_eq!(super::tidy_markdown_tables(ragged), ragged);
}

#[cfg(feature = "markdown-table-formatter")]
#[test]
fn aligns_pipes_with_the_feature() {
    let ragged = "| a | bb |\n| --- | --- |\n| ccc | d |\n";
    let tidy = super::tidy_markdown_tables(ragged);
    assert_ne!(tidy, ragged, "the table should be reformatted");
    assert!(tidy.contains("ccc"), "content preserved");
    // Every pipe-bearing row lines its pipes up at the same columns.
    let pipe_cols = |s: &str| {
        s.char_indices()
            .filter(|(_, c)| *c == '|')
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
    };
    let rows: Vec<&str> = tidy.lines().filter(|l| l.contains('|')).collect();
    let first = pipe_cols(rows[0]);
    for r in &rows {
        assert_eq!(pipe_cols(r), first, "pipes aligned across rows: {r:?}");
    }
}

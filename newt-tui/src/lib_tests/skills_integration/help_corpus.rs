use super::*;

#[serial_test::serial(real_fs)]
#[test]
fn help_documents_the_delete_alias_and_that_it_asks() {
    // #2009 PR6b: the ops moved to `/resume`. `rm` survives as an alias — it
    // is muscle memory — and the corpus now also promises the confirmation,
    // which is the part an operator most needs to know before typing it.
    let delete_row = help_lines()
        .iter()
        .find(|line| line.contains("/resume delete <id>"))
        .expect("the delete row is advertised");
    assert!(
        delete_row.contains("rm"),
        "the alias survives: {delete_row}"
    );
    assert!(
        delete_row.contains("asks first"),
        "the corpus promises the confirmation: {delete_row}"
    );
}

#[test]
fn help_lists_the_resume_command() {
    assert!(help_lines().iter().any(|l| l.contains("/resume")));
}

#[test]
fn help_lists_the_roadmap_and_tree_commands() {
    assert!(help_lines().iter().any(|l| l.contains("/roadmap")));
    // #2009 PR12: `/tree` retired into `/roadmap tree`, which is a real
    // subcommand now — the fold kept the word rather than costing it.
    assert!(help_lines().iter().any(|l| l.contains("/roadmap tree")));
}

#[test]
fn help_documents_the_search_that_does_not_reopen() {
    // #2009 PR6: `/recall` retired into `/resume find`. What the corpus must
    // still teach is the CAPABILITY — searching conversations without
    // reopening one — not the name it used to have.
    assert!(help_lines()
        .iter()
        .any(|line| line.contains("/resume find")));
    assert!(
        !help_lines().iter().any(|line| line.contains("/recall")),
        "a retired verb must not still be advertised as a top-level command"
    );
}

#[test]
fn help_documents_compress_command() {
    assert!(help_lines()
        .iter()
        .any(|line| line.contains("/compress [focus]")));
}

/// #1736: `/name <title>` is the ergonomic alias for `/rename <title>` — same
/// path, same semantics. Both verbs must be discoverable in /help, alongside
/// the basic conversation grammar (`/start`, `/resume`).
#[test]
fn help_lists_name_alias_and_resume_grammar() {
    let lines = help_lines();
    assert!(lines.iter().any(|l| l.contains("/start")), "missing /start");
    assert!(
        lines.iter().any(|l| l.contains("/resume")),
        "missing /resume"
    );
    assert!(
        lines.iter().any(|l| l.contains("/rename")),
        "missing /rename"
    );
    assert!(
        lines.iter().any(|l| l.contains("/name")),
        "missing /name alias"
    );
}

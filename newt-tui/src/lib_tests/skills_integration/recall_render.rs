use super::*;

/// Route a `/recall` (or `/resume find`) line the way `chat.rs` routes it.
///
/// `handle_recall_command` was deleted with its parser in #2009 PR6; this
/// mirrors the arm that replaced it, so the tests below keep exercising the
/// parse and not just the renderer. The `panic!` arm is the point: if the
/// line stops parsing as a find, these fail loudly instead of quietly
/// testing something else.
fn recall_via_resume(input: &str, store: &newt_core::ConversationStore) -> anyhow::Result<String> {
    match parse_resume_command(input) {
        ResumeCommand::Find(query) if query.is_empty() => recall_browse_message(store),
        ResumeCommand::Find(query) => recall_search_message(store, &query),
        other => panic!("`{input}` no longer parses as a find: {other:?}"),
    }
}

#[test]
fn recall_garbage_only_query_renders_friendly_hint() {
    let (_state, _ws, store) = recall_test_store();
    // "AND" sanitizes to nothing (bare operator) — must come back as a
    // friendly Ok message, never through the `error:` path.
    let msg = recall_via_resume("/recall AND", &store).unwrap();
    assert!(msg.contains("Nothing searchable"), "got: {msg}");
    assert!(msg.contains("Try plain keywords"), "got: {msg}");
}

#[test]
fn recall_browse_orders_by_activity_tick_with_short_ids() {
    let (_state, _ws, store) = recall_test_store();
    let alpha = store.create("Alpha task", None).unwrap();
    store
        .append_turn(&alpha, "alpha question", "alpha answer")
        .unwrap();
    let beta = store.create("Beta task", None).unwrap();
    store
        .append_turn(&beta, "beta question", "beta answer")
        .unwrap();
    // Reactivate alpha: a new turn gives it the highest activity tick.
    store
        .append_turn(&alpha, "alpha follow-up", "alpha again")
        .unwrap();

    let msg = recall_via_resume("/recall", &store).unwrap();
    assert!(msg.starts_with("Recent conversations (most recent first):"));
    let alpha_pos = msg.find("Alpha task").unwrap();
    let beta_pos = msg.find("Beta task").unwrap();
    assert!(alpha_pos < beta_pos, "most recently active first:\n{msg}");
    // Ids render as 12-char prefixes, never in full.
    assert!(msg.contains(short_conversation_id(&alpha)));
    assert!(!msg.contains(&alpha));
    assert!(!msg.contains(&beta));
    // Turn counts + the last-activity display claim (§6: a claim, hence ~).
    assert!(msg.contains("(2 turns, last active ~"), "got: {msg}");
    assert!(msg.contains("(1 turns, last active ~"), "got: {msg}");
    assert!(msg.ends_with("Restore with /conversation restore <id>."));
}

#[test]
fn recall_browse_empty_store_message() {
    let (_state, _ws, store) = recall_test_store();
    assert_eq!(
        recall_browse_message(&store).unwrap(),
        "No saved conversations for this workspace."
    );
}

#[test]
fn recall_browse_truncates_to_limit_with_overflow_line() {
    let (_state, _ws, store) = recall_test_store();
    for i in 0..(RECALL_LIMIT + 2) {
        store.create(&format!("conv-{i:02}"), None).unwrap();
    }
    let msg = recall_browse_message(&store).unwrap();
    // The two least-recently-created fall off the end of the browse view.
    assert!(!msg.contains("conv-00"), "got: {msg}");
    assert!(!msg.contains("conv-01"), "got: {msg}");
    assert!(msg.contains("conv-02"));
    assert!(msg.contains(&format!("conv-{:02}", RECALL_LIMIT + 1)));
    assert!(msg.contains("… 2 more — /conversation list shows all."));
}

#[test]
fn recall_search_renders_snippets_and_footer() {
    let (_state, _ws, store) = recall_test_store();
    let id = store.create("Login bug", None).unwrap();
    store
        .append_turn(
            &id,
            "the login form crashes on submit",
            "fixed the crash in the submit handler",
        )
        .unwrap();
    let other = store.create("Docs chore", None).unwrap();
    store
        .append_turn(&other, "write the readme", "done")
        .unwrap();

    let msg = recall_via_resume("/recall login", &store).unwrap();
    assert!(msg.starts_with("Recall matches for `login`:"), "got: {msg}");
    assert!(msg.contains(short_conversation_id(&id)));
    assert!(!msg.contains(&id), "full ids must not render:\n{msg}");
    assert!(msg.contains("Login bug"));
    assert!(msg.contains("  ·  seq "), "got: {msg}");
    // The FTS5 `>>>`/`<<<` match markers render as `«`/`»` highlights.
    assert!(msg.contains("«login»"), "got: {msg}");
    assert!(msg.contains("form crashes on submit"), "got: {msg}");
    assert!(!msg.contains("Docs chore"), "non-hit leaked:\n{msg}");
    assert!(msg.ends_with("Restore with /conversation restore <id>."));
}

#[test]
fn recall_search_no_matches_message() {
    let (_state, _ws, store) = recall_test_store();
    let id = store.create("Something", None).unwrap();
    store
        .append_turn(&id, "unrelated work", "still unrelated")
        .unwrap();
    assert_eq!(
        recall_search_message(&store, "zebra").unwrap(),
        "No matches for `zebra` in this workspace's conversations."
    );
}

#[test]
fn wal_fallback_startup_notice_surfaces_only_when_present() {
    // N7 (#261 review): the seam the run loop feeds the store's notice
    // through. Present → a visible warning naming the fallback + cause.
    let msg = wal_fallback_startup_notice(Some("locking protocol")).unwrap();
    assert!(msg.contains("journal_mode=DELETE"), "got: {msg}");
    assert!(msg.contains("locking protocol"), "got: {msg}");
    // Absent → silence.
    assert_eq!(wal_fallback_startup_notice(None), None);
    // A healthy local store reports no fallback end-to-end.
    let (_state, _ws, store) = recall_test_store();
    assert_eq!(
        wal_fallback_startup_notice(store.wal_fallback_notice()),
        None
    );
}

#[test]
fn recall_title_falls_back_to_first_user_turn_at_render() {
    let (_state, _ws, store) = recall_test_store();
    // An empty stored title (can't happen via the TUI create path —
    // `conversation_title_from_task` never returns empty — but a record
    // written elsewhere can carry one).
    let id = store.create("", None).unwrap();
    let task = "alpha ".repeat(20);
    store.append_turn(&id, &task, "reply").unwrap();
    let title = recall_display_title(&store, &id, "");
    assert_eq!(title.chars().count(), 60);
    assert!(task.starts_with(&title));
    // Empty title and no turns at all → "(untitled)".
    let bare = store.create("  ", None).unwrap();
    assert_eq!(recall_display_title(&store, &bare, "  "), "(untitled)");
    // And the browse view actually uses the fallback.
    let msg = recall_browse_message(&store).unwrap();
    assert!(msg.contains("(untitled)"), "got: {msg}");
    assert!(msg.contains(title.trim_end()), "got: {msg}");
    // A present title is used verbatim — no record load needed.
    assert_eq!(
        recall_display_title(&store, "no-such-id", " Kept title "),
        "Kept title"
    );
}

#[test]
fn recall_claim_timestamp_formats_and_clamps() {
    assert_eq!(claim_timestamp(0), "1970-01-01 00:00 UTC");
    // 2026-06-11 00:00:00 UTC in nanos.
    assert_eq!(
        claim_timestamp(1_781_136_000 * 1_000_000_000),
        "2026-06-11 00:00 UTC"
    );
    assert_eq!(claim_timestamp(u128::MAX), "unknown");
}

#[test]
fn recall_readable_snippet_flattens_and_marks() {
    assert_eq!(
        readable_snippet("…the >>>tokio<<< runtime\n  panicked…"),
        "…the «tokio» runtime panicked…"
    );
}

#[test]
fn recall_short_id_is_a_restorable_prefix() {
    let id = newt_core::new_conversation_id();
    let short = short_conversation_id(&id);
    assert_eq!(short.len(), 12);
    assert!(id.starts_with(short));
    // Shorter-than-prefix ids pass through whole.
    assert_eq!(short_conversation_id("abc"), "abc");
}

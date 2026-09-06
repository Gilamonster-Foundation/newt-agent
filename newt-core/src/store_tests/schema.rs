use super::*;

use super::fts::events_extract_sql;

/// #1671: the footer's per-turn title read — present after create/rename,
/// `None` for a not-yet-persisted id, and workspace-fenced like `exists`.
#[test]
fn title_reads_current_name_and_is_workspace_fenced() {
    let root = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

    let id = store_a.create("mesh docking", None).unwrap();
    assert_eq!(store_a.title(&id).unwrap().as_deref(), Some("mesh docking"));

    // A rename is reflected on the next read.
    store_a.rename(&id, "docking ceremony").unwrap();
    assert_eq!(
        store_a.title(&id).unwrap().as_deref(),
        Some("docking ceremony")
    );

    // A fresh session's id has no row yet — None, not an error.
    assert_eq!(store_a.title("no-such-conversation").unwrap(), None);

    // Workspace fence: another workspace cannot read this title.
    assert_eq!(store_b.title(&id).unwrap(), None);
}

/// #1668: the posture pin round-trips through the conversation row,
/// defaults to the empty pin (`'{}'`), and is workspace-fenced like the
/// other row metadata.
#[test]
fn preference_pin_round_trips_defaults_empty_and_is_workspace_fenced() {
    let root = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

    let id = store_a.create("pinned work", None).unwrap();
    // A fresh row carries the '{}' default — the EMPTY pin, not None and
    // not an error (resume treats it as a no-op).
    let fresh = store_a.preference_pin(&id).unwrap().expect("row exists");
    assert!(fresh.is_empty(), "fresh row must read as nothing pinned");

    let pin = crate::OperatorPreferencePin {
        backend: Some("sol".into()),
        model: Some("gpt-5.6-sol".into()),
        cognition: Some("off".into()),
        tenacity: Some(crate::Tenacity::Relentless),
    };
    store_a.update_preference_pin(&id, &pin).unwrap();
    assert_eq!(store_a.preference_pin(&id).unwrap(), Some(pin.clone()));

    // A not-yet-persisted id has no row — None, not an error.
    assert_eq!(
        store_a.preference_pin("no-such-conversation").unwrap(),
        None
    );

    // Workspace fence: another workspace can neither read nor write it.
    assert_eq!(store_b.preference_pin(&id).unwrap(), None);
    assert!(store_b
        .update_preference_pin(&id, &crate::OperatorPreferencePin::default())
        .is_err());
    assert_eq!(store_a.preference_pin(&id).unwrap(), Some(pin));
}

/// #1668: posture writes are metadata — they must not tick the §6
/// activity clock, so pinning posture can never perturb MRU ordering
/// (same contract as `rename` / `update_scratchpad`).
#[test]
fn update_preference_pin_does_not_tick_activity() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let older = store.create("older", None).unwrap();
    store.append_turn(&older, "q", "a").unwrap();
    let newer = store.create("newer", None).unwrap();
    store.append_turn(&newer, "q", "a").unwrap();

    let tick_of = |id: &str| -> i64 {
        let conn = store.lock_conn();
        conn.query_row(
            "SELECT activity_tick FROM conversations WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    };
    let before = tick_of(&older);
    store
        .update_preference_pin(
            &older,
            &crate::OperatorPreferencePin {
                tenacity: Some(crate::Tenacity::Relaxed),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(tick_of(&older), before, "posture must not bump the tick");
    assert_eq!(store.latest_open().unwrap().unwrap().id, newer);
}

/// #1668: a database written by an older newt (no `posture` column) gains
/// the column on open via the additive schema reconciliation, with the
/// empty backfill — old conversations read as "nothing pinned".
#[test]
fn older_database_gains_the_preference_pin_column_on_open() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let id;
    {
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        id = store.create("pre-1668 conversation", None).unwrap();
        let conn = store.lock_conn();
        // Simulate the pre-#1668 schema by dropping the column outright.
        conn.execute_batch("ALTER TABLE conversations DROP COLUMN preference_pin")
            .unwrap();
    }
    let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let pin = reopened.preference_pin(&id).unwrap().expect("row survives");
    assert!(pin.is_empty(), "backfill must read as nothing pinned");
}

/// #1668: strict decode — a corrupted `preference_pin` column is an error,
/// never a silently-garbled pin (same discipline as the scratchpad/plan
/// columns; resume callers degrade the error to a fail-open notice).
#[test]
fn corrupt_preference_pin_column_refuses_to_load_garbage() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("garbled", None).unwrap();
    {
        let conn = store.lock_conn();
        conn.execute(
            "UPDATE conversations SET preference_pin = 'not json' WHERE id = ?1",
            [&id],
        )
        .unwrap();
    }
    let err = store.preference_pin(&id).unwrap_err().to_string();
    assert!(err.contains("refusing to load garbage"), "{err}");
}

/// #1668 authority boundary: a `preference_pin` column tampered with
/// authority-shaped keys — credentials, endpoints, caveat clamps, sandbox
/// or permission state — is REFUSED, so the persistence layer cannot
/// smuggle authority into a session even when the row is hostile. The
/// resume path degrades the refusal to a notice and runs on the invocation
/// baseline (`a_corrupt_pin_falls_open_to_the_invocation_baseline` in
/// newt-tui grounds that half).
#[test]
fn a_tampered_preference_pin_column_cannot_carry_authority_state() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("tampered", None).unwrap();
    for hostile in [
        r#"{"backend":"sol","api_key":"sk-evil"}"#,
        r#"{"backend":"sol","endpoint":"http://evil.example:9"}"#,
        r#"{"caveats":{"fs":"unrestricted"}}"#,
        r#"{"sandbox":"off","permissions":["all"]}"#,
        r#"{"ocap":["fs:/"],"cognition":"off"}"#,
    ] {
        store.set_raw_preference_pin_for_test(&id, hostile).unwrap();
        let err = store
            .preference_pin(&id)
            .expect_err(&format!("must refuse: {hostile}"))
            .to_string();
        assert!(err.contains("refusing to load garbage"), "{err}");
    }
    // And a WELL-FORMED pin still round-trips — the refusal is about the
    // smuggled keys, not about pins in general.
    let honest = crate::OperatorPreferencePin {
        backend: Some("sol".into()),
        ..Default::default()
    };
    store.update_preference_pin(&id, &honest).unwrap();
    assert_eq!(store.preference_pin(&id).unwrap(), Some(honest));
}

#[test]
fn prompt_receipts_do_not_backfill_historical_turns() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conv = store.create("historical", None).unwrap();
    store
        .append_turn(&conv, "old user text", "old answer")
        .unwrap();

    // Opening the prompt-capable store adds only the empty table. Existing
    // completed turns are not silently reinterpreted as receipts because
    // their ingress/raw representation is unknowable after the fact.
    let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    assert!(reopened.prompt_chain(&conv).unwrap().is_empty());
    assert!(reopened.latest_prompt(&conv).unwrap().is_none());
}

#[test]
fn opening_v1_prompt_schema_adds_authority_column_without_backfill_guessing() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let conv = "v1-authority-migration";
    let (a_id, retry_id) = {
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let a = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator("A", "A"),
            )
            .unwrap();
        let b = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator_continuation("B", "B", a.submitted().id()),
            )
            .unwrap();
        let retry = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::harness_retry("retry", "retry", b.submitted().id()),
            )
            .unwrap();

        // Rewrite every row exactly as the v1 writer did, then remove the
        // v2-only column. Reconciliation must add it back as NULL rather
        // than fabricating authority that was never part of the v1 hash.
        for receipt in store.prompt_chain(conv).unwrap() {
            let legacy = receipt.into_legacy_v1_for_test();
            let conn = store.lock_conn();
            conn.execute(
                "UPDATE prompt_receipts
                        SET active_operator_id = NULL, receipt_hash = ?2,
                            encoding_version = 1
                      WHERE id = ?1",
                rusqlite::params![legacy.id().to_string(), legacy.receipt_hash()],
            )
            .unwrap();
        }
        {
            let conn = store.lock_conn();
            conn.execute_batch("ALTER TABLE prompt_receipts DROP COLUMN active_operator_id")
                .unwrap();
        }
        let _ = &b; // continuation node in the walk; authority is a
        (a.submitted().id(), retry.submitted().id())
    };

    let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let context = reopened
        .turn_prompt_context(conv, retry_id)
        .unwrap()
        .unwrap();
    assert_eq!(context.active().id(), a_id);
    assert_eq!(context.submitted().receipt().active_operator_id(), None);
    let columns: Vec<String> = {
        let conn = reopened.lock_conn();
        let mut stmt = conn.prepare("PRAGMA table_info(prompt_receipts)").unwrap();
        let selected = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        selected
    };
    assert!(columns.iter().any(|column| column == "active_operator_id"));
}

#[test]
fn wal_fallback_classifier_matches_known_nfs_failures() {
    assert!(wal_fallback_eligible("locking protocol"));
    assert!(wal_fallback_eligible("disk I/O error"));
    assert!(wal_fallback_eligible(
        "sqlite failure: `Error code 15: Locking Protocol`"
    ));
    assert!(!wal_fallback_eligible("no such table: turns"));
    assert!(!wal_fallback_eligible("database is locked"));
    assert!(!wal_fallback_eligible(""));
}

#[test]
fn wal_mode_pairs_with_synchronous_normal_on_the_stores_connection() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    // `synchronous` is per-connection, so ask the store's own connection
    // (a fresh external connection would only show its own default).
    let conn = store.lock_conn();
    let sync_level: i64 = conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sync_level, 1, "WAL must run at synchronous=NORMAL (1)");
}

// --- end_reason: /end · /restart · :wq close-out (17.7 wiring) ---------

/// `end_conversation` marks the row so `latest_open` skips it on
/// auto-resume, while `list` (and therefore `/recall`/`/conversation`)
/// still sees it — ended, not deleted.
#[test]
fn end_conversation_hides_row_from_latest_open_but_not_from_list() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let c1 = store.create("first", None).unwrap();
    store.append_turn(&c1, "q1", "a1").unwrap();
    let c2 = store.create("second", None).unwrap();
    store.append_turn(&c2, "q2", "a2").unwrap();

    // c2 was written last → highest activity tick → the resume target.
    assert_eq!(store.latest_open().unwrap().unwrap().id, c2);

    // End c2: latest_open falls back to the prior OPEN conversation…
    store.end_conversation(&c2, "wq").unwrap();
    assert_eq!(
        store.latest_open().unwrap().unwrap().id,
        c1,
        "an ended conversation is skipped on auto-resume"
    );
    // …but both rows are still listed (ended ≠ deleted).
    assert_eq!(store.list().unwrap().len(), 2);
    // …and the ended conversation is still recall-searchable.
    assert!(
        store
            .search("q2", 5)
            .unwrap()
            .iter()
            .any(|h| h.conversation_id == c2),
        "ended conversation stays in the FTS index for /recall"
    );

    // End the last open one too → nothing left to auto-resume → fresh.
    store.end_conversation(&c1, "end").unwrap();
    assert!(store.latest_open().unwrap().is_none());
    assert_eq!(store.list().unwrap().len(), 2, "still listed after ending");
}

/// `list_all` spans every workspace (the fenced `list` does not) and pairs
/// each conversation with the `workspace_path` a follower re-opens the store
/// at — the exact mechanism a cross-workspace attach surface needs.
#[test]
fn list_all_spans_workspaces_and_carries_their_paths() {
    let root = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let canon = |d: &std::path::Path| {
        std::fs::canonicalize(d)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };

    // One store root (one db); two different workspaces.
    let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let a = store_a.create("in A", None).unwrap();
    store_a.append_turn(&a, "q", "a").unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();
    let b = store_b.create("in B", None).unwrap();
    store_b.append_turn(&b, "q", "a").unwrap();

    // The fenced list only sees its own workspace.
    assert_eq!(store_a.list().unwrap().len(), 1);
    assert_eq!(store_b.list().unwrap().len(), 1);

    // list_all (from EITHER handle) sees both, each with its real path.
    let all = store_a.list_all().unwrap();
    assert_eq!(all.len(), 2, "both workspaces' conversations");
    let path_of = |id: &str| {
        all.iter()
            .find(|(s, _)| s.id == id)
            .map(|(_, p)| p.clone())
            .unwrap()
    };
    assert_eq!(path_of(&a), canon(ws_a.path()));
    assert_eq!(path_of(&b), canon(ws_b.path()));

    // The returned path is exactly what lets a follower load a conversation
    // from ANOTHER workspace: re-open at B's path, load B's conversation.
    let follower = ConversationStore::new(root.path(), path_of(&b), 100).unwrap();
    assert_eq!(follower.load(&b).unwrap().title, "in B");
}

#[test]
fn end_conversation_does_not_tick_activity_and_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let older = store.create("older", None).unwrap();
    store.append_turn(&older, "q", "a").unwrap();
    let newer = store.create("newer", None).unwrap();
    store.append_turn(&newer, "q", "a").unwrap();

    let tick_of = |id: &str| -> i64 {
        let conn = store.lock_conn();
        conn.query_row(
            "SELECT activity_tick FROM conversations WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    };
    let before = tick_of(&older);
    store.end_conversation(&older, "new").unwrap();
    assert_eq!(tick_of(&older), before, "ending must not bump the tick");
    // Idempotent: re-ending an already-ended conversation is fine.
    store.end_conversation(&older, "new").unwrap();
    // `newer` is still open and remains the resume target.
    assert_eq!(store.latest_open().unwrap().unwrap().id, newer);
}

// --- 17.3: the query-sanitizer adversarial matrix ---------------------

/// Shorthand: sanitize and unwrap (the input is expected to survive).
fn s(raw: &str) -> String {
    sanitize_fts5_query(raw).unwrap()
}

/// The hermes examples: dotted / hyphenated / path-like / colon tokens
/// are auto-quoted so FTS5 reads them as text, not syntax.
#[test]
fn sanitizer_auto_quotes_dotted_hyphenated_and_path_tokens() {
    assert_eq!(s("chat-send"), "\"chat-send\"");
    assert_eq!(s("P2.2"), "\"P2.2\"");
    assert_eq!(s("my-app.config.ts"), "\"my-app.config.ts\"");
    assert_eq!(s("src/store.rs"), "\"src/store.rs\"");
    assert_eq!(s("tcp:p4d.p4d-ascii:1666"), "\"tcp:p4d.p4d-ascii:1666\"");
    assert_eq!(s("issue #246"), "issue \"#246\"");
    // Clean barewords pass through untouched — including underscores
    // (in FTS5's bareword alphabet) and non-ASCII text.
    assert_eq!(s("hello world"), "hello world");
    assert_eq!(s("writer_clock"), "writer_clock");
    assert_eq!(s("schlüssel wörter"), "schlüssel wörter");
}

#[test]
fn sanitizer_preserves_balanced_phrases_and_drops_dangling_quotes() {
    assert_eq!(s("\"exact phrase\" extra"), "\"exact phrase\" extra");
    assert_eq!(s("say \"hello world\" now"), "say \"hello world\" now");
    // Unbalanced quote: the quote dies, its text survives as terms.
    assert_eq!(s("foo \"bar"), "foo bar");
    assert_eq!(s("\"unclosed"), "unclosed");
    assert_eq!(s("\"a b\" \"c"), "\"a b\" c");
    // Phrase content keeps operators/metachars as text (FTS5 allows
    // anything but a quote inside a phrase).
    assert_eq!(s("\"AND OR\""), "\"AND OR\"");
    assert_eq!(s("\"P2.2 chat-send\""), "\"P2.2 chat-send\"");
    // Empty / unindexable phrases are dropped, not emitted as "".
    let err = sanitize_fts5_query("\"\"").unwrap_err().to_string();
    assert!(err.contains("reduced to nothing"), "{err}");
    let err = sanitize_fts5_query("\"--\"").unwrap_err().to_string();
    assert!(err.contains("reduced to nothing"), "{err}");
}

#[test]
fn sanitizer_trims_dangling_operators() {
    assert_eq!(s("foo AND"), "foo");
    assert_eq!(s("OR foo"), "foo");
    assert_eq!(s("NOT foo"), "foo");
    assert_eq!(s("foo AND AND bar"), "foo AND bar");
    assert_eq!(s("foo AND OR bar"), "foo AND bar");
    assert_eq!(s("AND foo OR"), "foo");
    // Valid binary positions survive.
    assert_eq!(s("foo OR bar"), "foo OR bar");
    assert_eq!(s("foo NOT bar"), "foo NOT bar");
    assert_eq!(s("a OR b OR c"), "a OR b OR c");
    // Lowercase forms are ordinary terms, not operators.
    assert_eq!(s("foo and bar"), "foo and bar");
    // Bare AND reduces to nothing → error, not an FTS5 syntax error.
    let err = sanitize_fts5_query("AND").unwrap_err().to_string();
    assert!(err.contains("reduced to nothing"), "{err}");
    // NEAR is reserved by FTS5 — it survives only as a quoted term.
    assert_eq!(s("NEAR"), "\"NEAR\"");
    assert_eq!(s("near"), "near");
}

#[test]
fn sanitizer_strips_metacharacter_injection() {
    assert_eq!(s("(foo OR bar) AND baz"), "foo OR bar AND baz");
    assert_eq!(s("foo* ^bar"), "foo bar");
    assert_eq!(s("col*umn"), "column");
    // A lone quote / star / caret / paren reduces to nothing.
    for q in ["\"", "*", "^", "( )", "*^()"] {
        let err = sanitize_fts5_query(q).unwrap_err().to_string();
        assert!(err.contains("reduced to nothing"), "{q:?}: {err}");
    }
    // Mid-token quote: unbalanced → stripped; the halves survive.
    assert_eq!(s("fo\"o bar"), "fo o bar");
    // Punctuation-only tokens are dropped, indexable ones kept.
    assert_eq!(s("?? foo !!"), "foo");
    assert_eq!(s("foo \u{a0} "), "foo"); // unicode whitespace handled
}

#[test]
fn sanitizer_handles_mixed_phrases_terms_and_operators() {
    assert_eq!(
        s("\"tuning writeback\" OR coverage-floor"),
        "\"tuning writeback\" OR \"coverage-floor\""
    );
    assert_eq!(
        s("error \"chain violation\" NOT P2.2"),
        "error \"chain violation\" NOT \"P2.2\""
    );
    // Operator directly before a phrase works too.
    assert_eq!(s("AND \"lead phrase\" tail"), "\"lead phrase\" tail");
}

#[test]
fn sanitizer_errors_on_empty_and_whitespace_queries() {
    for q in ["", "   ", "\t\n"] {
        let err = sanitize_fts5_query(q).unwrap_err().to_string();
        assert!(err.contains("reduced to nothing"), "{q:?}: {err}");
    }
}

/// The events-extraction SQL is shared between the triggers and the
/// content view; pin its shape (json_valid guard + coalesce to '').
#[test]
fn events_extract_sql_guards_and_targets_the_seam_keys() {
    let sql = events_extract_sql("new.events", "tool");
    assert!(sql.contains("json_valid(new.events)"));
    assert!(sql.contains("json_each(new.events)"));
    assert!(sql.contains("'$.tool'"));
    assert!(sql.contains("ELSE '' END"));
}

use super::*;

// =========================================================================
// Part 5 — new in 17.3: the FTS5 recall index (trigger maintenance,
// backfill-on-migration, workspace fencing, ranking/snippet shape, the
// events seam for 17.6, and adversarial queries end to end).
// =========================================================================

/// Trigger maintenance, both directions: an appended turn is immediately
/// searchable (AFTER INSERT), and deleting the conversation removes its
/// hits (AFTER DELETE via the FK cascade).
#[test]
fn fts_appends_are_searchable_and_conversation_delete_removes_them() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("indexing test", None).unwrap();
    store
        .append_turn(
            &id,
            "please fix the tokenizer bug",
            "tokenizer fixed and tests added",
        )
        .unwrap();

    let hits = store.search("tokenizer", 10).unwrap();
    assert_eq!(hits.len(), 1, "one matching turn → one hit");
    assert_eq!(hits[0].conversation_id, id);
    assert_eq!(hits[0].title, "indexing test");
    assert!(hits[0].seq > 0, "seq is the turn's §6 tick");
    assert!(
        hits[0].snippet.contains(">>>tokenizer<<<"),
        "snippet must mark the match: {}",
        hits[0].snippet
    );
    assert!(
        hits[0].rank < 0.0,
        "bm25 scores are negative: {}",
        hits[0].rank
    );

    // Matches in the user half are found too.
    assert_eq!(store.search("please", 10).unwrap().len(), 1);

    store.delete(&id).unwrap();
    assert!(
        store.search("tokenizer", 10).unwrap().is_empty(),
        "the conversation-delete cascade must clear the index"
    );
}

/// The one-time legacy JSON import writes turns through the normal insert
/// path — the trigger indexes imported history with no extra pass.
#[test]
fn fts_indexes_legacy_imported_turns() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let rec = legacy_record(
        "9000-imported",
        "old times",
        ws.path(),
        &[("remember the quokka incident", "documented it")],
        1,
        2,
    );
    write_legacy_record(root.path(), &rec);

    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let hits = store.search("quokka", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].conversation_id, rec.id);
}

/// Workspace fencing: a hit in workspace A is never returned to workspace B,
/// even though both share one database and one FTS index.
#[test]
fn fts_search_is_workspace_fenced() {
    let root = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

    let id_a = store_a.create("a's secret", None).unwrap();
    store_a
        .append_turn(&id_a, "the zanzibar rollout plan", "drafted")
        .unwrap();
    let id_b = store_b.create("b's own", None).unwrap();
    store_b
        .append_turn(&id_b, "unrelated work", "done")
        .unwrap();

    let a_hits = store_a.search("zanzibar", 10).unwrap();
    assert_eq!(a_hits.len(), 1);
    assert_eq!(a_hits[0].conversation_id, id_a);
    assert!(
        store_b.search("zanzibar", 10).unwrap().is_empty(),
        "workspace B must never see A's turns"
    );
}

/// Backfill-on-migration: a database written by a pre-17.3 newt (no FTS
/// objects at all) opens, gains the index + triggers, and its existing
/// turns — including events-derived columns — become searchable. A second
/// open is a no-op (presence of the table = done): same single hit, no
/// duplicates.
#[test]
fn fts_backfills_pre_fts_databases_once() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let workspace_path = std::fs::canonicalize(ws.path()).unwrap();
    // A pre-17.3 db carries pre-17.2 (UUIDv5) keys in the general case;
    // the open-time migration re-keys them before any search can run.
    #[allow(deprecated)]
    let old_key = ConversationStore::workspace_id_for_path(ws.path()).unwrap();

    // Hand-build the 17.1-shaped database: current tables, NO fts objects.
    std::fs::create_dir_all(root.path()).unwrap();
    {
        let conn = rusqlite::Connection::open(db_path(root.path())).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (
                 id TEXT PRIMARY KEY, title TEXT NOT NULL,
                 workspace_path TEXT NOT NULL, workspace_key TEXT NOT NULL,
                 persona TEXT, end_reason TEXT,
                 writer_fingerprint TEXT NOT NULL, activity_tick INTEGER NOT NULL,
                 tip_hash TEXT NOT NULL,
                 started_at_claim INTEGER NOT NULL, updated_at_claim INTEGER NOT NULL
             );
             CREATE TABLE turns (
                 conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                 writer_fingerprint TEXT NOT NULL, seq INTEGER NOT NULL,
                 prev_hash TEXT NOT NULL, user TEXT NOT NULL, assistant TEXT NOT NULL,
                 events TEXT NOT NULL DEFAULT '[]',
                 tokens_in INTEGER, tokens_out INTEGER,
                 ts_claim INTEGER NOT NULL,
                 encoding_version INTEGER NOT NULL DEFAULT 1,
                 PRIMARY KEY (conversation_id, writer_fingerprint, seq)
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations
               (id, title, workspace_path, workspace_key, persona, end_reason,
                writer_fingerprint, activity_tick, tip_hash, started_at_claim, updated_at_claim)
             VALUES ('pre-fts-conv', 'from before recall', ?1, ?2, NULL, NULL,
                     'old-writer', 2, '', 1, 1)",
            rusqlite::params![workspace_path.to_string_lossy(), old_key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO turns VALUES
               ('pre-fts-conv', 'old-writer', 1, '', 'the wombat deployment failed',
                'rolled it back', '[]', NULL, NULL, 1, 1),
               ('pre-fts-conv', 'old-writer', 2, '', 'retry it', 'done',
                '[{\"tool\":\"chat-send\",\"args_digest\":\"channel=#ops\"}]', NULL, NULL, 1, 1)",
            [],
        )
        .unwrap();
    }

    // First 17.3 open: index created + backfilled in one transaction.
    {
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let hits = store.search("wombat", 10).unwrap();
        assert_eq!(hits.len(), 1, "pre-FTS turns must be searchable");
        assert_eq!(hits[0].conversation_id, "pre-fts-conv");
        assert_eq!(hits[0].title, "from before recall");
        // Backfill derives the events columns too — the 17.6 seam applies
        // to history, not just new appends.
        assert_eq!(store.search("chat-send", 10).unwrap().len(), 1);
    }

    // Second open: idempotent — still exactly one hit each, no duplicates.
    let again = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert_eq!(again.search("wombat", 10).unwrap().len(), 1);
    assert_eq!(again.search("chat-send", 10).unwrap().len(), 1);
    // And the live write path is wired in the migrated db.
    again
        .append_turn("pre-fts-conv", "also index the axolotl", "indexed")
        .unwrap();
    assert_eq!(again.search("axolotl", 10).unwrap().len(), 1);
}

/// Ranking sanity: bm25 puts the turn where the term is exact-and-dense
/// above one where it is buried in noise; hits arrive best-first; `limit`
/// truncates from the bottom. A quoted phrase matches only the turn with
/// the exact adjacent words, not scattered mentions.
#[test]
fn fts_ranking_prefers_exact_dense_matches_over_scattered() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let dense = store.create("dense", None).unwrap();
    store
        .append_turn(&dense, "kraken kraken status", "the kraken is released")
        .unwrap();
    let scattered = store.create("scattered", None).unwrap();
    store
        .append_turn(
            &scattered,
            "long unrelated discussion about build pipelines caching tokens \
             models budgets and somewhere in the middle a kraken appears once \
             before more words about pipelines caching and budgets trail off",
            "noted",
        )
        .unwrap();

    let hits = store.search("kraken", 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].conversation_id, dense,
        "dense match must rank first"
    );
    assert!(
        hits[0].rank <= hits[1].rank,
        "hits must arrive best-first: {} vs {}",
        hits[0].rank,
        hits[1].rank
    );
    // limit truncates from the bottom of the ranking.
    let top = store.search("kraken", 1).unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].conversation_id, dense);

    // Phrase query: only the exact adjacent words match.
    let phrase = store.search("\"kraken is released\"", 10).unwrap();
    assert_eq!(phrase.len(), 1);
    assert_eq!(phrase[0].conversation_id, dense);
}

/// Snippet shape: a match deep inside a long turn comes back as a short
/// excerpt — match marked `>>> <<<`, `…` at the trimmed edges — never the
/// full turn content.
#[test]
fn fts_snippet_is_a_marked_excerpt_not_the_full_turn() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do \
                  eiusmod tempor incididunt ut labore et dolore magna aliqua "
        .repeat(4);
    let long_text = format!("{filler} the platypus hides here {filler}");
    let id = store.create("haystack", None).unwrap();
    store.append_turn(&id, "question", &long_text).unwrap();

    let hits = store.search("platypus", 10).unwrap();
    assert_eq!(hits.len(), 1);
    let snippet = &hits[0].snippet;
    assert!(snippet.contains(">>>platypus<<<"), "{snippet}");
    assert!(
        snippet.contains('…'),
        "trimmed edges must show ellipses: {snippet}"
    );
    assert!(
        snippet.len() < long_text.len() / 4,
        "snippet must be an excerpt ({} chars of {})",
        snippet.len(),
        long_text.len()
    );
}

/// The 17.6 seam, proven end to end: a turn whose `events` JSON carries
/// tool entries (hand-inserted — nothing records events until 17.6) gets
/// its tool names and args digests indexed by the trigger, searchable
/// through the same API, with the snippet drawn from the derived column.
#[test]
fn fts_events_derived_columns_light_up_when_events_arrive() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("tool runs", None).unwrap();
    store.append_turn(&id, "seed turn", "ok").unwrap();

    // Hand-insert a turn carrying events, exactly as 17.6 will write them.
    let writer = store.writer_fingerprint().to_string();
    raw(root.path())
        .execute(
            "INSERT INTO turns
               (conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                events, tokens_in, tokens_out, ts_claim, encoding_version)
             VALUES (?1, ?2, 9999, 'x', 'run the deploy', 'deployed',
                     '[{\"tool\":\"chat-send\",\"args_digest\":\"target=ops channel=#general\"},
                       {\"tool\":\"file-read\",\"args_digest\":\"path=src/store.rs\"}]',
                     NULL, NULL, 1, 1)",
            rusqlite::params![id, writer],
        )
        .unwrap();

    // Tool names are searchable (auto-quoting carries `chat-send` through).
    let hits = store.search("chat-send", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].conversation_id, id);
    assert!(
        hits[0].snippet.contains(">>>chat-send<<<"),
        "{}",
        hits[0].snippet
    );
    assert_eq!(store.search("file-read", 10).unwrap().len(), 1);

    // Args digests are searchable too — including path-shaped tokens.
    assert_eq!(store.search("channel", 10).unwrap().len(), 1);
    assert_eq!(store.search("src/store.rs", 10).unwrap().len(), 1);

    // Malformed events must never break appends or search: the extraction
    // is json_valid-guarded and yields empty derived columns.
    raw(root.path())
        .execute(
            "INSERT INTO turns
               (conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                events, tokens_in, tokens_out, ts_claim, encoding_version)
             VALUES (?1, ?2, 10000, 'x', 'capybara checkpoint', 'ok',
                     'not json at all', NULL, NULL, 1, 1)",
            rusqlite::params![id, writer],
        )
        .unwrap();
    assert_eq!(store.search("capybara", 10).unwrap().len(), 1);
}

/// Re-creating an existing id (the JSON-parity REPLACE path) cascades the
/// old turns away — their index entries must go with them, or a later
/// turn reusing the rowid would inherit ghost terms.
#[test]
fn fts_recreating_a_conversation_resets_its_index() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = new_conversation_id();
    store.create_with_id(&id, "first life", None).unwrap();
    store
        .append_turn(&id, "the narwhal detail", "noted")
        .unwrap();
    assert_eq!(store.search("narwhal", 10).unwrap().len(), 1);

    store.create_with_id(&id, "second life", None).unwrap();
    assert!(
        store.search("narwhal", 10).unwrap().is_empty(),
        "REPLACE must clear the old turns' index entries"
    );
    store.append_turn(&id, "fresh start", "ok").unwrap();
    assert_eq!(store.search("fresh", 10).unwrap().len(), 1);
}

/// Adversarial queries end to end: everything the sanitizer matrix throws
/// must either search cleanly or fail with the sanitizer's own "reduced to
/// nothing" — an FTS5 syntax error reaching the user is a bug.
#[test]
fn fts_adversarial_queries_never_surface_syntax_errors() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("target", None).unwrap();
    store
        .append_turn(&id, "run chat-send for P2.2", "sent via src/lib.rs")
        .unwrap();
    store
        .append_turn(
            &id,
            "what about '; DROP TABLE turns; -- style attacks",
            "they are plain text to the index",
        )
        .unwrap();

    let nasties = [
        "\"",
        "*",
        "(",
        "^",
        "((((",
        "\"unbalanced",
        "AND",
        "NOT",
        "OR OR",
        "foo AND",
        "NEAR",
        "NEAR(a b, 2)",
        "col:filter",
        "user:secret",
        "-exclude",
        "+plus",
        "a.b/c:d-e",
        "chat-send P2.2 src/lib.rs",
        "\"phrase\" AND ( ) ^",
        "→ ☃",
        "'; DROP TABLE turns; --",
        "",
    ];
    for q in nasties {
        match store.search(q, 10) {
            Ok(_) => {}
            Err(e) => {
                let text = e.to_string();
                assert!(
                    text.contains("reduced to nothing"),
                    "{q:?} must sanitize or reduce, not error with: {text}"
                );
            }
        }
    }

    // And the sanitized forms actually FIND things.
    assert_eq!(store.search("chat-send", 10).unwrap().len(), 1);
    assert_eq!(store.search("P2.2", 10).unwrap().len(), 1);
    assert_eq!(store.search("src/lib.rs", 10).unwrap().len(), 1);
    // SQL injection text is just terms; the turns table survived.
    assert_eq!(store.search("\"DROP TABLE\"", 10).unwrap().len(), 1);
}

/// Perf probe (not a gate): build a 1k-turn corpus and time searches.
/// Run with: cargo test -p newt-core --test store -- --ignored fts_search_latency
#[test]
#[ignore = "perf probe — run with --ignored to measure recall latency"]
fn fts_search_latency_on_a_1k_turn_corpus() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 0).unwrap();

    let vocab = [
        "parser",
        "tokenizer",
        "budget",
        "probe",
        "kraken",
        "deploy",
        "rollback",
        "coverage",
        "ratchet",
        "snippet",
        "mesh",
        "caveat",
        "lattice",
        "chain",
    ];
    for c in 0..100 {
        let id = store.create(&format!("conv {c}"), None).unwrap();
        for t in 0..10 {
            let mut user = String::new();
            for w in 0..12 {
                user.push_str(vocab[(c + t * 3 + w) % vocab.len()]);
                user.push(' ');
            }
            let assistant = format!("turn {t} of conversation {c}: {user}");
            store.append_turn(&id, &user, &assistant).unwrap();
        }
    }

    let queries = [
        "kraken",
        "tokenizer budget",
        "\"parser tokenizer\"",
        "chat-send",
        "ratchet OR mesh",
    ];
    let started = std::time::Instant::now();
    let mut total_hits = 0usize;
    const ROUNDS: usize = 20;
    for _ in 0..ROUNDS {
        for q in queries {
            total_hits += store.search(q, 10).unwrap().len();
        }
    }
    let elapsed = started.elapsed();
    let per_query = elapsed / (ROUNDS * queries.len()) as u32;
    println!(
        "1k-turn corpus: {} queries in {elapsed:?} → {per_query:?}/query ({total_hits} hits)",
        ROUNDS * queries.len()
    );
    assert!(
        per_query < std::time::Duration::from_millis(50),
        "recall must stay interactive on a 1k-turn corpus: {per_query:?}"
    );
}

/// A corrupt identity.pem must not block the store: it falls back to the
/// per-install nonce — the same fingerprint the install had before.
#[test]
fn corrupt_identity_pem_falls_back_to_install_nonce() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let nonce_fp = {
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        store.writer_fingerprint().to_string()
    };
    std::fs::write(root.path().join("identity.pem"), "not a pem at all").unwrap();
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert_eq!(
        store.writer_fingerprint(),
        nonce_fp,
        "unparseable key file must fall back to the stable nonce identity"
    );
}

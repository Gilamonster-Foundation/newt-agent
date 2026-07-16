use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

///
/// This is THE 17.6 seam: events elements are objects, and the keys read
/// here — `tool` and `args_digest` — are the contract 17.6's recorder must
/// write. Shared verbatim by the view and both triggers so the indexed
/// terms and the content read back at query time can never disagree.
/// `json_valid` guards the whole expression: a garbage events blob yields
/// `''` instead of breaking the append (a trigger error would abort the
/// turn's transaction).
pub(super) fn events_extract_sql(source: &str, key: &str) -> String {
    format!(
        "CASE WHEN json_valid({source}) THEN \
            (SELECT coalesce(group_concat(json_extract(value, '$.{key}'), ' '), '') \
               FROM json_each({source})) \
         ELSE '' END"
    )
}

/// Create the 17.3 FTS5 recall index (module docs — FTS5 recall index):
/// the `turns_fts_content` view, the external-content `turns_fts` virtual
/// table (unicode61), and the AFTER INSERT / AFTER DELETE triggers on
/// `turns`. No UPDATE trigger by design: turns are append-only (§6).
///
/// Backfill-on-migration: when the virtual table does not exist yet (a
/// fresh db, or a 17.1/17.2 db opened by a 17.3+ newt), every existing
/// turn is indexed by an explicit `INSERT…SELECT` of the same derived
/// expressions (see the in-body comment for why not FTS5 `'rebuild'`) —
/// one-time, inside the same `BEGIN IMMEDIATE` transaction as the DDL,
/// idempotent because the presence of the table IS the done-marker
/// (checked under the write lock, so concurrent first opens cannot
/// double-backfill).
pub(super) fn create_fts_index(conn: &Connection) -> anyhow::Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let have_index = tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'turns_fts'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();

    let view_tools = events_extract_sql("events", "tool");
    let view_digests = events_extract_sql("events", "args_digest");
    let new_tools = events_extract_sql("new.events", "tool");
    let new_digests = events_extract_sql("new.events", "args_digest");
    let old_tools = events_extract_sql("old.events", "tool");
    let old_digests = events_extract_sql("old.events", "args_digest");
    tx.execute_batch(&format!(
        "CREATE VIEW IF NOT EXISTS turns_fts_content AS
            SELECT rowid,
                   user,
                   assistant,
                   {view_tools} AS tool_names,
                   {view_digests} AS tool_args_digest
              FROM turns;
         CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
             user, assistant, tool_names, tool_args_digest,
             content='turns_fts_content',
             content_rowid='rowid',
             tokenize='unicode61'
         );
         CREATE TRIGGER IF NOT EXISTS turns_fts_after_insert
         AFTER INSERT ON turns BEGIN
             INSERT INTO turns_fts(rowid, user, assistant, tool_names, tool_args_digest)
             VALUES (new.rowid, new.user, new.assistant, {new_tools}, {new_digests});
         END;
         -- Fires per cascaded row on conversation delete. The 'delete'
         -- command must receive the values that were indexed at insert
         -- time — guaranteed by the append-only invariant on turns.
         CREATE TRIGGER IF NOT EXISTS turns_fts_after_delete
         AFTER DELETE ON turns BEGIN
             INSERT INTO turns_fts(turns_fts, rowid, user, assistant, tool_names, tool_args_digest)
             VALUES ('delete', old.rowid, old.user, old.assistant, {old_tools}, {old_digests});
         END;"
    ))?;

    if !have_index {
        // One-time backfill of pre-17.3 turns. NOT the FTS5 `'rebuild'`
        // command: rebuild scans the content table through a
        // schema-qualified statement, and `json_each` — an eponymous
        // virtual table inside the content view — cannot be resolved
        // schema-qualified ("no such table: main.json_each", verified
        // against the bundled SQLite 3.45). An explicit INSERT…SELECT of
        // the same derived expressions is equivalent for an empty index
        // and prepares unqualified, so the view's seam stays intact.
        tx.execute(
            &format!(
                "INSERT INTO turns_fts(rowid, user, assistant, tool_names, tool_args_digest)
                 SELECT rowid, user, assistant, {view_tools}, {view_digests} FROM turns"
            ),
            [],
        )?;
        tracing::info!("created the FTS5 recall index and backfilled existing turns (17.3)");
    }
    tx.commit()?;
    Ok(())
}

/// A parsed piece of a raw recall query: a ready-to-emit term (bare word or
/// `"quoted phrase"`) or a boolean operator awaiting placement.
enum QueryPart {
    Term(String),
    Op(&'static str),
}

/// Sanitize a raw user/model query into a safe FTS5 `MATCH` expression
/// (17.3 — the hermes `_sanitize_fts5_query` port; see
/// `docs/design/evidence/hermes-study/report-hermes-sessions.md` §6).
///
/// Pure function, no database required. Rules:
///
/// 1. **Balanced `"phrases"` are preserved** as phrase queries. A dangling
///    unbalanced quote is dropped and its text processed as plain terms.
/// 2. Outside phrases, the pure-syntax FTS5 metacharacters `( ) * ^ "` are
///    stripped wherever they appear in a token.
/// 3. Bare uppercase `AND` / `OR` / `NOT` survive as boolean operators
///    only in positions FTS5's grammar accepts (between terms): leading
///    and trailing operators are trimmed and operator runs collapse to
///    their first (`NOT foo` → `foo`, `foo AND` → `foo`,
///    `a AND OR b` → `a AND b`). Lowercase forms are ordinary terms.
///    Bare uppercase `NEAR` is quoted into a term — FTS5 reserves it.
/// 4. Tokens still carrying any other ASCII punctuation are **auto-quoted**
///    so FTS5 reads them as text, not syntax: `chat-send` → `"chat-send"`,
///    `P2.2` → `"P2.2"`, `src/store.rs` → `"src/store.rs"`,
///    `tcp:1666` → `"tcp:1666"` (this also neutralizes `col:` filters and
///    `-`/`.` operator injection).
/// 5. Tokens and phrases with nothing the unicode61 tokenizer would index
///    (no letter or digit in any script) are dropped.
///
/// When everything sanitizes away, this is an **error** ("query reduced to
/// nothing") — never an empty `MATCH` (a syntax error) and never a
/// match-all.
pub fn sanitize_fts5_query(raw: &str) -> anyhow::Result<String> {
    let mut parts: Vec<QueryPart> = Vec::new();

    // Pass 1: split out balanced "phrases"; everything else is plain text.
    let mut rest = raw;
    loop {
        let Some(open) = rest.find('"') else {
            push_plain_tokens(rest, &mut parts);
            break;
        };
        push_plain_tokens(&rest[..open], &mut parts);
        let after_open = &rest[open + 1..];
        match after_open.find('"') {
            Some(close) => {
                let phrase = after_open[..close].trim();
                // An unindexable phrase ("--", "", …) would be dead weight
                // or an FTS5 error; drop it like an unindexable token.
                if phrase.chars().any(char::is_alphanumeric) {
                    parts.push(QueryPart::Term(format!("\"{phrase}\"")));
                }
                rest = &after_open[close + 1..];
            }
            None => {
                // Unbalanced: strip the dangling quote, keep its text.
                push_plain_tokens(after_open, &mut parts);
                break;
            }
        }
    }

    // Pass 2: place operators. An operator is emitted only between two
    // terms: leading ops are dropped (no left operand), runs collapse to
    // the first, and a trailing pending op is never flushed.
    let mut out: Vec<String> = Vec::new();
    let mut pending_op: Option<&'static str> = None;
    for part in parts {
        match part {
            QueryPart::Term(term) => {
                if let Some(op) = pending_op.take() {
                    out.push(op.to_string());
                }
                out.push(term);
            }
            QueryPart::Op(op) => {
                if !out.is_empty() && pending_op.is_none() {
                    pending_op = Some(op);
                }
            }
        }
    }

    if out.is_empty() {
        anyhow::bail!("search query reduced to nothing after FTS5 sanitizing: {raw:?}");
    }
    Ok(out.join(" "))
}

/// Tokenize a plain (non-phrase) text run on whitespace and classify each
/// token: uppercase boolean keywords become [`QueryPart::Op`]; everything
/// else goes through [`sanitize_bare_token`].
fn push_plain_tokens(text: &str, parts: &mut Vec<QueryPart>) {
    for token in text.split_whitespace() {
        match token {
            "AND" => parts.push(QueryPart::Op("AND")),
            "OR" => parts.push(QueryPart::Op("OR")),
            "NOT" => parts.push(QueryPart::Op("NOT")),
            // FTS5 reserves NEAR (case-sensitively); as a quoted phrase it
            // is just the word again.
            "NEAR" => parts.push(QueryPart::Term("\"NEAR\"".to_string())),
            _ => {
                if let Some(term) = sanitize_bare_token(token) {
                    parts.push(QueryPart::Term(term));
                }
            }
        }
    }
}

/// Sanitize one bare token: strip pure-syntax metacharacters, drop tokens
/// with nothing indexable, and auto-quote anything that is not a clean
/// FTS5 bareword (rules 2/4/5 of [`sanitize_fts5_query`]).
fn sanitize_bare_token(token: &str) -> Option<String> {
    let stripped: String = token
        .chars()
        .filter(|c| !matches!(c, '(' | ')' | '*' | '^' | '"'))
        .collect();
    // Nothing the unicode61 tokenizer would index → drop the token.
    if !stripped.chars().any(char::is_alphanumeric) {
        return None;
    }
    // FTS5's bareword alphabet: ASCII alphanumerics, `_`, and everything
    // non-ASCII. Any other character would parse as syntax — auto-quote
    // the token so `chat-send`, `P2.2`, paths, and issue refs match as
    // text (the hermes rule this port exists for).
    let is_bareword = stripped
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii());
    Some(if is_bareword {
        stripped
    } else {
        format!("\"{stripped}\"")
    })
}

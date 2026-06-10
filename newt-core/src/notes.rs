//! `NoteStore` v2 — persistent, agent-curated notes (Step 19.1, #248).
//!
//! Notes live in `~/.newt/NOTES.md` as free-text **entries** separated by a
//! `§` delimiter line (hermes-agent's `ENTRY_DELIMITER` pattern). Entries may
//! span multiple lines. The design goals, in order:
//!
//! - **The cap is the curator.** A hard character budget whose violation
//!   returns the *full current entry list* plus "Replace or remove existing
//!   entries first" turns the writer (human today, the model in 19.3) into
//!   its own compactor — no separate curation job.
//! - **Substring addressing.** `replace`/`remove` take a short unique
//!   substring; zero matches is a clear error, multiple matches list the
//!   candidates and ask the caller to be more specific.
//! - **Frozen snapshot.** Notes are read once at session start and frozen
//!   into the system prompt (preserves the model's prefix/KV cache).
//!   Mid-session writes hit disk immediately but take effect next session.
//! - **Crash-safe writes.** Write-then-rename (the `ConversationStore::
//!   save_record` idiom) so a crash mid-write can never leave a half-written
//!   NOTES.md, plus a best-effort sidecar lock for concurrent newts.
//!
//! ## On-disk format
//!
//! v2 files terminate every entry with a line containing only `§`:
//!
//! ```text
//! first entry — may
//! span lines
//! §
//! second entry
//! §
//! ```
//!
//! A NOTES.md with no `§` line is read as **legacy** line-per-entry format
//! and is rewritten in the v2 format transparently on the first write.
//! Because the delimiter terminates (rather than separates) entries, any v2
//! file — even a single-entry one — contains `§`, so format detection never
//! misreads a multi-line v2 entry as several legacy entries.
//!
//! ## Locking is advisory
//!
//! `.NOTES.md.lock` is created with `create_new` before each write and
//! removed after the rename. It serializes *writers* across concurrent newt
//! processes on a best-effort basis (locks older than [`LOCK_STALE`] are
//! treated as leftovers from a crashed process and taken over). It does NOT
//! provide read-modify-write isolation: two newts that both loaded NOTES.md
//! at session start can still overwrite each other's additions (last writer
//! wins). Readers need no lock — the rename is atomic, so a reader never
//! observes a partial file.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::memory::{MemMessage, MemoryProvider, SessionContext};
use crate::metrics::TurnMetrics;

/// The entry delimiter: a line containing only this string separates entries.
pub const ENTRY_DELIMITER: &str = "§";

/// Delimiter as it appears between entries in serialized/rendered form.
const DELIM_LINE: &str = "\n§\n";

/// A lock file older than this is assumed to be a leftover from a crashed
/// process and is taken over.
const LOCK_STALE: Duration = Duration::from_secs(3);

/// How many times to retry lock acquisition before giving up.
const LOCK_RETRIES: u32 = 20;

/// Delay between lock acquisition retries.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Persistent agent notes at `~/.newt/NOTES.md`.
///
/// Notes are read once at session start and **frozen** into the system prompt
/// so the model's prefix cache stays valid. Mid-session writes (via
/// `/remember <fact>`) update the file but NOT the system prompt block —
/// changes take effect next session.
///
/// Modelled on hermes-agent's `MemoryStore` (MEMORY.md pattern).
pub struct NoteStore {
    path: PathBuf,
    /// Rendered entries captured at `initialize` — frozen for the system prompt.
    snapshot: String,
    /// Live entries (may differ from the snapshot mid-session).
    entries: Vec<String>,
    char_limit: usize,
    /// Advisory-lock tuning — overridable so tests don't sleep for seconds.
    lock_stale: Duration,
    lock_retries: u32,
    lock_retry_delay: Duration,
}

impl NoteStore {
    pub const DEFAULT_CHAR_LIMIT: usize = 2_200;

    pub fn new(path: impl Into<PathBuf>, char_limit: usize) -> Self {
        Self {
            path: path.into(),
            snapshot: String::new(),
            entries: Vec::new(),
            char_limit: char_limit.max(10),
            lock_stale: LOCK_STALE,
            lock_retries: LOCK_RETRIES,
            lock_retry_delay: LOCK_RETRY_DELAY,
        }
    }

    /// Create at the default location `~/.newt/NOTES.md`.
    pub fn default_path() -> Self {
        let path = crate::Config::user_config_path()
            .map(|p| p.with_file_name("NOTES.md"))
            .unwrap_or_else(|| PathBuf::from("NOTES.md"));
        Self::new(path, Self::DEFAULT_CHAR_LIMIT)
    }

    // -- format ------------------------------------------------------------

    /// Parse file content into entries.
    ///
    /// Content containing a `§` delimiter line is v2; anything else is the
    /// legacy line-per-entry format (migrated to v2 on the first write).
    fn parse(content: &str) -> Vec<String> {
        let is_v2 = content.lines().any(|l| l.trim() == ENTRY_DELIMITER);
        if !is_v2 {
            return content
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();
        }
        let mut entries = Vec::new();
        let mut current: Vec<&str> = Vec::new();
        for line in content.lines() {
            if line.trim() == ENTRY_DELIMITER {
                let entry = current.join("\n").trim().to_string();
                if !entry.is_empty() {
                    entries.push(entry);
                }
                current.clear();
            } else {
                current.push(line);
            }
        }
        // Tolerate a missing final delimiter (hand-edited files).
        let last = current.join("\n").trim().to_string();
        if !last.is_empty() {
            entries.push(last);
        }
        entries
    }

    /// Serialize entries for disk: every entry is *terminated* by a `§` line,
    /// so even a single-entry file contains the delimiter and roundtrips as
    /// v2 (a multi-line entry is never misread as legacy lines).
    fn serialize(entries: &[String]) -> String {
        let mut out = String::new();
        for e in entries {
            out.push_str(e);
            out.push_str(DELIM_LINE);
        }
        out
    }

    /// Entries joined by the delimiter — what the prompt block shows and
    /// what the char budget measures.
    fn render(entries: &[String]) -> String {
        entries.join(DELIM_LINE)
    }

    fn rendered(&self) -> String {
        Self::render(&self.entries)
    }

    /// Reject text that would corrupt the on-disk format.
    fn validate_no_delimiter(text: &str) -> anyhow::Result<()> {
        if text.lines().any(|l| l.trim() == ENTRY_DELIMITER) {
            anyhow::bail!(
                "note text may not contain a line consisting only of \"{ENTRY_DELIMITER}\" \
                 — that is the entry delimiter"
            );
        }
        Ok(())
    }

    // -- introspection -----------------------------------------------------

    /// Current live entries, in order.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn char_usage(&self) -> (usize, usize) {
        (self.rendered().len(), self.char_limit)
    }

    fn numbered_listing(&self) -> String {
        if self.entries.is_empty() {
            return "  (no entries)".to_string();
        }
        self.entries
            .iter()
            .enumerate()
            .map(|(i, e)| format!("  {}. {}", i + 1, e))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The curator error: an over-budget write fails with the FULL current
    /// entry list so the writer can immediately decide what to replace or
    /// remove (hermes's error-path-as-curator).
    fn over_budget_error(&self, candidate_len: usize) -> anyhow::Error {
        let (used, limit) = self.char_usage();
        let pct = used * 100 / limit;
        anyhow::anyhow!(
            "NOTES.md is full: this write needs {candidate_len}/{limit} chars \
             (currently {used}/{limit}, {pct}% used). \
             Replace or remove existing entries first.\nCurrent entries:\n{}",
            self.numbered_listing()
        )
    }

    /// Find the index of the single entry containing `substr`.
    /// Zero matches and multiple matches are both errors.
    fn find_one(&self, substr: &str) -> anyhow::Result<usize> {
        let needle = substr.trim();
        if needle.is_empty() {
            anyhow::bail!("empty substring — quote a unique part of the entry you mean");
        }
        let matches: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.contains(needle))
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [one] => Ok(*one),
            [] => anyhow::bail!("no entry contains \"{needle}\""),
            many => {
                let listing = many
                    .iter()
                    .map(|&i| format!("  {}. {}", i + 1, self.entries[i]))
                    .collect::<Vec<_>>()
                    .join("\n");
                anyhow::bail!(
                    "{} entries match \"{needle}\". Be more specific:\n{listing}",
                    many.len()
                )
            }
        }
    }

    // -- mutation ----------------------------------------------------------

    /// Add an entry. Over-budget adds fail with the full current entry list
    /// (see [`Self::over_budget_error`]). Exact duplicates are a no-op.
    pub fn add(&mut self, fact: &str) -> anyhow::Result<()> {
        let fact = fact.trim();
        if fact.is_empty() {
            return Ok(());
        }
        Self::validate_no_delimiter(fact)?;
        if self.entries.iter().any(|e| e == fact) {
            return Ok(()); // already present — no-op
        }
        let mut candidate = self.entries.clone();
        candidate.push(fact.to_string());
        let new_len = Self::render(&candidate).len();
        if new_len > self.char_limit {
            return Err(self.over_budget_error(new_len));
        }
        // Persist first, commit to memory only on success — a failed save
        // (e.g. lock contention) must not leave a phantom in-memory entry
        // that the next successful write would silently persist.
        self.save(&candidate)?;
        self.entries = candidate;
        Ok(())
    }

    /// Replace the single entry containing `old_substr` with `new_text`.
    /// Exactly one entry must match; the result must fit the char budget.
    pub fn replace(&mut self, old_substr: &str, new_text: &str) -> anyhow::Result<()> {
        let new_text = new_text.trim();
        if new_text.is_empty() {
            anyhow::bail!("replacement text is empty — use remove to delete an entry");
        }
        Self::validate_no_delimiter(new_text)?;
        let idx = self.find_one(old_substr)?;
        let mut candidate = self.entries.clone();
        candidate[idx] = new_text.to_string();
        let new_len = Self::render(&candidate).len();
        if new_len > self.char_limit {
            return Err(self.over_budget_error(new_len));
        }
        self.save(&candidate)?;
        self.entries = candidate;
        Ok(())
    }

    /// Remove the single entry containing `substr`.
    /// Exactly one entry must match.
    pub fn remove(&mut self, substr: &str) -> anyhow::Result<()> {
        let idx = self.find_one(substr)?;
        let mut candidate = self.entries.clone();
        candidate.remove(idx);
        self.save(&candidate)?;
        self.entries = candidate;
        Ok(())
    }

    // -- persistence -------------------------------------------------------

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "NOTES.md".to_string())
    }

    fn lock_path(&self) -> PathBuf {
        self.path
            .with_file_name(format!(".{}.lock", self.file_name()))
    }

    /// Best-effort advisory lock via `create_new` on a sidecar file.
    /// See the module docs for what this does and does not guarantee.
    fn acquire_lock(&self) -> anyhow::Result<LockGuard> {
        let lock = self.lock_path();
        for _ in 0..self.lock_retries {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(_) => return Ok(LockGuard { path: lock }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Stale-lock takeover: a crashed newt leaves its lock
                    // behind; anything older than `lock_stale` is fair game.
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > self.lock_stale);
                    if stale {
                        let _ = std::fs::remove_file(&lock);
                        continue; // retry create_new immediately
                    }
                    std::thread::sleep(self.lock_retry_delay);
                }
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!(
            "could not acquire {} — another newt appears to be writing NOTES.md; try again",
            lock.display()
        )
    }

    /// Atomic save: write-then-rename (copied from
    /// `ConversationStore::save_record`) under the advisory lock.
    ///
    /// The temp file lives in the same directory so the rename never crosses
    /// a filesystem boundary (`std::fs::rename` replaces the destination on
    /// both Unix and Windows). A crash mid-write leaves only a stray
    /// `NOTES.md.tmp`, which the next save overwrites; reads only ever see
    /// the real file.
    ///
    /// Takes the entries to persist rather than reading `self.entries` so
    /// mutators can write first and commit to memory only on success.
    fn save(&self, entries: &[String]) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let _lock = self.acquire_lock()?;
        let tmp = self
            .path
            .with_file_name(format!("{}.tmp", self.file_name()));
        std::fs::write(&tmp, Self::serialize(entries))?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
        // _lock dropped here — lock file removed.
    }
}

/// Removes the advisory lock file on drop (best-effort).
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[async_trait]
impl MemoryProvider for NoteStore {
    fn name(&self) -> &str {
        "note_store"
    }

    async fn initialize(&mut self, _ctx: &SessionContext) -> anyhow::Result<()> {
        if self.path.exists() {
            let content = std::fs::read_to_string(&self.path).unwrap_or_default();
            self.entries = Self::parse(&content);
        }
        // Freeze the snapshot — this is what goes into the system prompt.
        self.snapshot = self.rendered();
        Ok(())
    }

    fn system_prompt_block(&self) -> Option<String> {
        if self.snapshot.trim().is_empty() {
            return None;
        }
        let used = self.snapshot.len();
        let pct = used * 100 / self.char_limit;
        Some(format!(
            "## Agent Notes ({}/{}, {}%)\n{}",
            used,
            self.char_limit,
            pct,
            self.snapshot.trim()
        ))
    }

    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        // NoteStore is a system-prompt-only provider — it doesn't manage history.
        Vec::new()
    }

    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}

    fn usage(&self) -> Option<(String, usize, usize)> {
        Some(("notes".into(), self.rendered().len(), self.char_limit))
    }

    fn add_note(&mut self, fact: &str) -> anyhow::Result<()> {
        self.add(fact)
    }
}

impl std::fmt::Debug for NoteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoteStore")
            .field("path", &self.path)
            .field("entries", &self.entries.len())
            .field("char_limit", &self.char_limit)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn ctx() -> SessionContext {
        SessionContext {
            workspace: "/ws".into(),
            session_id: "s".into(),
        }
    }

    async fn store_at(path: &Path, limit: usize) -> NoteStore {
        let mut ns = NoteStore::new(path, limit);
        ns.initialize(&ctx()).await.unwrap();
        ns
    }

    // -- roundtrip + format --------------------------------------------------

    #[tokio::test]
    async fn roundtrip_with_delimiter_and_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = store_at(&path, 2_200).await;
        ns.add("first entry\nspanning two lines").unwrap();
        ns.add("second entry").unwrap();
        ns.add("third\n\nwith a blank interior line").unwrap();

        let reloaded = store_at(&path, 2_200).await;
        assert_eq!(reloaded.entries(), ns.entries());
        assert_eq!(reloaded.entries().len(), 3);
        assert_eq!(reloaded.entries()[0], "first entry\nspanning two lines");
        assert_eq!(reloaded.entries()[2], "third\n\nwith a blank interior line");
    }

    #[tokio::test]
    async fn single_multiline_entry_roundtrips_as_one_entry() {
        // The delimiter terminates entries, so even a one-entry file
        // contains `§` and is never misread as legacy line-per-entry.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = store_at(&path, 2_200).await;
        ns.add("line one\nline two\nline three").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(ENTRY_DELIMITER), "v2 file must contain §");

        let reloaded = store_at(&path, 2_200).await;
        assert_eq!(reloaded.entries().len(), 1);
        assert_eq!(reloaded.entries()[0], "line one\nline two\nline three");
    }

    #[test]
    fn parse_tolerates_missing_final_delimiter() {
        let entries = NoteStore::parse("a\n§\nb without terminator");
        assert_eq!(
            entries,
            vec!["a".to_string(), "b without terminator".to_string()]
        );
    }

    #[tokio::test]
    async fn add_rejects_delimiter_line_in_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        let err = ns.add("evil\n§\ninjected").unwrap_err();
        assert!(err.to_string().contains("entry delimiter"), "{err}");
    }

    // -- legacy migration ------------------------------------------------------

    #[tokio::test]
    async fn legacy_file_read_as_line_per_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        std::fs::write(&path, "fact one\nfact two\n\nfact three\n").unwrap();
        let ns = store_at(&path, 2_200).await;
        assert_eq!(
            ns.entries(),
            &[
                "fact one".to_string(),
                "fact two".to_string(),
                "fact three".to_string()
            ]
        );
        // Snapshot is frozen from the legacy content.
        let block = ns.system_prompt_block().unwrap();
        assert!(block.contains("fact one") && block.contains("fact three"));
    }

    #[tokio::test]
    async fn legacy_file_rewritten_as_v2_on_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        std::fs::write(&path, "fact one\nfact two\n").unwrap();
        let mut ns = store_at(&path, 2_200).await;
        ns.add("fact three").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(ENTRY_DELIMITER),
            "first write migrates to v2: {raw}"
        );

        let reloaded = store_at(&path, 2_200).await;
        assert_eq!(
            reloaded.entries(),
            &[
                "fact one".to_string(),
                "fact two".to_string(),
                "fact three".to_string()
            ]
        );
    }

    // -- add ---------------------------------------------------------------

    #[tokio::test]
    async fn add_trims_and_ignores_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        ns.add("   ").unwrap();
        assert!(ns.is_empty());
        ns.add("  padded fact  ").unwrap();
        assert_eq!(ns.entries(), &["padded fact".to_string()]);
    }

    #[tokio::test]
    async fn add_exact_duplicate_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        ns.add("a fact").unwrap();
        ns.add("a fact").unwrap();
        assert_eq!(ns.entries().len(), 1);
    }

    #[tokio::test]
    async fn add_over_budget_lists_all_entries_and_instructs() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 60).await;
        ns.add("first existing entry").unwrap();
        ns.add("second one").unwrap();

        let err = ns.add(&"x".repeat(40)).unwrap_err().to_string();
        // The cap is the curator: full list + usage + instruction.
        assert!(
            err.contains("Replace or remove existing entries first"),
            "instruction missing: {err}"
        );
        assert!(err.contains("1. first existing entry"), "{err}");
        assert!(err.contains("2. second one"), "{err}");
        assert!(err.contains("/60"), "usage missing: {err}");
        assert!(err.contains('%'), "percentage missing: {err}");
        // The store is unchanged.
        assert_eq!(ns.entries().len(), 2);
    }

    #[tokio::test]
    async fn add_over_budget_on_empty_store_says_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 10).await;
        let err = ns
            .add("a fact that is far too long")
            .unwrap_err()
            .to_string();
        assert!(err.contains("(no entries)"), "{err}");
    }

    // -- replace -------------------------------------------------------------

    #[tokio::test]
    async fn replace_single_match_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = store_at(&path, 2_200).await;
        ns.add("prefers gemma3:4b for fast tier").unwrap();
        ns.add("workspace is /home/user/proj").unwrap();

        ns.replace("gemma3:4b", "prefers qwen3:8b for fast tier")
            .unwrap();
        assert_eq!(ns.entries()[0], "prefers qwen3:8b for fast tier");

        let reloaded = store_at(&path, 2_200).await;
        assert_eq!(reloaded.entries(), ns.entries());
    }

    #[tokio::test]
    async fn replace_zero_matches_is_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        ns.add("only entry").unwrap();
        let err = ns.replace("missing", "new text").unwrap_err().to_string();
        assert!(err.contains("no entry contains \"missing\""), "{err}");
    }

    #[tokio::test]
    async fn replace_ambiguous_lists_matches_and_asks_for_specificity() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        ns.add("model alpha is fast").unwrap();
        ns.add("model beta is slow").unwrap();
        ns.add("unrelated entry").unwrap();

        let err = ns.replace("model", "replacement").unwrap_err().to_string();
        assert!(err.contains("Be more specific"), "{err}");
        assert!(err.contains("model alpha is fast"), "{err}");
        assert!(err.contains("model beta is slow"), "{err}");
        assert!(
            !err.contains("unrelated entry"),
            "only matches listed: {err}"
        );
    }

    #[tokio::test]
    async fn replace_over_budget_fails_with_curator_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 40).await;
        ns.add("short entry").unwrap();
        let err = ns
            .replace("short", &"y".repeat(60))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Replace or remove existing entries first"),
            "{err}"
        );
        assert_eq!(ns.entries()[0], "short entry", "store unchanged on error");
    }

    #[tokio::test]
    async fn replace_with_empty_text_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        ns.add("an entry").unwrap();
        let err = ns.replace("an entry", "   ").unwrap_err().to_string();
        assert!(err.contains("use remove"), "{err}");
    }

    // -- remove --------------------------------------------------------------

    #[tokio::test]
    async fn remove_single_match_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = store_at(&path, 2_200).await;
        ns.add("fact one").unwrap();
        ns.add("fact two").unwrap();
        ns.remove("one").unwrap();
        assert_eq!(ns.entries(), &["fact two".to_string()]);

        let reloaded = store_at(&path, 2_200).await;
        assert_eq!(reloaded.entries(), &["fact two".to_string()]);
    }

    #[tokio::test]
    async fn remove_zero_matches_is_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        ns.add("something").unwrap();
        let err = ns.remove("not there").unwrap_err().to_string();
        assert!(err.contains("no entry contains \"not there\""), "{err}");
    }

    #[tokio::test]
    async fn remove_ambiguous_lists_matched_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        ns.add("alpha fact").unwrap();
        ns.add("beta fact").unwrap();
        let err = ns.remove("fact").unwrap_err().to_string();
        assert!(err.contains("Be more specific"), "{err}");
        assert!(
            err.contains("alpha fact") && err.contains("beta fact"),
            "{err}"
        );
        assert_eq!(ns.entries().len(), 2, "store unchanged on error");
    }

    #[tokio::test]
    async fn remove_empty_substring_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        ns.add("entry").unwrap();
        assert!(ns.remove("  ").is_err());
    }

    // -- prompt block + usage --------------------------------------------------

    #[tokio::test]
    async fn header_includes_usage_and_percentage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        std::fs::write(&path, "a fact\n§\n").unwrap();
        let ns = store_at(&path, 100).await;
        let block = ns.system_prompt_block().unwrap();
        let header = block.lines().next().unwrap();
        assert!(header.starts_with("## Agent Notes ("), "{header}");
        assert!(header.contains("/100"), "{header}");
        assert!(header.contains("6%"), "6/100 chars = 6%: {header}");
    }

    #[tokio::test]
    async fn empty_store_contributes_no_prompt_block() {
        let dir = tempfile::tempdir().unwrap();
        let ns = store_at(&dir.path().join("NOTES.md"), 100).await;
        assert!(ns.system_prompt_block().is_none());
    }

    #[tokio::test]
    async fn frozen_snapshot_ignores_mid_session_writes() {
        // By design: notes loaded at initialize stay frozen for the session
        // (prefix-cache stability). Do not "fix" this.
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 2_200).await;
        assert!(ns.system_prompt_block().is_none());
        ns.add("new fact").unwrap();
        assert!(ns.system_prompt_block().is_none(), "snapshot is frozen");
        assert_eq!(ns.entries(), &["new fact".to_string()]);
    }

    #[tokio::test]
    async fn frozen_snapshot_keeps_initial_content_after_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        std::fs::write(&path, "initial fact\n§\n").unwrap();
        let mut ns = store_at(&path, 2_200).await;
        ns.remove("initial").unwrap();
        let block = ns.system_prompt_block().unwrap();
        assert!(block.contains("initial fact"), "snapshot frozen: {block}");
        assert!(ns.is_empty(), "live state updated");
    }

    #[tokio::test]
    async fn usage_and_char_usage_report_rendered_length() {
        let dir = tempfile::tempdir().unwrap();
        let mut ns = store_at(&dir.path().join("NOTES.md"), 100).await;
        let (cur, max) = ns.char_usage();
        assert_eq!((cur, max), (0, 100));
        ns.add("abcde").unwrap();
        let (label, cur, max) = ns.usage().unwrap();
        assert_eq!(label, "notes");
        assert_eq!((cur, max), (5, 100));
    }

    // -- atomic write ------------------------------------------------------------

    #[tokio::test]
    async fn save_leaves_no_tmp_file_and_replaces_stale_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let tmp = dir.path().join("NOTES.md.tmp");
        // Simulate a crash mid-write from a previous run.
        std::fs::write(&tmp, "garbage from an interrupted write").unwrap();

        let mut ns = store_at(&path, 2_200).await;
        ns.add("durable fact").unwrap();

        assert!(!tmp.exists(), "tmp is renamed away after a save");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("durable fact"));
        assert!(!raw.contains("garbage"));
    }

    #[tokio::test]
    async fn interrupted_write_never_corrupts_the_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = store_at(&path, 2_200).await;
        ns.add("good entry").unwrap();

        // Simulate a crash mid-write: a partial tmp exists, the real file
        // was never touched (write-then-rename guarantees this ordering).
        std::fs::write(dir.path().join("NOTES.md.tmp"), "parti").unwrap();

        let reloaded = store_at(&path, 2_200).await;
        assert_eq!(reloaded.entries(), &["good entry".to_string()]);
    }

    // -- advisory lock ----------------------------------------------------------

    #[tokio::test]
    async fn lock_contention_two_stores_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let a = store_at(&path, 2_200).await;
        let mut b = store_at(&path, 2_200).await;
        // Keep the test fast: don't wait the full default retry budget.
        b.lock_retries = 3;
        b.lock_retry_delay = Duration::from_millis(5);

        // A holds the lock (simulating a write in progress).
        let guard = a.acquire_lock().unwrap();
        let err = b.add("rejected entry").unwrap_err().to_string();
        assert!(err.contains(".NOTES.md.lock"), "{err}");
        // The failed save must not leave a phantom in-memory entry that a
        // later successful write would silently persist.
        assert!(b.is_empty(), "failed add must not mutate live entries");
        drop(guard);

        // Lock released — B can write now.
        b.add("accepted entry").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("accepted entry"));
        assert!(
            !raw.contains("rejected entry"),
            "rejected write must not leak to disk"
        );
    }

    #[tokio::test]
    async fn stale_lock_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = store_at(&path, 2_200).await;
        ns.lock_stale = Duration::from_millis(50);

        // A leftover lock from a "crashed" process.
        std::fs::write(dir.path().join(".NOTES.md.lock"), "").unwrap();
        std::thread::sleep(Duration::from_millis(120));

        ns.add("fact after takeover").unwrap();
        assert!(
            !dir.path().join(".NOTES.md.lock").exists(),
            "lock released after the write"
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("fact after takeover"));
    }

    #[tokio::test]
    async fn lock_released_after_each_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NOTES.md");
        let mut ns = store_at(&path, 2_200).await;
        ns.add("one").unwrap();
        ns.add("two").unwrap();
        assert!(!dir.path().join(".NOTES.md.lock").exists());
    }
}

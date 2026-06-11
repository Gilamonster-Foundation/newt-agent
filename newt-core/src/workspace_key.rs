//! Workspace identity v2 — step 17.2 (issue #246).
//!
//! [`workspace_key_v2`] derives the stable key that scopes conversations to
//! a workspace (the `workspace_key` column in `conversations.db`), per the
//! decision doc `docs/decisions/conversation_context_architecture.md`
//! §"Cross-cutting: folder-derived `ConversationId`" and the design doc
//! `docs/design/context-memory-hermes-learnings.md` §"Workspace identity":
//!
//! * **Git-keyed** (the thesis case): when the directory is inside a git
//!   work tree that has an `origin` remote *and* a checked-out branch, the
//!   key is the BLAKE3 hex of `(origin URL, branch)`. The *same logical
//!   project* — any clone, any container, any path — derives the **same**
//!   key, so its conversations are shared wherever the project travels.
//! * **Path-keyed** (the fallback): otherwise the key is the BLAKE3 hex of
//!   the canonical absolute path — stable for the same directory, distinct
//!   across directories, exactly the granularity the retired UUIDv5 keying
//!   had.
//!
//! The two domains carry distinct hash prefixes, so a git-keyed value can
//! never collide with a path-keyed one, and every field is length-prefixed
//! into the hash (`("ab","c")` ≠ `("a","bc")` — same discipline as the §6
//! turn chain in [`crate::store`]).
//!
//! # Derivation choices (documented per the 17.2 acceptance notes)
//!
//! * **A branch is a different conversation context.** The branch name is
//!   part of the key, so checking out a different branch derives a
//!   *different* key. This is the decision doc's choice ("git remote URL +
//!   worktree branch"): work on `main` and work on `feat/x` are different
//!   conversations, and two worktrees of one repo (which git forces onto
//!   different branches) correctly get different contexts.
//! * **Detached HEAD falls back to path-keying.** A detached checkout is
//!   not a durable line of work: keying by commit hash would mint a new
//!   conversation context on every `checkout <sha>` and silently merge
//!   unrelated checkouts that happen to land on one commit. The canonical
//!   path is the stable, least-surprising identity for that state.
//! * **No `origin` remote falls back to path-keying.** Without a remote URL
//!   there is no cross-clone identity to share; the canonical path of the
//!   directory (not the repo toplevel) keeps the UUIDv5-era granularity.
//! * **The `origin` URL is read verbatim from `.git/config`** — no
//!   `url.<base>.insteadOf` rewriting, no normalization between ssh/https
//!   spellings. Two clones share conversations when their configured URLs
//!   are byte-identical; normalizing URL spellings is a judgment this
//!   module deliberately does not make.
//!
//! # Why not `git2` (or shelling out to `git`)?
//!
//! The derivation needs exactly three reads: find `.git`, read `HEAD`, read
//! `config`. Both file formats are plain text and stable for decades;
//! parsing them directly is ~100 lines with zero new dependencies. `git2`
//! would add a libgit2 FFI build (C toolchain, vendored OpenSSL questions,
//! slower cold builds) for what amounts to two `read_to_string` calls —
//! not worth a dependency under this repo's "argue every new dep" contract.
//! Shelling out to `git` would add a runtime binary requirement and a
//! subprocess per store open. Worktrees (`.git` *file* → `gitdir:` →
//! `commondir`) are handled below, which is the only structurally
//! interesting part of either format.

use std::path::{Path, PathBuf};

/// Domain-separation prefix for git-keyed (remote + branch) derivations.
const KEY_V2_GIT_PREFIX: &[u8] = b"newt-workspace-key:v2:git";

/// Domain-separation prefix for path-keyed (canonical path) derivations.
const KEY_V2_PATH_PREFIX: &[u8] = b"newt-workspace-key:v2:path";

/// Derive the v2 workspace key for a directory (see the module docs).
///
/// Errors only when the directory itself cannot be canonicalized (it does
/// not exist / is unreadable) — the same contract the UUIDv5 derivation
/// had. Every *git-side* irregularity (corrupt `.git`, detached HEAD, no
/// remote, unreadable config) degrades to the path-keyed fallback instead
/// of erroring: a broken repo must never block opening the store.
pub fn workspace_key_v2(dir: impl AsRef<Path>) -> anyhow::Result<String> {
    let canonical = std::fs::canonicalize(dir.as_ref())?;
    if let Some((url, branch)) = git_identity(&canonical) {
        return Ok(domain_hash(
            KEY_V2_GIT_PREFIX,
            &[url.as_bytes(), branch.as_bytes()],
        ));
    }
    // Match the UUIDv5 derivation's separator normalization so the same
    // Windows path always hashes identically regardless of spelling.
    let normalized = canonical.to_string_lossy().replace('\\', "/");
    Ok(domain_hash(KEY_V2_PATH_PREFIX, &[normalized.as_bytes()]))
}

/// `(origin URL, branch)` when `start` is inside a git work tree with both;
/// `None` routes the caller to the path-keyed fallback.
fn git_identity(start: &Path) -> Option<(String, String)> {
    let git_dir = discover_git_dir(start)?;
    let branch = head_branch(&git_dir.join("HEAD"))?;
    let url = origin_url(&config_path(&git_dir))?;
    Some((url, branch))
}

/// Walk up from `start` (inclusive) looking for a `.git` entry, mirroring
/// `git rev-parse --git-dir` discovery: a *directory* is the git dir
/// itself; a *file* is a worktree/submodule pointer (`gitdir: <path>`)
/// resolved relative to the directory containing it.
fn discover_git_dir(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            return resolve_gitdir_file(&dot_git, dir);
        }
        current = dir.parent();
    }
    None
}

/// Resolve a `.git` *file*'s `gitdir: <path>` pointer (linked worktrees,
/// submodules). A relative path is relative to the dir holding the file.
fn resolve_gitdir_file(dot_git: &Path, containing_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dot_git).ok()?;
    let target = text.lines().next()?.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let resolved = containing_dir.join(target); // join() keeps absolutes as-is
    Some(std::fs::canonicalize(&resolved).unwrap_or(resolved))
}

/// Where this git dir's `config` lives. Linked worktrees keep per-worktree
/// state (HEAD) in their own git dir but share `config` through the
/// `commondir` pointer file; a main checkout has no `commondir`.
fn config_path(git_dir: &Path) -> PathBuf {
    if let Ok(text) = std::fs::read_to_string(git_dir.join("commondir")) {
        let rel = text.trim();
        if !rel.is_empty() {
            return git_dir.join(rel).join("config");
        }
    }
    git_dir.join("config")
}

/// The checked-out branch from a `HEAD` file: `ref: refs/heads/<branch>`.
/// Detached HEAD (a bare commit hash) or a ref outside `refs/heads/`
/// returns `None` — path-keyed fallback (see the module docs).
fn head_branch(head: &Path) -> Option<String> {
    let text = std::fs::read_to_string(head).ok()?;
    let branch = text
        .lines()
        .next()?
        .trim()
        .strip_prefix("ref:")?
        .trim()
        .strip_prefix("refs/heads/")?;
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

/// First `url` of `[remote "origin"]` in a git config file — the same value
/// `git remote get-url origin` reports for the URL-only configs git itself
/// writes (no `insteadOf` rewriting — module docs). The INI subset parsed
/// here is exactly what `git clone`/`git remote add` emit: section headers,
/// `key = value` lines, `#`/`;` comments.
fn origin_url(config: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config).ok()?;
    let mut in_origin = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            in_origin = is_origin_section_header(line);
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim().eq_ignore_ascii_case("url") {
                let value = value.trim();
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// `true` for `[remote "origin"]` — section name case-insensitive,
/// subsection name exact, per git-config(5).
fn is_origin_section_header(line: &str) -> bool {
    let Some(inner) = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    let inner = inner.trim();
    let Some((section, subsection)) = inner.split_once(char::is_whitespace) else {
        return false;
    };
    section.eq_ignore_ascii_case("remote") && subsection.trim() == "\"origin\""
}

/// BLAKE3 hex over a domain prefix + length-prefixed fields (the same
/// unambiguous-encoding discipline as the store's §6 genesis/turn hashing).
fn domain_hash(domain: &[u8], fields: &[&[u8]]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    for field in fields {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-craft the minimal git-dir layout the parser reads. Tests at this
    /// level pin the *format* handling; the integration suite
    /// (`tests/workspace_key.rs`) proves the same against real `git` output.
    fn fake_repo(dir: &Path, url: Option<&str>, head: &str) {
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), head).unwrap();
        let mut config = String::from("[core]\n\trepositoryformatversion = 0\n");
        if let Some(url) = url {
            config.push_str(&format!("[remote \"origin\"]\n\turl = {url}\n"));
        }
        std::fs::write(git.join("config"), config).unwrap();
    }

    #[test]
    fn same_remote_and_branch_derive_the_same_key_across_paths() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let url = Some("https://example.invalid/owner/project.git");
        fake_repo(a.path(), url, "ref: refs/heads/main\n");
        fake_repo(b.path(), url, "ref: refs/heads/main\n");
        assert_eq!(
            workspace_key_v2(a.path()).unwrap(),
            workspace_key_v2(b.path()).unwrap(),
            "two checkouts of one logical project must share a key"
        );
    }

    #[test]
    fn branch_change_derives_a_different_key() {
        let dir = tempfile::tempdir().unwrap();
        let url = Some("https://example.invalid/owner/project.git");
        fake_repo(dir.path(), url, "ref: refs/heads/main\n");
        let on_main = workspace_key_v2(dir.path()).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/feat/other\n").unwrap();
        let on_feat = workspace_key_v2(dir.path()).unwrap();
        assert_ne!(
            on_main, on_feat,
            "a branch is a different conversation context (module docs)"
        );
    }

    #[test]
    fn subdirectory_of_a_repo_shares_the_repo_key() {
        let dir = tempfile::tempdir().unwrap();
        fake_repo(
            dir.path(),
            Some("https://example.invalid/o/p.git"),
            "ref: refs/heads/main\n",
        );
        let sub = dir.path().join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            workspace_key_v2(dir.path()).unwrap(),
            workspace_key_v2(&sub).unwrap(),
            "discovery walks up to the repo, like `git rev-parse`"
        );
    }

    #[test]
    fn non_git_dir_is_path_keyed_stable_and_distinct() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = workspace_key_v2(a.path()).unwrap();
        let again = workspace_key_v2(a.path()).unwrap();
        let other = workspace_key_v2(b.path()).unwrap();
        assert_eq!(first, again, "path key must be stable for the same dir");
        assert_ne!(first, other, "different dirs must derive different keys");
        // Pin the fallback formula itself: BLAKE3 over the path domain.
        let canonical = std::fs::canonicalize(a.path()).unwrap();
        let normalized = canonical.to_string_lossy().replace('\\', "/");
        assert_eq!(
            first,
            domain_hash(KEY_V2_PATH_PREFIX, &[normalized.as_bytes()])
        );
    }

    #[test]
    fn detached_head_falls_back_to_path_keying() {
        let dir = tempfile::tempdir().unwrap();
        fake_repo(
            dir.path(),
            Some("https://example.invalid/o/p.git"),
            // Detached: HEAD is a bare commit hash, no `ref:` line.
            "a94a8fe5ccb19ba61c4c0873d391e987982fbbd3\n",
        );
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let normalized = canonical.to_string_lossy().replace('\\', "/");
        assert_eq!(
            workspace_key_v2(dir.path()).unwrap(),
            domain_hash(KEY_V2_PATH_PREFIX, &[normalized.as_bytes()]),
            "detached HEAD must be path-keyed (module docs choice)"
        );
    }

    #[test]
    fn repo_without_origin_remote_falls_back_to_path_keying() {
        let dir = tempfile::tempdir().unwrap();
        fake_repo(dir.path(), None, "ref: refs/heads/main\n");
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let normalized = canonical.to_string_lossy().replace('\\', "/");
        assert_eq!(
            workspace_key_v2(dir.path()).unwrap(),
            domain_hash(KEY_V2_PATH_PREFIX, &[normalized.as_bytes()])
        );
    }

    #[test]
    fn worktree_gitfile_resolves_through_gitdir_and_commondir() {
        // Layout a linked worktree by hand: main repo holds config; the
        // worktree's `.git` FILE points at a per-worktree git dir whose
        // `commondir` points back at the main git dir.
        let main = tempfile::tempdir().unwrap();
        fake_repo(
            main.path(),
            Some("ssh://git@example.invalid/o/p.git"),
            "ref: refs/heads/main\n",
        );
        let wt_gitdir = main.path().join(".git/worktrees/wt1");
        std::fs::create_dir_all(&wt_gitdir).unwrap();
        std::fs::write(wt_gitdir.join("HEAD"), "ref: refs/heads/feat/wt\n").unwrap();
        std::fs::write(wt_gitdir.join("commondir"), "../..\n").unwrap();

        let wt = tempfile::tempdir().unwrap();
        std::fs::write(
            wt.path().join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();

        // The worktree must be git-keyed by (main repo's URL, its own
        // branch): identical to any other checkout of that URL + branch.
        let twin = tempfile::tempdir().unwrap();
        fake_repo(
            twin.path(),
            Some("ssh://git@example.invalid/o/p.git"),
            "ref: refs/heads/feat/wt\n",
        );
        assert_eq!(
            workspace_key_v2(wt.path()).unwrap(),
            workspace_key_v2(twin.path()).unwrap(),
            "worktree must key by (shared config URL, per-worktree branch)"
        );
    }

    #[test]
    fn git_and_path_domains_never_collide() {
        // Even if a canonical path were literally the length-prefixed
        // (url, branch) byte string, the domain prefixes keep the two
        // derivations apart.
        assert_ne!(
            domain_hash(KEY_V2_GIT_PREFIX, &[b"u", b"b"]),
            domain_hash(KEY_V2_PATH_PREFIX, &[b"u", b"b"])
        );
        // Field boundaries are unambiguous: ("ab","c") != ("a","bc").
        assert_ne!(
            domain_hash(KEY_V2_GIT_PREFIX, &[b"ab", b"c"]),
            domain_hash(KEY_V2_GIT_PREFIX, &[b"a", b"bc"])
        );
    }

    #[test]
    fn origin_section_header_matching_is_git_faithful() {
        assert!(is_origin_section_header("[remote \"origin\"]"));
        assert!(is_origin_section_header("[REMOTE \"origin\"]")); // section: case-insensitive
        assert!(!is_origin_section_header("[remote \"Origin\"]")); // subsection: exact
        assert!(!is_origin_section_header("[remote \"upstream\"]"));
        assert!(!is_origin_section_header("[branch \"origin\"]"));
        assert!(!is_origin_section_header("[remote]"));
        assert!(!is_origin_section_header("not a header"));
    }

    #[test]
    fn origin_url_parses_comments_quotes_and_key_case() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::write(
            &config,
            "# global comment\n\
             [remote \"upstream\"]\n\
             \turl = https://example.invalid/wrong.git\n\
             [remote \"origin\"]\n\
             \t; another comment\n\
             \tfetch = +refs/heads/*:refs/remotes/origin/*\n\
             \tURL = \"https://example.invalid/right.git\"\n",
        )
        .unwrap();
        assert_eq!(
            origin_url(&config).as_deref(),
            Some("https://example.invalid/right.git")
        );
    }

    #[test]
    fn head_branch_handles_refs_outside_heads_and_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let head = dir.path().join("HEAD");
        std::fs::write(&head, "ref: refs/heads/feat/x\n").unwrap();
        assert_eq!(head_branch(&head).as_deref(), Some("feat/x"));
        std::fs::write(&head, "ref: refs/tags/v1\n").unwrap();
        assert_eq!(head_branch(&head), None, "non-branch ref is not a branch");
        std::fs::write(&head, "ref: refs/heads/\n").unwrap();
        assert_eq!(head_branch(&head), None, "empty branch name is invalid");
        std::fs::write(&head, "").unwrap();
        assert_eq!(head_branch(&head), None);
        assert_eq!(head_branch(&dir.path().join("missing")), None);
    }

    #[test]
    fn corrupt_gitdir_pointer_degrades_to_path_keying() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "this is not a gitdir pointer").unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let normalized = canonical.to_string_lossy().replace('\\', "/");
        assert_eq!(
            workspace_key_v2(dir.path()).unwrap(),
            domain_hash(KEY_V2_PATH_PREFIX, &[normalized.as_bytes()]),
            "a broken repo must never block key derivation"
        );
    }
}

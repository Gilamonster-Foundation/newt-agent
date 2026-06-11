//! Workspace identity v2 against REAL git output — step 17.2 (issue #246).
//!
//! The unit tests inside `newt-core/src/workspace_key.rs` pin the parser
//! against hand-crafted `.git` layouts; this suite proves the same
//! derivation rules hold for repositories `git` itself writes — bare
//! origin + clones, `checkout -b`, linked worktrees (`.git` file +
//! `commondir`), detached HEAD — with no network: the "remote" is a bare
//! repo in a tempdir, cloned by absolute path.
//!
//! It also proves the cross-clone thesis end to end through the store: two
//! checkouts of the same logical project SHARE a conversation scope.

use std::path::{Path, PathBuf};

use newt_core::{workspace_key_v2, ConversationStore};

/// Run `git` in `dir` with hermetic identity/config; panic on failure.
///
/// The `GIT_*` environment is scrubbed because these tests can run inside a
/// git hook (the pre-push gate runs `cargo test`), and hooks export
/// `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` pointing at the REAL repo —
/// which overrides `-C`, so the fixture's `add`/`commit` would land on the
/// developer's actual branch (observed: a tree-wiping "seed commit" minted
/// onto a feature branch during a worktree push; see issue #276's sibling
/// finding). Scrubbing makes `-C` authoritative again.
fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_PREFIX")
        .args([
            "-c",
            "user.name=newt-test",
            "-c",
            "user.email=newt-test@example.invalid",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("these tests need the `git` binary on PATH");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed:\n{}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build the fixture: a seed repo with one commit, published to a bare
/// "origin", plus `n` clones of it (cloned by the SAME absolute path
/// string, so their configured origin URLs are byte-identical — the
/// derivation hashes the URL verbatim, see the module docs).
fn fixture(tmp: &Path, n: usize) -> Vec<PathBuf> {
    let seed = tmp.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init"]);
    std::fs::write(seed.join("README.md"), "fixture\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "seed commit"]);
    git(tmp, &["clone", "--bare", "seed", "origin.git"]);
    let origin = tmp.join("origin.git");
    let origin_url = origin.to_str().unwrap().to_string();

    (0..n)
        .map(|i| {
            let name = format!("clone-{i}");
            git(tmp, &["clone", &origin_url, &name]);
            tmp.join(name)
        })
        .collect()
}

/// The thesis case: same remote + same branch ⇒ same key, regardless of
/// the checkout's path — and therefore one shared conversation scope.
#[test]
fn two_clones_of_one_project_share_a_key_and_a_conversation_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let clones = fixture(tmp.path(), 2);
    let key_a = workspace_key_v2(&clones[0]).unwrap();
    let key_b = workspace_key_v2(&clones[1]).unwrap();
    assert_eq!(
        key_a, key_b,
        "same remote+branch at different paths must derive ONE key"
    );

    // End to end through the store: a conversation started in clone A is
    // the SAME conversation context in clone B (shared db root, as on one
    // machine — across containers the db travels with ~/.newt).
    let store_root = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(store_root.path(), &clones[0], 100).unwrap();
    let id = store_a.create("started in clone A", None).unwrap();
    store_a.append_turn(&id, "begin here", "ok").unwrap();

    let store_b = ConversationStore::new(store_root.path(), &clones[1], 100).unwrap();
    assert!(
        store_b.exists(&id).unwrap(),
        "clone B must see clone A's conversation"
    );
    store_b.append_turn(&id, "continue there", "ok").unwrap();
    store_b.verify_chain(&id).unwrap();
    assert_eq!(store_b.load(&id).unwrap().turns.len(), 2);
    assert_eq!(store_a.load(&id).unwrap().turns.len(), 2);
}

/// A branch is a different conversation context (decision-doc choice): the
/// key changes with the branch, and re-converges for any checkout of that
/// same remote+branch pair.
#[test]
fn branch_change_derives_a_different_key_that_other_clones_reconverge_on() {
    let tmp = tempfile::tempdir().unwrap();
    let clones = fixture(tmp.path(), 2);
    let on_main = workspace_key_v2(&clones[0]).unwrap();

    git(&clones[0], &["checkout", "-b", "feature-x"]);
    let on_feature = workspace_key_v2(&clones[0]).unwrap();
    assert_ne!(
        on_main, on_feature,
        "a branch is a different conversation context"
    );

    git(&clones[1], &["checkout", "-b", "feature-x"]);
    assert_eq!(
        on_feature,
        workspace_key_v2(&clones[1]).unwrap(),
        "any checkout of (remote, feature-x) shares the feature key"
    );
}

/// A linked worktree (`.git` FILE → `gitdir:` → `commondir`) keys by the
/// shared origin URL and its OWN branch — identical to a plain clone
/// checked out to that branch.
#[test]
fn linked_worktree_keys_like_a_clone_on_its_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let clones = fixture(tmp.path(), 2);

    let wt = tmp.path().join("wt");
    git(
        &clones[0],
        &["worktree", "add", wt.to_str().unwrap(), "-b", "wt-branch"],
    );
    assert!(
        wt.join(".git").is_file(),
        "fixture sanity: a linked worktree has a .git FILE"
    );

    git(&clones[1], &["checkout", "-b", "wt-branch"]);
    assert_eq!(
        workspace_key_v2(&wt).unwrap(),
        workspace_key_v2(&clones[1]).unwrap(),
        "worktree = (shared remote URL, per-worktree branch)"
    );
    assert_ne!(
        workspace_key_v2(&wt).unwrap(),
        workspace_key_v2(&clones[0]).unwrap(),
        "the worktree's branch differs from the main checkout's"
    );
}

/// Detached HEAD falls back to path-keying (documented choice): stable for
/// the directory, but NOT shared across clones — there is no branch
/// identity to share.
#[test]
fn detached_head_falls_back_to_per_path_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let clones = fixture(tmp.path(), 2);
    let attached = workspace_key_v2(&clones[0]).unwrap();

    git(&clones[0], &["checkout", "--detach"]);
    git(&clones[1], &["checkout", "--detach"]);
    let detached_a = workspace_key_v2(&clones[0]).unwrap();
    let detached_b = workspace_key_v2(&clones[1]).unwrap();

    assert_ne!(detached_a, attached, "detached must not reuse the git key");
    assert_ne!(
        detached_a, detached_b,
        "detached checkouts are path-scoped, never shared"
    );
    assert_eq!(
        detached_a,
        workspace_key_v2(&clones[0]).unwrap(),
        "path fallback is stable for the same dir"
    );
}

/// A repo with no `origin` remote has no cross-clone identity: path-keyed,
/// stable per dir, distinct across dirs.
#[test]
fn repo_without_origin_remote_is_path_keyed() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("local-a");
    let b = tmp.path().join("local-b");
    for dir in [&a, &b] {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init"]);
    }
    let key_a = workspace_key_v2(&a).unwrap();
    let key_b = workspace_key_v2(&b).unwrap();
    assert_eq!(key_a, workspace_key_v2(&a).unwrap(), "stable per dir");
    assert_ne!(key_a, key_b, "no remote ⇒ no shared identity across dirs");
}

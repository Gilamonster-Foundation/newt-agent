//! Read-only content witness for #2831. Git's canonical stage listing includes
//! every tracked path, mode and blob OID. Equality of this listing plus index ==
//! HEAD is equivalent to equality of Git trees, without `write-tree` mutations.
use content_addressable::RawContentId;
use std::path::Path;

fn git(root: &Path, read: &crate::Scope<String>, args: &[&str]) -> Option<Vec<u8>> {
    // Reuse the metadata read gate and hardened Git, including no optional
    // index locks. Bounded read grants decline this optional hint entirely.
    let output = crate::git_hardening::metadata_git(root, args, read)
        .ok()?
        .output()
        .ok()?;
    (output.status.success() && output.stdout.len() <= 4 * 1024 * 1024).then_some(output.stdout)
}

pub(super) fn index_tree(root: &Path, read: &crate::Scope<String>) -> Option<RawContentId> {
    // A clean filter can execute while Git compares the worktree to the index.
    // Optional advice declines configured filters rather than invoking them.
    let config = git(root, read, &["config", "--null", "--list"])?;
    if config
        .split(|b| *b == 0)
        .any(|entry| entry.starts_with(b"filter."))
    {
        return None;
    }
    // Sparse/assume-unchanged entries can hide working content from diff-files.
    // Decline them and submodules rather than claiming a complete checked tree.
    let flags = git(root, read, &["ls-files", "-v", "-z"])?;
    if flags
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .any(|e| e[0] != b'H')
    {
        return None;
    }
    let entries = git(root, read, &["ls-files", "--stage", "-z"])?;
    if entries
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .any(|entry| {
            entry.starts_with(b"160000 ")
                || !entry
                    .split(|b| *b == b'\t')
                    .next()
                    .is_some_and(|fields| fields.ends_with(b" 0"))
        })
    {
        return None;
    }
    // Require the index to represent everything Cargo saw: no unstaged tracked
    // changes or untracked non-ignored source. Ignored build output is excluded.
    git(
        root,
        read,
        &["diff-files", "--quiet", "--no-ext-diff", "--no-textconv"],
    )?;
    if !git(
        root,
        read,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?
    .is_empty()
    {
        return None;
    }
    // A concurrent index change cannot silently substitute the checked listing.
    if git(root, read, &["ls-files", "--stage", "-z"])? != entries {
        return None;
    }
    Some(RawContentId::from_content(&entries))
}

pub(super) fn index_is_head(root: &Path, read: &crate::Scope<String>) -> bool {
    git(
        root,
        read,
        &[
            "diff-index",
            "--cached",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
            "--",
        ],
    )
    .is_some()
}

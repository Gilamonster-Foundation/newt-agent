//! `newt caps` — the content identity of a vetted tool tree (#3034).
//!
//! `digest` names a directory by the root id of a
//! [`dir_tree::snapshot`]: one CIDv1 over every file's bytes, name and place,
//! so an operator can approve exact bytes rather than a path. The id is minted
//! by `content-addressable`; this module only refuses what the snapshot must
//! not silently accept, and reports.
//!
//! A tree holding a symlink, a hard link, or a fifo, socket or device is
//! refused. Where the hard-link check cannot run (std exposes no link count
//! off unix), every tree is refused rather than digested unchecked.
//!
//! A file link is `{id, size}` with **no mode bits**, so a `chmod` does not
//! change the id; whatever installs a tree derives modes on its own. The
//! node bytes are non-frozen upstream, which is why the workspace pins
//! `content-addressable` exactly.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::Subcommand;
use content_addressable::structures::dir_tree::{self, DirNode, Link};
use content_addressable::{ContentAddressable, MemoryStore, NodeStoreExt};
use serde::Serialize;

#[derive(Subcommand, Debug)]
pub enum CapsCmd {
    /// Print the content id (CIDv1) of a tool tree. Refuses a symlink, a hard
    /// link, or a fifo, socket or device anywhere in it, and refuses every tree
    /// on a platform where hard links cannot be detected.
    Digest {
        /// The tree to digest.
        dir: PathBuf,
        /// Print `{"cid", "size", "entries"}` instead of the bare id.
        #[arg(long)]
        json: bool,
    },
}

/// `size` is the total file bytes; `entries` counts every file and
/// subdirectory below the root.
#[derive(Debug, Serialize)]
struct Digest {
    cid: String,
    size: u64,
    entries: u64,
}

pub fn run(cmd: &CapsCmd) -> Result<()> {
    let CapsCmd::Digest { dir, json } = cmd;
    let d = digest(dir)?;
    if *json {
        println!("{}", serde_json::to_string(&d)?);
    } else {
        println!("{}", d.cid);
    }
    Ok(())
}

fn digest(dir: &Path) -> Result<Digest> {
    refuse_hard_links(dir)?;
    let mut store = MemoryStore::new();
    let root = dir_tree::snapshot(&mut store, dir)?;
    let (size, entries) = tally(&store, &root)?;
    Ok(Digest {
        cid: root.content_id()?.to_string(),
        size,
        entries,
    })
}

/// Bytes and entries below `link`, read back from the nodes the snapshot
/// stored, so the report describes exactly what was digested.
fn tally(store: &MemoryStore, link: &Link) -> Result<(u64, u64)> {
    match link {
        Link::File { size, .. } => Ok((*size, 0)),
        Link::Dir { id } => {
            let node: DirNode = store.get_node(id)?;
            node.entries().values().try_fold((0, 0), |(size, n), l| {
                let (s, m) = tally(store, l)?;
                Ok((size + s, n + m + 1))
            })
        }
    }
}

/// Refuse a regular file with more than one link anywhere under `path`: it
/// aliases bytes in the tree to a path outside it, which could change them
/// after approval. The snapshot already refuses symlinks, fifos, sockets and
/// devices with a named error; this walk adds only the link count it does not
/// check, and never follows a symlink.
#[cfg(unix)]
fn refuse_hard_links(path: &Path) -> Result<()> {
    use anyhow::Context;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.is_dir() {
        let entries = fs::read_dir(path).with_context(|| format!("reading {}", path.display()))?;
        for entry in entries {
            refuse_hard_links(&entry?.path())?;
        }
    } else if meta.is_file() && meta.nlink() > 1 {
        bail!("hard link refused: {}", path.display());
    }
    Ok(())
}

/// Off unix std has no stable link count (`number_of_links` is unstable), so
/// the check cannot run, and a digest that skipped it could silently accept an
/// NTFS hard link. Fail closed.
#[cfg(not(unix))]
fn refuse_hard_links(_: &Path) -> Result<()> {
    bail!("hard-link check unavailable on this platform; refusing to digest")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// `{README, bin/tool, lib/mod.py}` in a fresh tempdir.
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("README"), b"fixture\n").unwrap();
        fs::write(root.join("bin/tool"), b"#!/bin/sh\necho hi\n").unwrap();
        fs::write(root.join("lib/mod.py"), b"x = 1\n").unwrap();
        tmp
    }

    /// Pinned while dir_tree bytes are non-frozen upstream: a change here
    /// means a `content-addressable` bump changed every tree id, and the bump
    /// must say so.
    #[cfg(unix)]
    #[test]
    fn a_fixed_tree_digests_to_a_pinned_cid() {
        let tmp = fixture();
        let d = digest(tmp.path()).unwrap();
        assert_eq!(
            d.cid,
            "bafyr4ie6bqltrjdtpotlrvwclem35wba46vxg62xhyuezx5fhu5dduje74"
        );
    }

    #[cfg(unix)]
    #[test]
    fn json_reports_cid_size_and_entry_count() {
        let tmp = fixture();
        let d = digest(tmp.path()).unwrap();
        let v = serde_json::to_value(&d).unwrap();
        // 8 + 18 + 6 bytes; README, bin, bin/tool, lib, lib/mod.py.
        assert_eq!(
            v,
            serde_json::json!({ "cid": d.cid, "size": 32, "entries": 5 })
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused() {
        use content_addressable::structures::dir_tree::DirTreeError;
        let tmp = fixture();
        std::os::unix::fs::symlink("tool", tmp.path().join("bin/alias")).unwrap();
        let err = digest(tmp.path()).unwrap_err();
        assert!(
            matches!(err.downcast_ref(), Some(DirTreeError::Symlink { .. })),
            "{err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_link_is_refused() {
        let tmp = fixture();
        let outside = tempfile::tempdir().unwrap();
        let alias = outside.path().join("tool");
        fs::hard_link(tmp.path().join("bin/tool"), &alias).unwrap();
        let err = digest(tmp.path()).unwrap_err();
        assert!(
            err.to_string().starts_with("hard link refused: "),
            "{err:#}"
        );
    }

    /// `Link::File` carries no mode bits, so making a file executable (or
    /// not) leaves the id alone.
    #[cfg(unix)]
    #[test]
    fn chmod_does_not_change_the_cid() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = fixture();
        let before = digest(tmp.path()).unwrap().cid;
        let tool = tmp.path().join("bin/tool");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(digest(tmp.path()).unwrap().cid, before);
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(digest(tmp.path()).unwrap().cid, before);
    }

    /// Off unix the hard-link check cannot run, so a clean tree is refused
    /// rather than digested with the check skipped.
    #[cfg(not(unix))]
    #[test]
    fn without_a_link_count_every_tree_is_refused() {
        let tmp = fixture();
        let err = digest(tmp.path()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "hard-link check unavailable on this platform; refusing to digest"
        );
    }
}

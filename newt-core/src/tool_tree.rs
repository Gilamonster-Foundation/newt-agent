//! The single tool-tree identity used by `caps digest` and build admission.
//! Modes are intentionally not part of the CID; admission checks them separately.
use anyhow::{bail, Result};
use content_addressable::structures::dir_tree::{self, DirNode, Link};
use content_addressable::{ContentAddressable, ContentId, MemoryStore, NodeStoreExt};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Digest {
    pub cid: ContentId,
    pub size: u64,
    pub entries: u64,
}

/// Snapshot a link-free regular tree, using content-addressable's root-link CID.
/// Fails closed when hard-link detection is unavailable.
pub fn digest(dir: &Path) -> Result<Digest> {
    refuse_hard_links(dir)?;
    let mut store = MemoryStore::new();
    let root = dir_tree::snapshot(&mut store, dir)?;
    let (size, entries) = tally(&store, &root)?;
    Ok(Digest {
        cid: root.content_id()?,
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

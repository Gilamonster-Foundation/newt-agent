//! Path normalisation for authority admission: a lexical form and an
//! existing-ancestor-resolved form, with no filesystem effects.
//!
//! Moved out of `config::smart_harness` (#3140) so the policy layer can use
//! them without the config module.

use std::path::{Path, PathBuf};

pub fn normalize_path(path: &Path) -> anyhow::Result<PathBuf> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("frame authority paths must have lossless UTF-8 names"))?;
    Ok(crate::caveats::lexically_normalize(path))
}

/// Resolve existing ancestors without creating paths named by untrusted grants.
/// Unlike ResolvedPath's write setup, admission must have no filesystem effects.
pub fn resolve_uncreated_path(path: &Path) -> anyhow::Result<PathBuf> {
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let resolved = ancestor.canonicalize()?.join(path.strip_prefix(ancestor)?);
                return normalize_path(&resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("frame authority path has no existing ancestor")
}

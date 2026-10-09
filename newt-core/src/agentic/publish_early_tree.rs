//! #2831: optional content evidence with no process execution. Git blobs are
//! hashed from raw bytes; config/attributes can only make us decline evidence.
use content_addressable::{ContentAddressable, ContentId};
use grit_lib::{
    config::ConfigSet,
    index::Index,
    objects::{HashAlgo, ObjectId, ObjectKind},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 50_000;

#[derive(serde::Serialize)]
struct Tree(BTreeMap<Vec<u8>, (u32, Vec<u8>)>);
impl ContentAddressable for Tree {
    fn canonical_form(&self) -> Result<Vec<u8>, content_addressable::ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

fn read_file(path: &Path) -> Option<(Vec<u8>, std::fs::Metadata)> {
    use std::io::Read;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let file = crate::fs_cap::WorkspaceDir::open_granted_file(path, Path::new("."), true).ok()?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let file = {
        if !std::fs::symlink_metadata(path).ok()?.is_file() {
            return None;
        }
        std::fs::File::open(path).ok()?
    };
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_BYTES).then_some((bytes, meta))
}

fn config(admin: &Path) -> Option<ConfigSet> {
    // Pure parser, including system/global/repo/worktree config and includes.
    // No conversion is performed, regardless of subsequent config changes.
    let config = ConfigSet::load(Some(admin), true).ok()?;
    for entry in config.entries() {
        let key = entry.key.to_ascii_lowercase();
        if key.starts_with("filter.")
            || matches!(
                key.as_str(),
                "core.eol" | "core.attributesfile" | "core.excludesfile" | "core.worktree"
            )
            || (key == "core.autocrlf"
                && !entry.value.as_deref().is_some_and(|v| {
                    matches!(
                        v.to_ascii_lowercase().as_str(),
                        "false" | "no" | "off" | "0"
                    )
                }))
        {
            return None;
        }
    }
    Some(config)
}

fn index(root: &Path, read: &crate::Scope<String>) -> Option<(PathBuf, Vec<u8>, Index)> {
    // Preserve the existing optional-metadata read gate before any traversal.
    crate::agentic::check_git_read_scope("metadata", read).ok()?;
    let admin = crate::workspace_key::discover_git_dir(root)?;
    let cfg = config(&admin)?;
    let algo = HashAlgo::from_name(
        &cfg.get("extensions.objectformat")
            .unwrap_or_else(|| "sha1".into()),
    )?;
    let bytes = read_file(&admin.join("index"))?.0;
    // Bound allocation and the parser's fixed header/checksum accesses first.
    if bytes.len() < 12 + algo.len() {
        return None;
    }
    let count = u32::from_be_bytes(bytes.get(8..12)?.try_into().ok()?) as usize;
    if count > MAX_ENTRIES {
        return None;
    }
    let index = Index::parse_with_algo(&bytes, algo).ok()?;
    if index.sparse_directories
        || index.split_index_base_oid().is_some()
        || index.entries.iter().any(|e| {
            e.stage() != 0
                || e.assume_unchanged()
                || e.skip_worktree()
                || e.intent_to_add()
                || !matches!(e.mode, 0o100644 | 0o100755)
        })
    {
        return None;
    }
    Some((admin, bytes, index))
}

fn absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

fn plain_path(root: &Path, name: &[u8]) -> Option<PathBuf> {
    let name = std::str::from_utf8(name).ok()?;
    if name.is_empty() || name.contains('\\') {
        return None;
    }
    let mut path = root.to_path_buf();
    let parts: Vec<_> = Path::new(name).components().collect();
    for (i, part) in parts.iter().enumerate() {
        let std::path::Component::Normal(part) = part else {
            return None;
        };
        path.push(part);
        if i + 1 < parts.len() && !std::fs::symlink_metadata(&path).ok()?.is_dir() {
            return None;
        }
    }
    Some(path)
}

fn no_attributes(root: &Path, admin: &Path, index: &Index) -> Option<()> {
    let common = grit_lib::refs::common_dir(admin).unwrap_or_else(|| admin.to_owned());
    let mut paths = std::collections::BTreeSet::from([
        root.join(".gitattributes"),
        admin.join("info/attributes"),
        common.join("info/attributes"),
    ]);
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    {
        paths.insert(config_home.join("git/attributes"));
    }
    paths.insert(PathBuf::from("/etc/gitattributes"));
    if let Ok(program) =
        crate::git_hardening::trusted_git_program(std::env::var_os("PATH").as_deref())
    {
        if let Some(prefix) = program.parent().and_then(Path::parent) {
            paths.insert(prefix.join("etc/gitattributes"));
        }
    }
    for entry in &index.entries {
        let path = plain_path(root, &entry.path)?;
        let mut parent = path.parent()?;
        while parent.starts_with(root) {
            paths.insert(parent.join(".gitattributes"));
            let Some(next) = parent.parent() else {
                break;
            };
            parent = next;
        }
    }
    paths.iter().all(|p| absent(p)).then_some(())
}

pub(super) fn index_tree(root: &Path, read: &crate::Scope<String>) -> Option<ContentId> {
    index_tree_with(root, read, || {})
}

fn index_tree_with(
    root: &Path,
    read: &crate::Scope<String>,
    after_screen: impl FnOnce(),
) -> Option<ContentId> {
    let (admin, before, index) = index(root, read)?;
    no_attributes(root, &admin, &index)?;
    after_screen();
    let mut tree = BTreeMap::new();
    let mut total = 0usize;
    for entry in &index.entries {
        let path = plain_path(root, &entry.path)?;
        let (bytes, meta) = read_file(&path)?;
        total = total.checked_add(bytes.len())?;
        if total > 4 * MAX_BYTES {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if (meta.permissions().mode() & 0o111 != 0) != (entry.mode == 0o100755) {
                return None;
            }
        }
        #[cfg(not(unix))]
        let _ = meta;
        // Grit implements Git's `blob <len>\0` framing and SHA-1/SHA-256.
        // Never use a Git worktree hash/diff API here: those apply conversions.
        let oid =
            grit_lib::pack::hash_object_bytes(ObjectKind::Blob, &bytes, index.hash_algo.len())
                .ok()?;
        if oid != entry.oid.as_bytes() {
            return None;
        }
        if tree.insert(entry.path.clone(), (entry.mode, oid)).is_some() {
            return None;
        }
    }
    // Ignore traversal is also in-process: no fsmonitor, filter, hook or helper.
    let dot_git = root.join(".git");
    let walk = ignore::WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .parents(false)
        .git_global(false)
        .follow_links(false)
        .filter_entry(move |e| e.path() != dot_git)
        .build();
    for (count, entry) in walk.enumerate() {
        let entry = entry.ok()?;
        if count > MAX_ENTRIES || entry.error().is_some() {
            return None;
        }
        if entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry
            .path()
            .strip_prefix(root)
            .ok()?
            .to_str()?
            .replace(std::path::MAIN_SEPARATOR, "/");
        if !tree.contains_key(name.as_bytes()) {
            return None;
        }
    }
    // Rechecks protect witness freshness; no-exec safety does NOT depend on
    // them. Even a config/attribute swap after these checks cannot run a child.
    no_attributes(root, &admin, &index)?;
    config(&admin)?;
    if read_file(&admin.join("index"))?.0 != before {
        return None;
    }
    Tree(tree).content_id().ok()
}

pub(super) fn index_is_head(root: &Path, read: &crate::Scope<String>) -> bool {
    head::index_is_head(root, read)
}

#[path = "publish_early_head.rs"]
mod head;

#[cfg(test)]
#[path = "publish_early_tree_tests.rs"]
mod tests;

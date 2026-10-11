//! Bounded snapshots reuse the authorized no-follow receipt reader and Git discovery.
use super::*;
use content_addressable::RawContentId;
use std::io::Read;

#[derive(Debug, Serialize)]
pub(super) struct Snapshot(BTreeMap<String, Entry>);
#[derive(Debug, Serialize, PartialEq)]
struct Entry {
    file: File,
    enumerated: bool,
}
#[derive(Debug, Serialize, PartialEq)]
enum File {
    Absent,
    Present {
        bytes: RawContentId,
        lf: Option<usize>,
    },
    Unverified,
}
impl ContentAddressable for Snapshot {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

impl Snapshot {
    /// Incomplete/unauthorized reads never witness a check's input content.
    pub(super) fn witness(&self) -> Option<ContentId> {
        if self.0.values().any(|entry| entry.file == File::Unverified) {
            return None;
        }
        self.content_id().ok()
    }
}

fn read(scope: &Scope<String>, path: &Path, remaining: &mut usize) -> File {
    use crate::agentic::tools::file_capture::{is_absent_leaf, open_for_scope};
    match open_for_scope(scope, path, true) {
        Ok(file) => read_contents(file, remaining).unwrap_or(File::Unverified),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && is_absent_leaf(path) => {
            File::Absent
        }
        Err(_) => File::Unverified,
    }
}

fn read_contents(mut file: std::fs::File, remaining: &mut usize) -> Option<File> {
    let before = file.metadata().ok()?;
    let limit = (*remaining).min(2 * 1024 * 1024);
    if before.len() > limit as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    *remaining = remaining.saturating_sub(bytes.len());
    let after = file.metadata().ok()?;
    if bytes.len() > limit
        || before.len() != after.len()
        || after.len() != bytes.len() as u64
        || before.modified().ok() != after.modified().ok()
    {
        return None;
    }
    Some(File::Present {
        bytes: RawContentId::from_content(&bytes),
        lf: (!bytes.contains(&0)).then(|| bytes.iter().filter(|b| **b == b'\n').count()),
    })
}

pub(super) fn snapshot(root: &Path, scope: &Scope<String>) -> Option<Snapshot> {
    snapshot_with_baseline(root, scope, None)
}

pub(super) fn snapshot_with_baseline(
    root: &Path,
    scope: &Scope<String>,
    baseline: Option<&Snapshot>,
) -> Option<Snapshot> {
    // metadata_git refuses partial metadata authority; file reads independently
    // enforce their scope. Never discover files above the selected root.
    if !crate::agentic::claim_check::is_workspace_repo_root(&root.to_string_lossy(), scope) {
        return None;
    }
    let output = crate::git_hardening::metadata_git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        scope,
    )
    .ok()?
    .output()
    .ok()?;
    if !output.status.success() || output.stdout.len() > 1024 * 1024 {
        return None;
    }
    let paths = std::str::from_utf8(&output.stdout).ok()?;
    let mut files = BTreeMap::new();
    let mut remaining = 32 * 1024 * 1024;
    for path in paths.split('\0').filter(|p| !p.is_empty()) {
        if files.contains_key(path) {
            continue;
        }
        if files.len() >= 4096
            || Path::new(path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        files.insert(
            path.into(),
            Entry {
                file: read(scope, &root.join(path), &mut remaining),
                enumerated: true,
            },
        );
    }
    // A Git listing is discovery, not proof of absence. Reopen baseline paths
    // omitted today through exactly the same scoped, no-follow reader. These
    // postimages share the snapshot's file/byte limits and canonical identity.
    if let Some(baseline) = baseline {
        for path in baseline.0.keys() {
            if files.contains_key(path) {
                continue;
            }
            if files.len() >= 4096 {
                return None;
            }
            files.insert(
                path.clone(),
                Entry {
                    file: read(scope, &root.join(path), &mut remaining),
                    enumerated: false,
                },
            );
        }
    }
    Some(Snapshot(files))
}

fn count(entry: Option<&Entry>, baseline: bool) -> String {
    match entry {
        None if baseline => "unverified (not enumerated at baseline)".into(),
        None
        | Some(Entry {
            file: File::Unverified,
            ..
        }) => "unverified".into(),
        Some(Entry {
            file: File::Absent, ..
        }) => "0 (absent)".into(),
        Some(Entry {
            file: File::Present { lf, .. },
            enumerated,
        }) => {
            let count = lf.map_or_else(|| "unavailable".into(), |n| n.to_string());
            if *enumerated {
                count
            } else {
                format!("{count} (present, not enumerated (ignored/untracked transition))")
            }
        }
    }
}

pub(super) fn changes(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let keys: std::collections::BTreeSet<_> = before.0.keys().chain(after.0.keys()).collect();
    keys.into_iter()
        .filter_map(|path| {
            let old = before.0.get(path);
            let new = after.0.get(path);
            let unknown = [old, new].iter().flatten().any(|entry| {
                matches!(
                    entry.file,
                    File::Unverified | File::Present { lf: None, .. }
                )
            });
            (old != new || unknown).then(|| {
                format!(
                    "{}: {} → {}",
                    literal(path),
                    count(old, true),
                    count(new, false)
                )
            })
        })
        .collect()
}

pub(super) fn ambiguous(snapshot: &Snapshot, prose: &str) -> Vec<String> {
    let mut names = BTreeMap::<&str, usize>::new();
    for (path, entry) in &snapshot.0 {
        if matches!(entry.file, File::Absent) {
            continue;
        }
        *names
            .entry(path.rsplit('/').next().unwrap_or(path))
            .or_default() += 1;
    }
    names
        .into_iter()
        .filter(|(name, count)| {
            *count > 1
                && prose
                    .split(|c: char| c.is_whitespace() || "`*(),;:[]".contains(c))
                    .any(|t| t.trim_end_matches('.') == *name)
        })
        .map(|(name, _)| literal(name))
        .take(16)
        .collect()
}

impl Snapshot {
    pub(super) fn lines(&self, path: &str) -> Option<i64> {
        match &self.0.get(path)?.file {
            File::Absent => Some(0),
            File::Present { lf: Some(n), .. } => i64::try_from(*n).ok(),
            _ => None,
        }
    }
    pub(super) fn same_paths(&self, other: &Self) -> bool {
        self.0.keys().eq(other.0.keys())
    }
    pub(super) fn total_lines(&self) -> Option<i64> {
        self.0
            .keys()
            .try_fold(0_i64, |sum, path| sum.checked_add(self.lines(path)?))
    }
}

pub(super) fn resolve(
    before: Option<&Snapshot>,
    after: Option<&Snapshot>,
    name: &str,
) -> Option<String> {
    let paths: std::collections::BTreeSet<_> = before
        .into_iter()
        .chain(after)
        .flat_map(|s| s.0.keys())
        .filter(|path| path.as_str() == name || path.ends_with(&format!("/{name}")))
        .collect();
    (paths.len() == 1).then(|| (*paths.into_iter().next().expect("one path")).clone())
}

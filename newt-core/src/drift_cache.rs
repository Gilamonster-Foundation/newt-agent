//! Content-hash drift-cache (#1282, epic #1277) — the persistent, rebuildable
//! project index. The **Rust realization of `formal/ProjectModel/Basic.lean`**:
//! cache each unit's derived value keyed by its content hash; on re-scan,
//! re-derive only the units whose hash **drifted** and reuse the cached value
//! for the rest. Two properties, both proved in the Lean and pinned by test
//! here:
//!
//! - **PO-B (drift completeness / no false-clean):** a unit is `changed` iff its
//!   content hash differs. A value change implies a content change implies a
//!   hash change (blake3 collision-resistance), so a changed unit is *always*
//!   reported — the cache can never serve a stale value. The hash is the
//!   unit's [`RawContentId`]: identity is the content alone, never size or
//!   mtime (#3027).
//! - **PO-C (incremental ≡ full rebuild):** [`apply_drift`] equals [`rebuild`]
//!   for every edit (modify / add / delete). The drift-cache is IDE speed with a
//!   machine-checked guarantee it never diverges from an honest fresh scan —
//!   *freshness by verification*.
//!
//! Generic over the cached value `T`, so it serves the project model, the symbol
//! table, or the embedding vectors alike. The `version` (derive/chunker/
//! embedding-model id) forces a clean rebuild on a bump. Pure core +
//! [`load`]/[`save`] fs wrappers over a verify-on-read
//! [`FrameStore`](agent_harness::store::FrameStore) (fail-soft: a corrupt cache
//! rebuilds, never blocks).

use content_addressable::{ContentAddressable, ContentError, ContentId, RawContentId};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The drift key: the unit's [`RawContentId`]. Collision-resistance is what
/// discharges PO-B (hash-equality ⇒ content-equality).
#[must_use]
pub fn content_hash(bytes: &[u8]) -> RawContentId {
    RawContentId::from_content(bytes)
}

/// One cached unit: its content hash + the derived value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEntry<T> {
    pub hash: RawContentId,
    pub value: T,
}

/// The drift-cache: unit-id → `(content hash, derived value)`, plus the store
/// `version` (a bump — new derive/chunker/embedding-model — forces a rebuild).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftCache<T> {
    pub version: String,
    pub entries: BTreeMap<String, CacheEntry<T>>,
}

impl<T> DriftCache<T> {
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            entries: BTreeMap::new(),
        }
    }
}

impl<T> Default for DriftCache<T> {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// The stored cache is a node addressed by its own canonical dag-cbor form.
impl<T: Serialize> ContentAddressable for DriftCache<T> {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

/// What drifted between the cache and the current scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftSet {
    /// Units new or whose content hash changed (need re-derive).
    pub changed: Vec<String>,
    /// Units in the cache but absent now (evicted).
    pub removed: Vec<String>,
}

impl DriftSet {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }
}

/// **Pure — PO-B.** The drift between a cache and the current unit hashes.
/// `changed` = units whose hash differs from the cache (new or modified);
/// `removed` = cached units absent now. A unit whose derived value would change
/// has a different content hash, so it is always in `changed` — no false-clean.
#[must_use]
pub fn drift<T>(cache: &DriftCache<T>, current: &BTreeMap<String, RawContentId>) -> DriftSet {
    let changed = current
        .iter()
        .filter(|(unit, hash)| cache.entries.get(*unit).map(|e| &e.hash) != Some(hash))
        .map(|(unit, _)| unit.clone())
        .collect();
    let removed = cache
        .entries
        .keys()
        .filter(|u| !current.contains_key(*u))
        .cloned()
        .collect();
    DriftSet { changed, removed }
}

/// **Pure — PO-C.** Apply drift: build the next cache by re-deriving only the
/// units whose hash changed and reusing the cached value for clean ones
/// (dropping removed). `derive(unit, hash)` produces a (re-)derived value.
/// Provably equals [`rebuild`] (the Lean theorem `po_c_incremental_eq_full`).
#[must_use]
pub fn apply_drift<T: Clone>(
    prev: &DriftCache<T>,
    current: &BTreeMap<String, RawContentId>,
    derive: impl Fn(&str, &RawContentId) -> T,
) -> DriftCache<T> {
    let entries = current
        .iter()
        .map(|(unit, hash)| {
            let value = match prev.entries.get(unit) {
                Some(e) if &e.hash == hash => e.value.clone(),
                _ => derive(unit, hash),
            };
            (unit.clone(), CacheEntry { hash: *hash, value })
        })
        .collect();
    DriftCache {
        version: prev.version.clone(),
        entries,
    }
}

/// **Pure.** A from-scratch rebuild (derive every unit) — the reference
/// [`apply_drift`] is proved equal to (PO-C).
#[must_use]
pub fn rebuild<T>(
    version: &str,
    current: &BTreeMap<String, RawContentId>,
    derive: impl Fn(&str, &RawContentId) -> T,
) -> DriftCache<T> {
    let entries = current
        .iter()
        .map(|(unit, hash)| {
            (
                unit.clone(),
                CacheEntry {
                    hash: *hash,
                    value: derive(unit, hash),
                },
            )
        })
        .collect();
    DriftCache {
        version: version.to_string(),
        entries,
    }
}

/// A repo's index dir: `<config dir>/index/<RawContentId of the canonical
/// root>/` (config-lean drop-in dir). The first time a repo's dir is resolved,
/// the pre-#3027 JSON caches are swept.
#[must_use]
pub fn store_dir(config_dir: &Path, repo_root: &Path) -> PathBuf {
    let root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let index = config_dir.join("index");
    let dir = index.join(content_hash(root.to_string_lossy().as_bytes()).to_string());
    if !dir.exists() {
        remove_legacy_json(&index);
    }
    dir
}

/// Best-effort, no importer: delete `index/*/project-model.*.json` (and their
/// `.json.tmp`) from the hand-rolled era. A failure never fails the scan.
fn remove_legacy_json(index: &Path) {
    let Ok(repos) = std::fs::read_dir(index) else {
        return;
    };
    for repo in repos.flatten().map(|e| e.path()) {
        let Ok(files) = std::fs::read_dir(&repo) else {
            continue;
        };
        for file in files.flatten().map(|e| e.path()) {
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned());
            if name.is_some_and(|n| {
                n.starts_with("project-model.")
                    && (n.ends_with(".json") || n.ends_with(".json.tmp"))
            }) {
                let _ = std::fs::remove_file(file);
            }
        }
        let _ = std::fs::remove_dir(&repo); // only succeeds when now empty
    }
}

/// How many times, and how far apart, [`load`] reads a head that a concurrent
/// [`save`] is replacing: at most ~0.1 s before a still-unreadable head is no cache.
const HEAD_READ_ATTEMPTS: u32 = 20;
const HEAD_READ_RETRY: std::time::Duration = std::time::Duration::from_millis(5);

fn head_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.head"))
}

/// Load cache `name` from the store at `dir`: `<name>.head` names a
/// [`ContentId`]; the node is verified on read. `None` on a missing or garbage
/// head, a missing / tampered / truncated node, OR a `version` other than
/// `expected_version` (a bump ⇒ a clean rebuild). A node that fails
/// verification is removed so the next [`save`] can rewrite its address.
#[must_use]
pub fn load<T: Serialize + DeserializeOwned>(
    dir: &Path,
    name: &str,
    expected_version: &str,
) -> Option<DriftCache<T>> {
    let head = read_head(|| std::fs::read_to_string(head_path(dir, name)))?;
    let id: ContentId = head.trim().parse().ok()?;
    let store = agent_harness::store::FrameStore::open(dir).ok()?;
    match store.get::<DriftCache<T>>(&id) {
        Ok(cache) => (cache.version == expected_version).then_some(cache),
        Err(_) => {
            let _ = std::fs::remove_file(dir.join(format!("{id}.cbor")));
            None
        }
    }
}

/// Read a head through `read`, riding out a concurrent [`save`] replacing it.
/// On Windows that rename (`MoveFileExW`) can fail a reader for an instant with
/// `ERROR_ACCESS_DENIED` (the old head is delete-pending, `PermissionDenied`) or
/// `ERROR_SHARING_VIOLATION` (32); taken as "no cache", it cost a rebuild. Any
/// other error is no cache at once, and a head that stays unreadable is too.
fn read_head(mut read: impl FnMut() -> std::io::Result<String>) -> Option<String> {
    for _ in 1..HEAD_READ_ATTEMPTS {
        match read() {
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    || (cfg!(windows) && e.raw_os_error() == Some(32)) =>
            {
                std::thread::sleep(HEAD_READ_RETRY);
            }
            result => return result.ok(),
        }
    }
    read().ok()
}

/// Save the cache as a write-once node, then point `<name>.head` at it through
/// a uniquely named temp file + atomic rename, so concurrent sessions never
/// share a temp path and a reader sees one whole head or the other.
/// Best-effort: a failure never perturbs the session (the next run rebuilds).
pub fn save<T: Serialize>(dir: &Path, name: &str, cache: &DriftCache<T>) {
    let publish = || -> Option<()> {
        let id = agent_harness::store::FrameStore::open(dir)
            .ok()?
            .put(cache)
            .ok()?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).ok()?;
        writeln!(tmp, "{id}").ok()?;
        tmp.persist(head_path(dir, name)).ok()?;
        Some(())
    };
    let _ = publish();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn cache(version: &str, kv: &[(&str, &str, &str)]) -> DriftCache<String> {
        let mut c = DriftCache::new(version);
        for (u, h, v) in kv {
            c.entries.insert(
                (*u).to_string(),
                CacheEntry {
                    hash: content_hash(h.as_bytes()),
                    value: (*v).to_string(),
                },
            );
        }
        c
    }

    fn hashes(kv: &[(&str, &str)]) -> BTreeMap<String, RawContentId> {
        kv.iter()
            .map(|(u, h)| (u.to_string(), content_hash(h.as_bytes())))
            .collect()
    }

    /// Windows CI: `concurrent_saves_never_tear_the_head` saw no cache when a
    /// reader opened the head while a concurrent save replaced it. A head being
    /// replaced is re-read, a bounded number of times; any other error is no
    /// cache at once.
    #[test]
    fn head_read_rides_out_a_concurrent_replace_and_nothing_else() {
        let calls = std::cell::Cell::new(0u32);
        let read = |error: fn() -> std::io::Error, failures: u32| {
            calls.set(0);
            read_head(|| {
                calls.set(calls.get() + 1);
                if calls.get() <= failures {
                    Err(error())
                } else {
                    Ok("head".to_string())
                }
            })
        };
        let denied = || std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(read(denied, 2).as_deref(), Some("head"));
        assert_eq!(calls.get(), 3, "re-read until the replace is done");
        assert_eq!(read(denied, u32::MAX), None);
        assert_eq!(calls.get(), HEAD_READ_ATTEMPTS, "bounded");
        assert_eq!(read(|| std::io::ErrorKind::NotFound.into(), u32::MAX), None);
        assert_eq!(calls.get(), 1, "a missing head is not retried");
        #[cfg(windows)]
        {
            let sharing = || std::io::Error::from_raw_os_error(32);
            assert_eq!(read(sharing, 1).as_deref(), Some("head"));
            assert_eq!(calls.get(), 2, "a sharing violation is a replace too");
        }
    }

    #[test]
    fn drift_reports_changed_new_and_removed_no_false_clean() {
        // PO-B: a same-hash unit is clean; a different-hash unit is changed
        // (a value change ⇒ content change ⇒ hash change — never a false-clean);
        // a new unit is changed; a vanished unit is removed.
        let c = cache(
            "v1",
            &[("a", "h1", "A"), ("b", "h2", "B"), ("gone", "h9", "G")],
        );
        let d = drift(&c, &hashes(&[("a", "h1"), ("b", "h2_NEW"), ("c", "h3")]));
        let mut changed = d.changed.clone();
        changed.sort();
        assert_eq!(changed, vec!["b".to_string(), "c".to_string()]);
        assert_eq!(d.removed, vec!["gone".to_string()]);
        // Fully-unchanged tree ⇒ empty drift.
        assert!(drift(&c, &hashes(&[("a", "h1"), ("b", "h2"), ("gone", "h9")])).is_empty());
    }

    #[test]
    fn apply_drift_equals_full_rebuild_for_every_edit() {
        // PO-C: the drift-updated cache == a from-scratch rebuild, for modify /
        // add / delete. `derive` stamps "D:" so reuse vs re-derive is visible.
        let prev = cache("v1", &[("a", "h1", "A"), ("b", "h2", "B")]);
        let derive = |u: &str, _h: &RawContentId| format!("D:{u}");
        for current in [
            hashes(&[("a", "h1"), ("b", "h2_NEW")]),          // modify b
            hashes(&[("a", "h1"), ("b", "h2"), ("c", "h3")]), // add c
            hashes(&[("a", "h1")]),                           // delete b
            hashes(&[("x", "hx")]),                           // total churn
        ] {
            let incremental = apply_drift(&prev, &current, derive);
            let full = rebuild(&prev.version, &current, derive);
            // The theorem: same entries + hashes. (Values differ only where the
            // incremental path REUSED a cached value it was entitled to.)
            assert_eq!(
                incremental.entries.keys().collect::<Vec<_>>(),
                full.entries.keys().collect::<Vec<_>>()
            );
            for (u, e) in &incremental.entries {
                assert_eq!(&e.hash, &full.entries[u].hash);
                // A clean carried-over unit keeps its cached value; a re-derived
                // one matches the fresh derive — either way, model-equal.
                let clean = prev.entries.get(u).map(|p| &p.hash) == Some(&e.hash);
                if clean {
                    assert_eq!(&e.value, &prev.entries[u].value);
                } else {
                    assert_eq!(&e.value, &full.entries[u].value);
                }
            }
        }
    }

    #[test]
    fn apply_drift_only_rederives_changed_units() {
        // The efficiency claim: derive is called exactly for the changed units,
        // never for the clean ones (this is what makes a re-scan fast).
        let prev = cache(
            "v1",
            &[("a", "h1", "A"), ("b", "h2", "B"), ("c", "h3", "C")],
        );
        let calls = RefCell::new(Vec::new());
        let derive = |u: &str, _h: &RawContentId| {
            calls.borrow_mut().push(u.to_string());
            format!("D:{u}")
        };
        // Only b changed; a and c are clean.
        let _ = apply_drift(
            &prev,
            &hashes(&[("a", "h1"), ("b", "h2_NEW"), ("c", "h3")]),
            derive,
        );
        assert_eq!(
            *calls.borrow(),
            vec!["b".to_string()],
            "only the drifted unit re-derives"
        );
    }
}

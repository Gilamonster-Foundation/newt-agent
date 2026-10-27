//! #3027: the drift-cache persisted as a verify-on-read content-addressed
//! node behind an atomic head. Every case is judged against an honest fresh
//! [`scan_project`] — the PO-C reference.

use newt_core::drift_cache::{self, DriftCache};
use newt_core::project_model::{
    builtin_project_packs, scan_project, scan_project_cached, ProjectModel, ProjectUnit,
};
use std::path::{Path, PathBuf};

const VERSION: &str = "project-model.v1";
const NAME: &str = "project-model.rust";

/// A two-member Rust workspace whose unit names are easy to find in bytes.
fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let p = root.path();
    std::fs::write(
        p.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\n",
    )
    .unwrap();
    for (dir, name) in [("a", "alpha"), ("b", "bravo")] {
        std::fs::create_dir_all(p.join(dir)).unwrap();
        std::fs::write(
            p.join(dir).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\n"),
        )
        .unwrap();
    }
    root
}

fn cached(root: &Path, cfg: &Path) -> ProjectModel {
    scan_project_cached(root, &builtin_project_packs(), cfg).unwrap()
}

fn fresh(root: &Path) -> ProjectModel {
    scan_project(root, &builtin_project_packs()).unwrap()
}

fn head(root: &Path, cfg: &Path) -> PathBuf {
    drift_cache::store_dir(cfg, root).join(format!("{NAME}.head"))
}

fn blob(root: &Path, cfg: &Path) -> PathBuf {
    let id = std::fs::read_to_string(head(root, cfg)).unwrap();
    drift_cache::store_dir(cfg, root).join(format!("{}.cbor", id.trim()))
}

fn load(root: &Path, cfg: &Path) -> Option<DriftCache<ProjectUnit>> {
    drift_cache::load(&drift_cache::store_dir(cfg, root), NAME, VERSION)
}

/// A flipped byte that still DECODES (a unit name rewritten in place), a
/// truncated node, and a garbage head each rebuild to exactly a fresh scan —
/// and the rebuild heals the store so the next start loads again. The flipped
/// name is the case only verify-on-read catches: the entry hashes still
/// match, so an unverified decode would serve "alphZ" as clean.
#[test]
fn corrupted_store_rebuilds_equal_to_a_fresh_scan() {
    type Corrupt = fn(&Path, &Path);
    let cases: [(&str, Corrupt); 3] = [
        ("flipped byte", |root, cfg| {
            let path = blob(root, cfg);
            let mut bytes = std::fs::read(&path).unwrap();
            let at = bytes.windows(5).position(|w| w == b"alpha").unwrap();
            bytes[at + 4] = b'Z';
            std::fs::write(path, bytes).unwrap();
        }),
        ("truncated node", |root, cfg| {
            let path = blob(root, cfg);
            let bytes = std::fs::read(&path).unwrap();
            std::fs::write(path, &bytes[..bytes.len() / 2]).unwrap();
        }),
        ("garbage head", |root, cfg| {
            std::fs::write(head(root, cfg), "not a content id\n").unwrap();
        }),
    ];
    for (label, corrupt) in cases {
        let root = workspace();
        let cfg = tempfile::tempdir().unwrap();
        let (root, cfg) = (root.path(), cfg.path());
        assert_eq!(cached(root, cfg).units, fresh(root).units, "{label}: cold");
        corrupt(root, cfg);
        assert_eq!(load(root, cfg), None, "{label}: corruption must not load");
        assert_eq!(
            cached(root, cfg).units,
            fresh(root).units,
            "{label}: rebuild"
        );
        assert!(
            load(root, cfg).is_some(),
            "{label}: the rebuild heals the store"
        );
    }
}

/// Two writers racing on one cache name, with a reader watching: every load
/// sees one whole cache or the other, and one valid head survives. The
/// pre-#3027 save shared a single `<name>.json.tmp`, so a writer could
/// truncate and rewrite the very inode the other had just renamed live.
#[test]
fn concurrent_saves_never_tear_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let make = |n: usize, tag: &str| {
        let mut c = DriftCache::<String>::new("v1");
        c.entries = (0..n)
            .map(|i| {
                let unit = format!("u{i:05}");
                let entry = drift_cache::CacheEntry {
                    hash: drift_cache::content_hash(unit.as_bytes()),
                    value: format!("{tag}{}", "x".repeat(64)),
                };
                (unit, entry)
            })
            .collect();
        c
    };
    let (big, small) = (make(2_000, "big"), make(3, "small"));
    drift_cache::save(dir, "race", &big);
    let done = std::sync::atomic::AtomicBool::new(false);
    let torn = std::thread::scope(|s| {
        let writers: Vec<_> = [&big, &small, &big, &small]
            .into_iter()
            .map(|cache| {
                s.spawn(move || {
                    for _ in 0..60 {
                        drift_cache::save(dir, "race", cache);
                    }
                })
            })
            .collect();
        let reader = s.spawn(|| {
            let mut torn = 0;
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let got = drift_cache::load::<String>(dir, "race", "v1");
                torn += usize::from(got.as_ref() != Some(&big) && got.as_ref() != Some(&small));
            }
            torn
        });
        for writer in writers {
            writer.join().unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().unwrap()
    });
    assert_eq!(torn, 0, "a reader saw a torn or missing cache");
    let last = drift_cache::load::<String>(dir, "race", "v1");
    assert!(last == Some(big) || last == Some(small));
}

/// Identity is the content, never metadata: a marker rewritten to the same
/// length with its mtime restored is still re-derived.
#[test]
fn same_size_same_mtime_content_change_is_detected() {
    let root = workspace();
    let cfg = tempfile::tempdir().unwrap();
    let (root, cfg) = (root.path(), cfg.path());
    let _ = cached(root, cfg);
    let marker = root.join("a").join("Cargo.toml");
    let before = std::fs::metadata(&marker).unwrap();
    std::fs::write(&marker, "[package]\nname = \"alphZ\"\n").unwrap();
    let file = std::fs::File::options().write(true).open(&marker).unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
    drop(file);
    let after = std::fs::metadata(&marker).unwrap();
    assert_eq!(
        (after.len(), after.modified().unwrap()),
        (before.len(), before.modified().unwrap())
    );
    let model = cached(root, cfg);
    assert!(model.units.iter().any(|u| u.name == "alphZ"), "{model:?}");
    assert_eq!(model.units, fresh(root).units);
}

/// Pre-#3027 JSON caches are deleted, never imported. The legacy file here is
/// one the old loader WOULD have served: right version, right per-unit
/// blake3 hex, poisoned value.
#[test]
fn legacy_json_cache_is_removed_and_never_read() {
    let root = workspace();
    let cfg = tempfile::tempdir().unwrap();
    let (root, cfg) = (root.path(), cfg.path());
    let repo = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
    let legacy_dir = cfg.join("index").join(&repo[..16]);
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let entries: serde_json::Map<String, serde_json::Value> = fresh(root)
        .units
        .into_iter()
        .map(|mut unit| {
            let text = std::fs::read_to_string(root.join(&unit.dir).join("Cargo.toml")).unwrap();
            let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
            unit.name = "POISON".into();
            let entry = serde_json::json!({"hash": hash, "value": unit});
            (unit.dir.clone(), entry)
        })
        .collect();
    let legacy = serde_json::json!({"version": VERSION, "entries": entries});
    let json = legacy_dir.join(format!("{NAME}.json"));
    let tmp = legacy_dir.join(format!("{NAME}.json.tmp"));
    std::fs::write(&json, legacy.to_string()).unwrap();
    std::fs::write(&tmp, "torn").unwrap();

    assert_eq!(cached(root, cfg).units, fresh(root).units);
    assert!(
        !json.exists() && !tmp.exists(),
        "legacy files must be deleted"
    );
}

/// A saved cache loads back equal; a version bump or another name loads
/// nothing (a clean rebuild).
#[test]
fn store_round_trips_and_rejects_a_version_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = DriftCache::<String>::new("v1");
    c.entries.insert(
        "a".into(),
        drift_cache::CacheEntry {
            hash: drift_cache::content_hash(b"a"),
            value: "A".into(),
        },
    );
    drift_cache::save(dir.path(), "model", &c);
    assert_eq!(
        drift_cache::load::<String>(dir.path(), "model", "v1"),
        Some(c)
    );
    assert_eq!(drift_cache::load::<String>(dir.path(), "model", "v2"), None);
    assert_eq!(drift_cache::load::<String>(dir.path(), "other", "v1"), None);
}

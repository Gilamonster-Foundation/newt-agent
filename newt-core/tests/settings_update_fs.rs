//! Real-filesystem tier for `settings::update_at` (#3090). Grounds the pure
//! `apply_project_choice` unit tests: they cannot see two processes racing
//! on the one shared `~/.newt/settings.toml`.

use newt_core::settings::{update_at, Choice, Session};
use std::sync::{Arc, Barrier};

/// Regression for #3090: `update()` was an unlocked read-modify-write, so
/// helpers switching `/model` at the same moment overwrote each other's
/// project entries. Every concurrent write must persist.
#[test]
fn concurrent_updates_for_different_projects_all_persist() {
    const N: usize = 4;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Arc::new(dir.path().join("settings.toml"));
    let barrier = Arc::new(Barrier::new(N));
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let (path, barrier) = (Arc::clone(&path), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                update_at(&path, &format!("/p{i}"), Choice::Model("m"), Some("dflt"))
                    .expect("update");
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread");
    }
    let doc: toml::Table =
        toml::from_str(&std::fs::read_to_string(&*path).expect("read")).expect("parse");
    for i in 0..N {
        assert_eq!(
            Session::for_project(&doc, &format!("/p{i}"))
                .model
                .as_deref(),
            Some("m"),
            "project /p{i} lost"
        );
    }
}

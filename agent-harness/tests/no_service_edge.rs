//! The reusable harness has no network, inference, async, or terminal edge.
//!
//! These Cargo subprocess checks ground that boundary in the resolved normal
//! and build dependency graph, following `agent-frame`'s closure guard.
//! The same walker must find a known edge in `newt-core`: an empty result must
//! not make an unread dependency graph look safe.

use std::collections::BTreeSet;
use std::process::Command;

const FORBIDDEN: &[&str] = &[
    "reqwest",
    "hyper",
    "axum",
    "tonic",
    "tokio",
    "candle-core",
    "candle-nn",
    "candle-transformers",
    "tokenizers",
    "ureq",
    "curl",
    "ratatui",
    "crossterm",
];

/// The transitive shipped closure of `-p pkg`, by crate name.
/// Build dependencies count because their code executes during compilation.
fn closure(pkg: &str) -> BTreeSet<String> {
    let out = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
            "-p",
            pkg,
        ])
        .output()
        .expect("cargo tree runs");
    assert!(
        out.status.success(),
        "cargo tree failed for {pkg}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| !n.starts_with('['))
        .map(str::to_string)
        .collect()
}

#[test]
fn the_harness_has_no_service_or_terminal_edge() {
    let shipped = closure("agent-harness");
    let found: Vec<&&str> = FORBIDDEN.iter().filter(|f| shipped.contains(**f)).collect();
    assert!(
        found.is_empty(),
        "agent-harness resolves {found:?}. Network, inference, async, and terminal \
         dependencies belong in a consumer, not the reusable harness."
    );
}

/// **ANTI-VACUOUS TWIN.** The same walker must find an edge known to exist.
/// A walker that always returns empty must make this test fail.
#[test]
fn the_walker_sees_edges_that_do_exist() {
    let hub = closure("newt-core");
    assert!(
        hub.contains("tokio"),
        "the walker found no `tokio` in newt-core's closure, so it is not reading the \
         dependency graph and the assertion above is vacuous. Closure size: {}",
        hub.len()
    );
}

#[test]
fn the_harness_stays_small() {
    // MEASURED: 51 normal/build crates on 2026-09-10, plus 3 of headroom.
    // Lower this when the number shrinks. Never raise it to make a red test
    // pass -- a closure that grew is a finding, not a number to edit.
    const CEILING: usize = 54;
    let n = closure("agent-harness").len();
    assert!(
        n <= CEILING,
        "agent-harness resolves {n} crates, over its {CEILING} ceiling. The reusable \
         harness must not acquire a consumer's dependency closure. Lower the ceiling \
         when it shrinks; never raise it to make this pass."
    );
}

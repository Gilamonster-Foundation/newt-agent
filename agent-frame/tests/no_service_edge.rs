//! **Invariant 7.4, static half: this crate has no service edge.**
//!
//! v0 is a library. The design admits it may one day sit behind a daemon, and
//! that option stays open only while the kernel itself cannot reach the
//! network or a model. An HTTP or inference dependency arriving here would end
//! that quietly — a `reqwest` in the tree is not a compile error, it is a
//! design decision nobody voted on.
//!
//! Asserted rather than documented, because a comment claiming "no HTTP" goes
//! stale the first time someone adds a convenience dependency.
//!
//! # The anti-vacuous twin
//!
//! Every assertion here has the shape "this list does not contain X", which is
//! exactly what a reader that reads NOTHING satisfies. So the same walker is
//! also pointed at a crate whose closure is known to contain those names, and
//! must come back finding them. Deleting the body of `closure` must turn this
//! file RED.

use std::collections::BTreeSet;
use std::process::Command;

/// Names that would mean this crate can reach the network or a model.
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
];

/// The transitive shipped closure of `-p pkg`, by crate name.
///
/// Normal **and** build dependencies count: a build script runs with the full
/// authority of the building user before a line of this crate compiles, so a
/// dependency arriving through `[build-dependencies]` is the guard's whole
/// subject coming in another door.
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
fn the_kernel_has_no_http_or_inference_edge() {
    let shipped = closure("agent-frame");
    let found: Vec<&&str> = FORBIDDEN.iter().filter(|f| shipped.contains(**f)).collect();
    assert!(
        found.is_empty(),
        "agent-frame resolves {found:?}. The kernel is a library of pure functions and \
         must not be able to reach the network or a model. If the dependency is real, \
         it belongs in a consumer, not here."
    );
}

/// **ANTI-VACUOUS TWIN.** The same walker, pointed at a crate that certainly
/// does have these edges, must come back naming them.
///
/// An empty result from a walker that always returns empty proves nothing, and
/// "the list does not contain X" is precisely the claim a broken reader
/// satisfies for free.
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

/// The kernel's own closure is small. A leaf that quietly grows a hub's
/// closure has stopped being a leaf.
#[test]
fn the_kernel_stays_small() {
    // MEASURED, not guessed: 37 on 2026-09-08, plus 3 of slack so a third-party
    // graph moving underneath us does not turn this red on someone else's
    // release. The 37 are entirely mandatory: `content-addressable` and its
    // content-addressing stack (cid, multihash, multibase, blake3, ipld-core,
    // serde_ipld_dagcbor, data-encoding), plus serde and thiserror. There is no
    // HTTP, no tokio and no inference crate among them, which is the separate
    // and stronger claim `the_kernel_has_no_http_or_inference_edge` makes.
    //
    // Lower this when the number shrinks. Never raise it to make a red test
    // pass -- a closure that grew is a finding, not a number to edit.
    const CEILING: usize = 40;
    let n = closure("agent-frame").len();
    assert!(
        n <= CEILING,
        "agent-frame resolves {n} crates, over its {CEILING} ceiling. This crate is \
         incubated inside newt-agent's workspace and must not acquire newt-core's \
         closure (644 crates). Lower the ceiling when it shrinks; never raise it to \
         make this pass."
    );
}

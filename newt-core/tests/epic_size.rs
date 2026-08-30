//! **The epic's own acceptance criterion, measured** (F0d, #1929).
//!
//! Epic #1803's acceptance is *"the refactor ends with fewer production
//! paths, not old plus new"*. That is a claim about the tree, so it is
//! measured rather than asserted — and measured with the SAME scanner every
//! ratchet uses, not a second one written for the occasion.
//!
//! ## Why the shared scanner, and not a line count
//!
//! The naive heuristic — "code is everything before the first `#[cfg(test)]`"
//! — is badly wrong on this tree. `agentic/mod.rs` carries **19** separate
//! `#[cfg(test)]` regions interleaved with production code, the first at line
//! 216 of 20,301. Latching at the first one reports that file as ~216 code
//! lines. `common::for_each_production_line` skips gated items by BRACE
//! DEPTH and drops whole files that another file declares as `#[cfg(test)]
//! mod x;`, which is the only honest instrument available here.
//!
//! ## `#[ignore]`, deliberately
//!
//! It is a report, not a gate. Asserting a line count would fail on every
//! honest change; guessing at one is worse. Run it:
//!
//! ```text
//! cargo test -p newt-core --test epic_size -- --ignored --nocapture
//! NEWT_SIZE_ROOT=<path> …      # measure another checkout (a pre-epic rev)
//! NEWT_SIZE_NONBLANK=1 …       # exclude blank lines
//! NEWT_SIZE_CRATES=1 …         # per-crate breakdown
//! ```

mod common;
use common::{for_each_production_line, production_roots, workspace_root};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn no_extra_skips(_: &Path) -> bool {
    false
}

#[test]
#[ignore = "a report, not a gate: run with --ignored --nocapture"]
fn production_line_count() {
    let root: PathBuf =
        std::env::var("NEWT_SIZE_ROOT").map_or_else(|_| workspace_root(), PathBuf::from);
    let skip_blank = std::env::var("NEWT_SIZE_NONBLANK").is_ok();

    let mut total = 0usize;
    let mut per_crate: BTreeMap<String, usize> = BTreeMap::new();
    for_each_production_line(
        &production_roots(&root),
        &no_extra_skips,
        &mut |path, _, raw| {
            if skip_blank && raw.trim().is_empty() {
                return;
            }
            total += 1;
            let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy();
            let krate = rel.split('/').next().unwrap_or("?").to_string();
            *per_crate.entry(krate).or_default() += 1;
        },
    );

    println!("SIZE_ROOT {}", root.display());
    println!("SIZE_TOTAL {total}");
    if std::env::var("NEWT_SIZE_CRATES").is_ok() {
        for (krate, n) in &per_crate {
            println!("SIZE_CRATE {n:>7}  {krate}");
        }
    }
}

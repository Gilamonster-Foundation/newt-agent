//! **The epic's own acceptance criterion, measured** (F0d, #1929).
//!
//! Epic #1803's acceptance is *"the refactor ends with fewer production
//! paths, not old plus new"*. That is a claim about the tree, so it is
//! measured rather than asserted — and measured with the SAME scanner every
//! ratchet uses, not a second one written for the occasion.
//!
//! ## What it counts, precisely
//!
//! **Production, non-comment, non-blank source lines.** The shared scanner
//! skips `#[cfg(test)]` items by BRACE DEPTH, drops whole files another file
//! declares as `#[cfg(test)] mod x;`, and — the part easy to miss —
//! `continue`s on any line whose first non-space characters are `//`.
//!
//! That last rule matters for reading the number. `agentic/mod.rs` is 20,301
//! lines: 3,403 comment-only, 792 blank, 16,898 code. Of the code, 8,163 is
//! production and 8,735 sits inside `#[cfg(test)]`.
//!
//! ## A disagreement F0d recorded that turned out not to be one
//!
//! F0d (#1929) reported this file as "8,163 / 12,138" against #1929's
//! "10,600 / 9,702" and said it believed the scanner over the figure. Both
//! were right about DIFFERENT QUANTITIES, and F0d's framing was the error:
//!
//! * 10,600 + 9,702 = 20,302 — every line in the file assigned to a side,
//!   comments and blanks included.
//! * 8,163 is production CODE only, and the 12,138 F0d called "test" was
//!   everything else: test code, plus all 3,403 comments, plus all 792
//!   blanks.
//!
//! So the instrument was not disputing the figure; it was answering a
//! different question while labelled as though it answered that one. The
//! reconciliation needed no second brace-matcher — comment and blank lines
//! classify by inspection, and the scanner is still the only thing that
//! decides which code is production.
//!
//! ## Why the shared scanner
//!
//! The naive heuristic — "code is everything before the first `#[cfg(test)]`"
//! — is badly wrong here. `agentic/mod.rs` carries NINETEEN separate
//! `#[cfg(test)]` regions interleaved with production code, the first at line
//! 216. Latching at the first one reports that file as ~216 code lines.
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
//! NEWT_SIZE_FILES=1 …          # per-file: production, test, comment+blank
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
    let mut per_file: BTreeMap<String, usize> = BTreeMap::new();
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
            *per_file.entry(rel.into_owned()).or_default() += 1;
        },
    );

    println!("SIZE_ROOT {}", root.display());
    println!("SIZE_TOTAL {total}");
    // Per file: production code, then TEST code, then comments+blanks.
    //
    // Test code is DERIVED, not counted by a second scanner — a parallel
    // brace-matcher is exactly the duplicate this repo's reuse discipline
    // forbids, and the one place it would disagree is the one place it
    // matters. Comments and blanks classify by inspection; the scanner alone
    // decides which of the remaining code is production, and what is left
    // over is test.
    if std::env::var("NEWT_SIZE_FILES").is_ok() {
        for (rel, prod) in &per_file {
            let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
                continue;
            };
            let (mut comment, mut blank) = (0usize, 0usize);
            for line in text.lines() {
                let t = line.trim_start();
                if t.is_empty() {
                    blank += 1;
                } else if t.starts_with("//") {
                    comment += 1;
                }
            }
            let code = text.lines().count() - comment - blank;
            println!(
                "SIZE_FILE {prod:>6} {:>6} {:>6}  {rel}",
                code.saturating_sub(*prod),
                comment + blank
            );
        }
    }
    if std::env::var("NEWT_SIZE_CRATES").is_ok() {
        for (krate, n) in &per_crate {
            println!("SIZE_CRATE {n:>7}  {krate}");
        }
    }
}

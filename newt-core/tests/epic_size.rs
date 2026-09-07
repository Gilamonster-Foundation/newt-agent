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
//! lines: 3,403 comment-only, 792 blank, and 16,106 code. Of the code, 8,163
//! is production and the rest sits inside `#[cfg(test)]`.
//!
//! **The 16,898 this paragraph used to say was the defect, in prose** (#2151).
//! It was `lines - comments`, so it still had the 792 blanks inside it, and
//! 3,403 + 792 + 16,898 = 21,093 — 792 more than the file has. The report had
//! the same bug in code: blanks counted as production AND again under
//! "comment+blank". Both are fixed; the three columns now sum to the file.
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
//! `NEWT_SIZE_NONBLANK` is gone. It used to select between a correct and an
//! incorrect measurement, which made every report depend on whether the caller
//! remembered it; blanks are now excluded unconditionally. Existing
//! invocations that still set it are unaffected — an unread env var changes
//! nothing.
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

    let mut total = 0usize;
    let mut per_crate: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_file: BTreeMap<String, usize> = BTreeMap::new();
    for_each_production_line(
        &production_roots(&root),
        &no_extra_skips,
        &mut |path, _, raw| {
            // Blank lines are NOT production source. Counting them here while
            // the per-file report also counts them under "comment+blank" put
            // the same line in two columns, so the three columns summed to
            // more than the file (#2151). Unconditional, because a measurement
            // that is only correct when the caller remembers a flag is a trap.
            if raw.trim().is_empty() {
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
            let (code, comment_blank) = classify(&text);
            // NOT `saturating_sub`. The walker skips comments and (since
            // #2151) blanks, so `prod` is a subset of `code` and this cannot
            // underflow. A clamp here silently reported "0 test lines" for
            // every file whose production count had been inflated past its
            // code count, which is how the blank double-count stayed
            // invisible. Panicking makes the next such defect loud.
            let test = code.checked_sub(*prod).unwrap_or_else(|| {
                panic!("{rel}: production {prod} exceeds code {code} — the scanner and the classifier disagree")
            });
            println!("SIZE_FILE {prod:>6} {test:>6} {comment_blank:>6}  {rel}");
        }
    }
    if std::env::var("NEWT_SIZE_CRATES").is_ok() {
        for (krate, n) in &per_crate {
            println!("SIZE_CRATE {n:>7}  {krate}");
        }
    }
}

/// Split a file's raw text into `(code, comment_blank)` line counts.
///
/// The two are exhaustive and disjoint over the file's lines, which is what
/// makes the report's three columns sum to the file: `prod + test +
/// comment_blank == lines`, where `test` is `code - prod` and `prod` comes
/// from the shared scanner. #2151 broke that identity by counting blanks as
/// production as well as under `comment_blank`.
fn classify(text: &str) -> (usize, usize) {
    let mut comment_blank = 0usize;
    let mut code = 0usize;
    for line in text.lines() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with("//") {
            comment_blank += 1;
        } else {
            code += 1;
        }
    }
    (code, comment_blank)
}

/// **The identity the report's three columns must satisfy** (#2151).
///
/// Not `#[ignore]`d: the report above is a report, but the arithmetic under it
/// is a gate. `production_line_count` prints `prod`, `code - prod`, and
/// `comment_blank`; those sum to the file's line count only if `classify`
/// partitions every line into exactly one of `code` and `comment_blank`. The
/// old code derived `code` as `lines - comment - blank` while a separate loop
/// counted `comment` and `blank`, so a line matching neither rule — or, as it
/// turned out, a blank line also counted as production — broke the sum with
/// nothing to notice.
#[test]
fn classify_partitions_every_line_exactly_once() {
    let cases: &[&str] = &[
        "",
        "\n",
        "fn a() {}\n",
        "// just a comment\n",
        "/// a doc comment\n",
        "//! a module doc\n",
        "\n\n\n",
        "    \t  \n",
        "fn a() {}\n\n// c\n    let x = 1;\n",
        "no trailing newline",
    ];
    for text in cases {
        let (code, comment_blank) = classify(text);
        assert_eq!(
            code + comment_blank,
            text.lines().count(),
            "every line lands in exactly one column: {text:?}"
        );
    }
}

/// Blank lines are never production, with or without an env var (#2151).
///
/// The regression this pins is the one that made the instrument wrong by
/// default: `NEWT_SIZE_NONBLANK` used to decide whether a blank line counted
/// as production, so the same line could appear in the production column AND
/// in `comment_blank`. `classify` is the half that must never call a blank
/// line code; the walker's half is unconditional at the call site above.
#[test]
fn a_blank_line_is_never_code() {
    for blank in ["", " ", "   ", "\t", " \t "] {
        let (code, comment_blank) = classify(&format!("fn a() {{}}\n{blank}\n"));
        assert_eq!(code, 1, "only the fn is code, given {blank:?}");
        assert_eq!(
            comment_blank, 1,
            "the blank is the other column, given {blank:?}"
        );
    }
}

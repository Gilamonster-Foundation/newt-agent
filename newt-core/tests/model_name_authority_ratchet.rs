//! **Ratchet: no new authoritative fuzzy model-name selectors.**
//!
//! The law: model display names are labels, never evidence. No authoritative
//! execution behavior may be selected through `contains`, `starts_with`,
//! fuzzy matching, or parsing a display model ID.
//!
//! # Why a scanner and not a review convention
//!
//! The banned shape is cheap to write, reads as pragmatic, and each instance
//! looks locally harmless — `if model.contains("qwen3")` is one line and
//! obviously "works" on the model in front of you. It fails silently in two
//! directions that only show up on someone else's machine: an alias that
//! contains the token but is not that model, and that model served under an
//! alias that does not. Neither produces an error; both produce wrong output.
//! A convention does not catch that. A failing test does.
//!
//! # What is scanned
//!
//! Production sources only (`*/src/**`, `#[cfg(test)]` items skipped by brace
//! depth), for a **hardcoded model-lineage literal** inside a string
//! predicate — `contains("qwen…")`, `starts_with("claude…")`, and friends.
//! That shape is the banned pattern almost by definition: a lineage name
//! written into a matcher is a label being consulted as evidence.
//!
//! # Precision
//!
//! Tokens are matched at a **boundary**, so `contains("gptq")` — a
//! quantization format, not a lineage — does not trip on `gpt`. Getting this
//! wrong in the permissive direction makes the ratchet noise that people
//! silence; getting it wrong in the strict direction makes it a counter that
//! never fires. `the_scanner_actually_matches_the_banned_shape` pins both
//! edges.
//!
//! # What this does NOT claim
//!
//! It does not prove the surviving sites are safe, nor that every
//! authoritative selector uses a literal (one reading a family key from
//! config — `tenacity.rs`'s `family_for` — is invisible here and is tracked
//! by its own migration). It proves only that this *specific, most common*
//! shape cannot silently multiply.
//!
//! # Baseline provenance
//!
//! 16 is MEASURED, not estimated. The first guess was 8; the scanner then
//! found its own three precision defects, each caught by an assertion rather
//! than by reading it:
//!
//! * it was blind to the real shape in `reasoning.rs`, which spans three
//!   lines (`[…lineage array…]` / `.iter()` / `.any(|f| m.contains(f))`)
//!   while the single-line test fixture passed — hence the collapsed-source
//!   scan;
//! * it counted `lib_tests/` files, which are test code living under `src/`
//!   via `#[path]`, so their `#[cfg(test)]` marker is at the mod declaration
//!   and invisible to brace-depth skipping;
//! * it scored four phantom hits on `ApiMode::Ollama` — "o-**llama**" — and
//!   thirteen more on provider CATALOGS (`models: &["gpt-5.2"]`), which are
//!   data, not matchers. Hence boundary checks on BOTH sides of a token and
//!   the requirement that a lineage array be consumed by a predicate.
//!
//! A baseline that had been guessed would have encoded all four errors as
//! fact.

use std::collections::BTreeMap;

/// Hardcoded lineage literals inside a string predicate, in production code.
///
/// MAY ONLY GO DOWN. A new one is the defect this file exists to prevent.
const KNOWN_LINEAGE_LITERAL_SELECTORS: usize = 16;

/// Per-file expectations, so paying one file down cannot mask a new site in
/// another — an aggregate alone nets to zero and reports success.
fn expected_by_file() -> BTreeMap<&'static str, usize> {
    BTreeMap::from([
        // ACCOUNTING-ONLY — justified exception, not an oversight.
        //
        // Maps a provider's own published model id to its published price. It
        // selects no execution behavior: a wrong match misprices a token
        // count, it does not change what the model is asked to do or how its
        // output is parsed. The ids are the PROVIDER's identifiers for their
        // own hosted catalog (`claude-3-5-sonnet`), which is the one case
        // where the name genuinely is the authority — Anthropic decides what
        // `claude-3-5-sonnet` means and bills accordingly. Migrating it would
        // require a price registry keyed by artifact digest, which no hosted
        // provider publishes.
        ("newt-core/src/pricing.rs", 13),
        // AUTHORITATIVE — the next migration target.
        //
        // `fabricates: model.contains("nemotron")` and
        // `model.contains("coder") || contains("codestral") || contains("deepseek")`
        // synthesize per-model capability from the display name, and the
        // roster assigns ROLES from it: which model plans, which navigates,
        // which is trusted not to fabricate. An alias decides what a model is
        // asked to do. This is the scheduler family/capability synthesis the
        // slice must migrate to `ResolvedModel` + declared capability.
        ("newt-scheduler/src/roster.rs", 3),
    ])
}

/// Lineage tokens. A hardcoded one inside a string predicate is the banned
/// shape; a *configured* family key is not (that is data, and its own
/// migration).
const LINEAGE_TOKENS: &[&str] = &[
    "qwen",
    "gemma",
    "nemotron",
    "deepseek",
    "llama",
    "mistral",
    "glm",
    "kimi",
    "claude",
    "phi-",
    "granite",
    "ornith",
    "minimax",
    "codestral",
    "gpt-",
];

const PREDICATES: &[&str] = &["contains(", "starts_with(", "ends_with("];

/// How far after a lineage array to look for the predicate that consumes it.
/// Sized for the real shape in `reasoning.rs`, which collapses to roughly
/// `["nemotron", "deepseek-r1", "qwen3"] .iter() .any(|fam| m.contains(fam))`
/// — comfortably under this, while a provider catalog entry's following text
/// (`keys_at: "https://…"`) is not a predicate at any distance.
const CONSUMPTION_WINDOW: usize = 160;

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("newt-core has a parent workspace directory")
        .to_path_buf()
}

/// Strip line comments and `#[cfg(test)]` items (by brace depth), then
/// collapse whitespace.
///
/// Collapsing is what makes the scan see the shape as WRITTEN. The real
/// stopgap in `reasoning.rs` spans three lines —
/// `["nemotron", "deepseek-r1", "qwen3"]` / `.iter()` / `.any(|f| m.contains(f))`
/// — so a line-at-a-time scan misses it entirely while a single-line test
/// fixture passes. That gap was found by the see-the-real-sites assertion,
/// not by reading the code.
///
/// Brace-depth, never a latch: "saw `#[cfg(test)]`, skip the rest of the
/// file" would blind the scanner to everything after the first test item —
/// a failure this workspace has already shipped once and caught with a drill.
fn code_text(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut test_depth: i32 = 0;
    let mut pending = false;
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if test_depth > 0 || pending {
            let opens = line.matches('{').count() as i32;
            let closes = line.matches('}').count() as i32;
            if pending && opens > 0 {
                pending = false;
            } else if pending && trimmed.ends_with(';') {
                pending = false;
                continue;
            }
            test_depth = (test_depth + opens - closes).max(0);
            continue;
        }
        if trimmed.starts_with("#[cfg(") && trimmed.contains("test") {
            pending = true;
            continue;
        }
        out.push_str(trimmed);
        out.push(' ');
    }
    out
}

/// Is `tok` at `at` a whole lineage token — not part of a longer word?
///
/// BOTH sides matter, and the second one was found by inspection rather than
/// by the tests: `gptq` is a quantization format, not `gpt-`, and — the one
/// that actually bit — **"o-llama"** means `provider_preset.rs`'s `Ollama`
/// enum variants each looked like a `llama` lineage literal. Four phantom
/// sites would have sat in the baseline forever, making the ratchet track
/// noise while reporting diligence.
fn at_token_boundary(hay: &str, tok: &str, at: usize) -> bool {
    let before_ok = at == 0
        || !hay[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphabetic());
    let after = at + tok.len();
    let after_ok = hay[after..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_ascii_alphabetic());
    before_ok && after_ok
}

/// Count the two spellings of the banned shape in one source file.
fn count_in_source(src: &str) -> usize {
    let hay = code_text(src).to_ascii_lowercase();
    let mut n = 0usize;
    for tok in LINEAGE_TOKENS {
        // (a) a predicate applied directly to a lineage literal.
        for p in PREDICATES {
            let needle = format!("{p}\"{tok}");
            n += hay
                .match_indices(&needle)
                .filter(|(at, _)| at_token_boundary(&hay, tok, at + p.len() + 1))
                .count();
        }
        // (b) a hardcoded lineage TABLE that is CONSUMED BY A PREDICATE —
        // `["nemotron", "deepseek-r1", "qwen3"].iter().any(|f| m.contains(f))`.
        //
        // The consumption requirement is not fussiness. A bare array of model
        // ids is ordinary data: `provider_preset.rs` lists each provider's
        // offered catalog (`models: &["gpt-5.2"]`, `&["claude-sonnet-4-5"]`),
        // which selects nothing. Counting those put four phantom sites in the
        // baseline. What makes an array the banned shape is that a predicate
        // reads it to decide behavior.
        let needle = format!("[\"{tok}");
        n += hay
            .match_indices(&needle)
            .filter(|(at, _)| at_token_boundary(&hay, tok, at + 2))
            .filter(|(at, _)| {
                let end = (at + CONSUMPTION_WINDOW).min(hay.len());
                let tail = &hay[*at..end];
                tail.contains(".contains(") || tail.contains(".any(")
            })
            .count();
    }
    n
}

/// Line-level helper retained for the precision tests, which assert on
/// single lines.
fn is_lineage_literal_selector(line: &str) -> bool {
    count_in_source(line) > 0
}

fn scan() -> BTreeMap<String, usize> {
    let root = workspace_root();
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if matches!(name.as_ref(), "target" | ".git" | ".claude" | "docs") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if !path.components().any(|c| c.as_os_str() == "src") {
                continue;
            }
            // Test code that lives under `src/` because lib.rs pulls it in with
            // `#[path]` — the `#[cfg(test)]` marker is at the mod declaration,
            // not in the file, so brace-depth skipping cannot see it. Counting
            // these would make the ratchet track test fixtures instead of
            // production selectors.
            let is_test_file = path.components().any(|c| {
                matches!(
                    c.as_os_str().to_string_lossy().as_ref(),
                    "lib_tests" | "mod_tests"
                )
            }) || name.ends_with("_tests.rs");
            if is_test_file {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let n = count_in_source(&text);
            if n > 0 {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                found.insert(rel, n);
            }
        }
    }
    found
}

/// **The ratchet.**
#[test]
fn lineage_literal_selectors_only_decrease() {
    let found = scan();
    let total: usize = found.values().sum();
    assert!(
        total <= KNOWN_LINEAGE_LITERAL_SELECTORS,
        "a NEW hardcoded model-lineage selector appeared: {total} > \
         {KNOWN_LINEAGE_LITERAL_SELECTORS}.\n\
         Model display names are LABELS, never evidence. Resolve identity through \
         `newt_core::model_identity::ResolvedModel` (operator card > resolved card > \
         artifact metadata > provider metadata), and when family is Unknown apply NO \
         family policy. A name may only produce a `FamilySuggestion` the operator \
         confirms.\n\
         Sites now: {found:#?}"
    );
    assert_eq!(
        total, KNOWN_LINEAGE_LITERAL_SELECTORS,
        "count fell to {total} — good. Lower KNOWN_LINEAGE_LITERAL_SELECTORS and \
         update expected_by_file() in the same change, so the next one cannot hide \
         in the slack."
    );
}

/// Per-file, so one file's migration cannot mask another file's regression.
#[test]
fn no_file_gains_a_lineage_literal_selector() {
    let found = scan();
    let expected = expected_by_file();
    for (file, count) in &found {
        let allowed = expected.get(file.as_str()).copied().unwrap_or(0);
        assert!(
            *count <= allowed,
            "{file} has {count} hardcoded lineage selector(s), expected at most \
             {allowed}. Use ResolvedModel; a name may only suggest, never decide."
        );
    }
}

/// **The scanner must actually match the banned shape, and must not match its
/// look-alikes.** A counter that always returns zero would pass the ratchet
/// above while the law was being broken — this codebase has caught that
/// vacuous-green pattern five times, and a sixth is not free.
#[test]
fn the_scanner_actually_matches_the_banned_shape() {
    // Positive: the exact shape, in its common spellings.
    assert!(is_lineage_literal_selector(
        r#"    if m.contains("qwen3") { return true; }"#
    ));
    assert!(is_lineage_literal_selector(
        r#"    model.starts_with("claude-3-opus")"#
    ));
    assert!(is_lineage_literal_selector(
        r#"    ["nemotron", "deepseek-r1"].iter().any(|f| m.contains(f))"#
    ));

    // Negative: a quantization format that merely contains a lineage token as
    // a prefix. Tripping on this would make the ratchet noise, and noise gets
    // silenced.
    assert!(!is_lineage_literal_selector(
        r#"    if lc.contains("gptq") {"#
    ));
    // Negative: a predicate with no lineage literal at all.
    assert!(!is_lineage_literal_selector(
        r#"    if path.contains("config.toml") {"#
    ));
    // Negative: "O-llama" is a PROVIDER, not the llama lineage. This one was
    // a live false positive — provider_preset.rs's `ApiMode::Ollama` arms
    // scored four phantom hits before the leading-boundary check existed.
    assert!(!is_lineage_literal_selector(
        r#"    if mode.contains("ollama") { BackendKind::Ollama }"#
    ));
    // Negative: a lineage word in prose, with no predicate.
    assert!(!is_lineage_literal_selector(
        r#"    let note = "qwen3 is a model family";"#
    ));
    // Negative: a provider's OFFERED-MODEL CATALOG. Data, not a matcher — it
    // selects nothing. Four of these sat in an earlier baseline.
    assert!(!is_lineage_literal_selector(
        r#"    Preset { env: &["openai_api_key"], models: &["gpt-5.2"], keys_at: "https://x" }"#
    ));
    // Positive, and the one a line-at-a-time scan misses: a lineage array
    // consumed by a predicate, as actually written in reasoning.rs.
    assert!(is_lineage_literal_selector(
        r#"["nemotron", "deepseek-r1", "qwen3"].iter().any(|fam| m.contains(fam))"#
    ));
}

/// The scanner must see real sites in the real tree — otherwise every green
/// it reports is uninformative.
#[test]
fn the_scanner_sees_the_real_sites() {
    let found = scan();
    assert!(
        !found.is_empty(),
        "the scanner found no lineage selectors anywhere — either they are all \
         migrated (then lower the constant and delete this assertion) or the \
         scanner is broken"
    );
    assert!(
        found.contains_key("newt-scheduler/src/roster.rs"),
        "the scanner must see the scheduler's name-derived capability synthesis, \
         the next migration target; found: {found:#?}"
    );
}

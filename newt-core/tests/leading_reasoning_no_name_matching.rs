//! **Zero-baseline guard for the migrated selector.**
//!
//! `reasoning.rs` used to decide leading-reasoning stream filtering with
//! `model.contains("nemotron" | "deepseek-r1" | "qwen3")` — authoritative
//! execution behavior chosen from a display label, wrong in both directions
//! and silent in both. That list is gone; the flag comes from an inline
//! backend capability declaration.
//!
//! This asserts it stays gone. Scope is deliberately one file and one
//! property: **the migrated selector does not regress.**
//!
//! # Why not a workspace-wide scanner
//!
//! An earlier version of this PR carried a 426-line generalized scanner
//! counting every hardcoded lineage literal across the workspace, held at a
//! measured baseline of 16. It was cut: 13 of those 16 are `pricing.rs`
//! (accounting-only — a provider's published id mapped to their published
//! price, the one case where the name genuinely IS the authority) and 3 are
//! the scheduler's roster, neither of which this PR touches. A guard that
//! bakes in unrelated debt makes one migration look like a workspace audit,
//! and it costs more lines than the migration it guards.
//!
//! The generalized scanner earns its keep once the selector migrations —
//! tenacity, bundles, scheduler — bring the baseline near zero and it becomes
//! a real ratchet rather than a ledger of things nobody is fixing yet. Until
//! then, one zero-baseline guard per migrated selector: precise, cheap, and
//! it fails by NAME the moment the specific thing regresses.

/// Model-lineage tokens that must not drive behavior from this file again.
const LINEAGE_TOKENS: &[&str] = &["qwen", "nemotron", "deepseek", "gemma", "llama", "mistral"];

/// The migrated selector's source file.
fn reasoning_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/reasoning.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// **No lineage token is consulted as a string in this file.**
///
/// Comments are skipped: the file explains WHY the old list is gone, and that
/// explanation necessarily names the families it used to match. Naming them
/// in prose is the record; matching on them is the defect.
#[test]
fn reasoning_does_not_match_model_names() {
    let src = reasoning_source();
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();

    for tok in LINEAGE_TOKENS {
        assert!(
            !code.contains(&format!("\"{tok}")),
            "newt-core/src/reasoning.rs mentions the lineage token `{tok}` in a \
             string literal.\n\
             Leading-reasoning is resolved from an inline backend capability \
             declaration ([backends.<name>.capability] emits_leading_reasoning), \
             never from the model name — an operator serves any artifact under any \
             alias, so a label is wrong in both directions and silent in both."
        );
    }
}

/// The deleted function must not return, in name or in shape: nothing in this
/// file may take a model name and answer a behavioral question about it.
#[test]
fn reasoning_exposes_no_name_keyed_behavioural_predicate() {
    let src = reasoning_source();
    assert!(
        !src.contains("pub fn emits_leading_reasoning"),
        "`reasoning::emits_leading_reasoning(&str)` is deleted — the capability is \
         declared on the backend and threaded through ChatCtx/TurnDriverConfig. A \
         function here taking a model name is the stopgap returning."
    );
}

/// The guard must be able to SEE its subject — otherwise it reports success
/// forever against a file it never read.
#[test]
fn the_guard_reads_the_real_file() {
    let src = reasoning_source();
    assert!(
        src.contains("ThinkFilter"),
        "reasoning.rs should contain the filter this guard is about; if it moved, \
         move the guard with it rather than leaving it passing vacuously"
    );
}

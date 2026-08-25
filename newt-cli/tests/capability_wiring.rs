//! **Headless `solve` threads the model capabilities it needs.**
//!
//! Independent review found that `newt solve` set `reasoning_replay_scope`
//! from the resolved backend but never `emits_leading_reasoning`, so headless
//! runs silently used the `false` default: a model whose backend declared
//! leading-reasoning had its chain-of-thought printed into the answer — in
//! the one lane with no human watching the stream.
//!
//! The two assignments now sit adjacent in `solve.rs`. That is not
//! protection; the omission was adjacent to its sibling before too, and it
//! still shipped. This is the N-call-sites trap the codebase names elsewhere
//! (`ollama_auth_headers`, #1312): a per-site assignment that must be
//! repeated, where forgetting one is silent.
//!
//! So it is asserted — against `solve.rs` directly. An earlier version walked
//! the whole workspace pairing capabilities per file; it was cut for two
//! reasons. It false-positived on `config.rs`, where the accessor is DEFINED
//! rather than threaded, and it spent a hundred lines of traversal to make
//! one claim about one file. A guard should read the thing it is about.

/// Capabilities headless `solve` must thread from the resolved backend.
const REQUIRED: &[&str] = &["reasoning_replay_scope", "emits_leading_reasoning"];

fn solve_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/solve.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Every required capability is assigned from the resolved backend.
#[test]
fn headless_solve_threads_every_capability() {
    let src = solve_source();
    for cap in REQUIRED {
        let assignment = format!("dc.{cap} = backend.{cap}()");
        assert!(
            src.contains(&assignment),
            "newt-cli/src/solve.rs is missing `{assignment};`.\n\
             A capability threaded in the TUI but not here is silent in headless \
             runs — `solve` shipped exactly that, defaulting leading-reasoning to \
             false so a model whose backend declared it printed its \
             chain-of-thought into the answer."
        );
    }
}

/// The guard must be reading the real file — otherwise it reports success
/// forever against something it never opened.
#[test]
fn the_guard_reads_the_real_solve_source() {
    let src = solve_source();
    assert!(
        src.contains("TurnDriverConfig::new"),
        "solve.rs should build the driver config this guard is about; if that \
         moved, move the guard with it rather than leaving it passing vacuously"
    );
}

//! #2907 P2 3: the TUI lane takes the card's tuning ceiling from the SAME
//! per-turn capability decision as every other card layer.
//!
//! Source-shape guard, mirroring newt-cli's `tests/capability_wiring.rs`:
//! the ChatCtx literal lives inside the interactive run loop, which no
//! in-process unit test can drive (a full behavioral proof of this lane is a
//! real-PTY concern). The decision seam it reads — `output_ceiling()` — is
//! unit-tested in newt-core (`model_card`), and the headless process test
//! `a_tuning_card_output_ceiling_bounds_the_headless_retry` grounds the
//! shared card → decision → ctx chain end-to-end.

#[test]
fn chat_loop_wires_the_decision_output_ceiling_into_the_ctx() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/chat.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    assert!(
        src.contains("output_ceiling: turn_capabilities.output_ceiling(),"),
        "newt-tui/src/chat.rs no longer wires turn_capabilities.output_ceiling() \
         into the ChatCtx — the card key output_ceiling_tokens would silently \
         stop overriding the retry ceiling in TUI turns"
    );
}

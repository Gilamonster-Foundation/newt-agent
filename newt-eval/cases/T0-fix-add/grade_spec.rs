//! Canonical behavioral spec for T0 — the **ungameable** grade.
//!
//! This file is NOT in `workspace/`, so the agent never sees it and cannot edit
//! it. `ratchet.sh` drops it into the produced tree's `tests/` and runs ONLY
//! this (`cargo test --test grade_spec`). A crew that "passes" by editing its
//! own inline assertion to match a buggy `add` still FAILS here — the only way
//! to green is a correct `add`. (Structurally enforced TDD, measurement side.)
use t0_fix_add::add;

#[test]
fn add_returns_the_sum() {
    assert_eq!(add(2, 3), 5);
    assert_eq!(add(0, 0), 0);
    assert_eq!(add(-4, 1), -3);
    assert_eq!(add(100, 23), 123);
}

//! **The Python surface of the scorecard, guarded at the source (D3b, #1886).**
//!
//! `PyScorecard`'s table method is named `table` in Rust and `render_table` in
//! Python. Nothing else in a per-PR gate checks that pairing: `pyo3_module.rs`
//! compiles only under the `pyo3` feature, the wheel is built on tag only, and
//! the pytest suite at `newt-agent-py/tests/` is not part of the PR workflow
//! (`.github/workflows/ci.yml` names it only to explain a coverage exclusion).
//!
//! So a bad rename would reach a release before anything noticed. This reads
//! the source and pins the pairing. It is a text guard, and a text guard is
//! what is available here — the exposed name is a string pyo3 hands to a
//! Python interpreter, and no Rust-side test can ask for it without one.

const PYO3_MODULE: &str = include_str!("../src/pyo3_module.rs");

/// True when `src` binds the Python name `render_table` to a method, with the
/// attribute IMMEDIATELY on the method it renames — an attribute floating
/// beside an unrelated function renames nothing.
fn binds_render_table(src: &str) -> bool {
    let Some(at) = src.find("#[pyo3(name = \"render_table\")]") else {
        return false;
    };
    src[at..]
        .lines()
        .skip(1)
        .map(str::trim_start)
        .find(|l| !l.is_empty() && !l.starts_with("//"))
        .is_some_and(|l| l.starts_with("fn table("))
}

#[test]
fn the_python_scorecard_still_exposes_render_table() {
    assert!(
        binds_render_table(PYO3_MODULE),
        "PyScorecard must expose `render_table` to Python. \
         newt-agent-py/tests/test_eval.py::test_scorecard_renders_table \
         calls it, and the Rust method is named `table` so the sprawl \
         ratchet stops counting a binding as a table implementation."
    );
}

/// The twin. A guard that cannot fail is not a guard, and this one is a
/// substring search — the shape most likely to pass for the wrong reason.
#[test]
fn the_guard_can_fail() {
    assert!(binds_render_table(
        "#[pyo3(name = \"render_table\")]\n    fn table(&self) -> String {"
    ));
    // Blank lines and doc comments between the attribute and the method are
    // formatting, not separation.
    assert!(binds_render_table(
        "#[pyo3(name = \"render_table\")]\n\n    fn table(&self) -> String {"
    ));
    assert!(binds_render_table(
        "#[pyo3(name = \"render_table\")]\n    /// doc\n    fn table(&self) {"
    ));
    // Renamed away: Python loses the method.
    assert!(!binds_render_table("    fn table(&self) -> String {"));
    // Attribute present but attached to something else — the failure a
    // plain `contains` would have waved through.
    assert!(!binds_render_table(
        "#[pyo3(name = \"render_table\")]\n    fn cases(&self) -> Vec<String> {"
    ));
    assert!(!binds_render_table(""));
}

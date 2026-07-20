//! `test_stub()` exists, but only under `cfg(test)` or the `test-util` feature.
//! A downstream crate that has not opted in cannot reach it, so the escape
//! hatch cannot be taken by accident.
fn main() {
    let _w = newt_core::tty::PromptWindow::test_stub();
}

//! There is no `new`, no `Default`, and no other public way in. The ONLY
//! sanctioned constructor is `Terminal::suspend_for_prompt()`, which erases
//! every live ephemeral writer before it returns one.
fn main() {
    let _w = newt_core::tty::PromptWindow::new();
}

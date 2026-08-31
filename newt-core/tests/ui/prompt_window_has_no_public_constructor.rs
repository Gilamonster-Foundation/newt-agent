// There is no `new`, no `Default`, and no other public way in. The sanctioned
// constructors are `Terminal::suspend_for_prompt()` and `suspend_for_prompt_to()`;
// both erase every live ephemeral writer before returning a sealed window.
fn main() {
    let _w = newt_core::tty::PromptWindow::new();
}

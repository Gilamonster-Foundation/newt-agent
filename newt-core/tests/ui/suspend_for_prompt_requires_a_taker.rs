// #2027: there is no argument-free door onto the terminal. Every acquisition
// names the surface doing the acquiring, so an eighteenth call site cannot be
// added without declaring what it does about rows another surface holds — the
// half of the guard the compiler enforces, rather than the registry test.
fn main() {
    let _w = newt_core::tty::Terminal::suspend_for_prompt();
}

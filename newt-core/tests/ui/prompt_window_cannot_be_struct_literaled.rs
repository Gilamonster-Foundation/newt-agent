//! The field is a PRIVATE sealed ZST, so struct-literal syntax cannot name it
//! — the seal is on the type, not merely on a constructor function.
fn main() {
    let _w = newt_core::tty::PromptWindow {};
}

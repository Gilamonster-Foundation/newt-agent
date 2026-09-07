use super::*;

/// The narrator + list-item printers write to stdout (hard to capture here),
/// so this just exercises every branch — color/no-color × active/inactive ×
/// verbose — to keep them from rotting and to cover them for the gate.
#[test]
fn printers_cover_every_branch_without_panicking() {
    for color in [true, false] {
        for verbose in [true, false] {
            print_newt("narrator line", color, verbose);
        }
        print_list_item("name · ollama · model @ url", true, color);
        print_list_item("name · ollama · model @ url", false, color);
        print_harness_notice(
            "over budget — dispatching and letting the backend decide",
            color,
        );
    }
}

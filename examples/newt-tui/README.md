# newt-tui Example

Shows a minimal TUI setup:

```rust
use newt_tui::Tui;
let mut tui = Tui::new();
tui.display("Hello, Newt!");
tui.run();
```
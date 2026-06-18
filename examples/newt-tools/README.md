# newt-tools Example

Shows basic tool usage:

```rust
use newt_tools::format_code;
let formatted = format_code("fn main() { println!(\"Hello\"); }");
assert!(formatted.contains("fn main() {"));
```
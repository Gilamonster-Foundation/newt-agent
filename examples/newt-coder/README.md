# newt-coder Example

Demonstrates code generation:

```rust
use newt_coder::build_prompt;
let prompt = build_prompt("rename foo to bar");
assert!(prompt.contains("rename"));
```
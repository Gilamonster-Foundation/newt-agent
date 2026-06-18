# newt-eval Example

Demonstrates test case evaluation:

```rust
use newt_eval::TestCase;
let test = TestCase::new("rename foo to bar", "bar", "foo");
assert!(test.evaluate().is_success());
```
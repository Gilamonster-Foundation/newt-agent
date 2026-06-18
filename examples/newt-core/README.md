# newt-core Example

Demonstrates basic router usage:

```rust
use newt_core::Router;
let router = Router::new();
let tier = router.classify("rename foo to bar");
assert_eq!(tier, newt_core::Tier::Fast);
```
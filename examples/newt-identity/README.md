# newt-identity Example

Shows identity creation and management:

```rust
use newt_identity::Identity;
let identity = Identity::new("user123", "agent-key");
assert_eq!(identity.id(), "user123");
```
# newt-mcp-data Example

Demonstrates MCP data structure usage:

```rust
use newt_mcp_data::Message;
let msg = Message::new("request", "data");
assert_eq!(msg.kind(), "request");
```
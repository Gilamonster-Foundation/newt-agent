# newt-mcp-client Example

Shows basic MCP client usage:

```rust
use newt_mcp_client::Client;
let client = Client::new("localhost:8080");
let response = client.request("ping").await;
assert_eq!(response, "pong");
```
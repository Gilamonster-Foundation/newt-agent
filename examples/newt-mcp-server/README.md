# newt-mcp-server Example

Shows basic MCP server setup:

```rust
use newt_mcp_server::Server;
let mut server = Server::new("0.0.0.0:8080");
server.handle("ping", |_| "pong");
server.start().await;
```
# newt-mesh Example

Demonstrates mesh network setup:

```rust
use newt_mesh::Mesh;
let mut mesh = Mesh::new();
mesh.add_node("node1");
assert!(mesh.has_node("node1"));
```
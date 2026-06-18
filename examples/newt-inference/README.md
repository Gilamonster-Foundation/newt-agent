# newt-inference Example

Demonstrates local model inference:

```rust
use newt_inference::LocalOllamaBackend;
let backend = LocalOllamaBackend::discover("llama3.1:8b").await;
let response = backend.complete("Hello!").await;
assert_eq!(response.content, "Hello!");
```
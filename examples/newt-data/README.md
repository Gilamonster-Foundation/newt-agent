# newt-data Example

Shows basic data structure usage:

```rust
use newt_data::DataStore;
let mut store = DataStore::new();
store.insert("key", "value");
assert_eq!(store.get("key"), Some("value"));
```
# newt-data

`newt-data` is Newt-Agent's headless data-science engine. It provides a
`DataStore` abstraction backed by bundled SQLite, with CSV ingestion, SQL
queries, schema and statistical summaries, table listing, and reviewable
Jupyter notebook persistence.

```rust
use std::path::Path;
use newt_data::{DataStore, SqliteBackend};

fn main() -> Result<(), newt_data::DataError> {
    let store = SqliteBackend::open_in_memory()?;
    store.ingest_csv(Path::new("sales.csv"), "sales")?;

    let result = store.query("SELECT * FROM sales", 100)?;
    println!("returned {} rows", result.returned);
    Ok(())
}
```

The default build is a local SQLite library with no inference or shell
surface. Optional features add:

- `kernel` — attach to an existing Jupyter server over REST and WebSocket
- `pyo3` — Python bindings used by Newt-Agent's Python package

Query truncation is measured by reading one row past the requested cap, and
numeric summaries use sample standard deviation and linearly interpolated
quartiles.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).

## License

Apache-2.0

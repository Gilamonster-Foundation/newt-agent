# newt-mcp-data

`newt-mcp-data` is Newt-Agent's local stdio MCP server for data-science
workflows. It adapts the headless
[`newt-data`](https://crates.io/crates/newt-data) SQLite engine and an
already-running Jupyter kernel into MCP tools; it does not perform inference or
expose a shell.

Its tool surface includes:

- SQLite CSV ingest, queries, table summaries, and table listing
  (`sql_ingest_csv`, `sql_query`, `sql_summarize`, `sql_list_tables`)
- Jupyter attachment and cell execution (`kernel_attach`, `run_cell`)
- Reviewable notebook reading and editing (`notebook_read`,
  `notebook_insert_cell`, `notebook_persist_executed_cell`)
- Read-only inspection of live pandas DataFrames (`list_dataframes`,
  `inspect_dataframe`)

## Configure Newt

Install the binary, then add it to `~/.newt/config.toml`:

```toml
[[mcp_servers]]
name = "data"
command = "newt-mcp-data"
```

Newt namespaces the tools with the configured server name, such as
`data__sql_query`. By default, the SQLite database is
`<workspace>/.newt-data/data.db`; set `NEWT_DATA_DB` to choose another path.

SQL tools can modify the local database, and `run_cell` executes code in the
human's Jupyter kernel. Clients should present those exact inputs for review
before invoking them.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).

## License

Apache-2.0

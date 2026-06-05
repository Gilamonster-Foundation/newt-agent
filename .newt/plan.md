# Configuration & Logging Improvement Plan

## Goals
- Introduce a unified configuration system (`newt-core::Config`) that consolidates environment variables, TOML/YAML files, and CLI flags.
- Replace ad‑hoc `println!` / `print!` logging with a structured, levels‑based logger (e.g., `tracing` + JSON output).
- Ensure all components (CLI, inference back‑ends, MCP server) read configuration consistently and emit structured logs.

## Tasks
1. **Config Abstraction**
   - Create `newt-core/src/config.rs` with a `Config` struct, loading logic (env vars > .toml > CLI flags), and default values.
   - Provide a global accessor (`Config::global()` or similar) for the rest of the codebase.
2. **Logging Infrastructure**
   - Add `tracing` crate and set up a logger in `main.rs` (or equivalent entry point) that reads log level from config.
   - Replace existing `println!` statements with `info!`, `debug!`, `warn!`, `error!` macros.
   - Ensure logs are JSON‑serializable for external processing.
3. **Migration**
   - Update `newt-cli` argument parsing to feed CLI flags into `Config`.
   - Modify `newt-mcp-server` and inference modules to use `Config::global()` for settings such as log level, timeout, etc.
   - Remove obsolete environment‑variable accesses scattered across the codebase.
4. **Documentation**
   - Add a design doc (`docs/config.md`) describing the configuration hierarchy and logging format.
   - Update README with usage examples for config files and log output.
5. **Testing**
   - Write unit tests for `Config::load` covering precedence rules.
   - Add integration test that runs the CLI with a custom config file and verifies log output.

## Success Criteria
- All components read configuration from a single source.
- Logs are structured, level‑aware, and can be parsed by external tools.
- No remaining raw `println!` statements in the codebase.
- CI passes with new config tests.

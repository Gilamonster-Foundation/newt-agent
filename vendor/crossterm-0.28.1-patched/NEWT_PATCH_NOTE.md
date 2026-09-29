# Why this vendored copy exists (#2644 / #2646)

Upstream crossterm 0.28.1's `cursor::sys::unix::read_position_raw` swallows a
`poll` error and retries forever instead of returning it, so under a pty that
answers `poll`/`read` with an error (vhs's bundled `ttyd`, not a plain
timeout) the DSR cursor-position query never returns — not even past its own
advertised 2s timeout. Confirmed still present in crossterm 0.29.0 and
current `master` as of 2026-09-29 (an upgrade does not fix this). The one
changed function is in `src/cursor/sys/unix.rs`, marked with a `newt #2644`
comment; nothing else in this copy is touched.

## This only helps builds FROM the newt workspace

`[patch.crates-io]` (root `Cargo.toml`) is a Cargo workspace-level override.
It applies to `cargo build`/`test` run inside this workspace, CI, and
`just install`. It is **ignored** by `cargo install newt-agent` pulled from
crates.io — that path resolves the real, unpatched `crossterm` from the
registry and reintroduces the hang. This vendoring does not fix that path.

**Exit plan**, in order of preference:
1. Land the upstream fix (draft PR + patch in `../upstream-crossterm/`) and
   bump newt's `crossterm` dependency past the release that includes it —
   remove this directory once that lands.
2. If newt starts publishing to crates.io before upstream lands, publish a
   small crossterm fork carrying just this patch (the `brush-ocap-*` model)
   and depend on that instead of `[patch.crates-io]`, so `cargo install`
   picks it up too.

**Remove this vendored copy when crossterm's published version includes the
`read_position_raw` fix** (check `src/cursor/sys/unix.rs` upstream before
bumping — see the draft PR for the exact function).

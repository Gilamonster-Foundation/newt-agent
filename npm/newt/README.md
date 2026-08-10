# @gilamonster/newt

The `newt` CLI ([newt-agent](https://github.com/Gilamonster-Foundation/newt-agent)),
delivered as a prebuilt Rust binary for your platform — no Rust toolchain, no pip.

```bash
npm install -g newt-agent      # umbrella (recommended)
# or the scoped package directly:
npm install -g @gilamonster/newt
newt --help
```

Ships no binary itself: it declares per-platform `@gilamonster/newt-<os>-<arch>`
packages as `optionalDependencies`; your package manager installs the matching one
and the `newt` shim execs it. No `postinstall`. Not your platform? `cargo install newt-agent`.

License: Apache-2.0

# newt-agent CLI

The `newt-agent` crate provides the `newt` command and CLI entry points for the
local coding agent. The [workspace README](../README.md) covers installation,
configuration, and interactive use.

With Smart Harness enabled, `newt --resume <conversation> --adopt-frame`
explicitly admits that selected saved conversation into Frame. The saved
history is verified and retained as quoted historical context; old tool claims
do not become new execution receipts. Later resumes follow the admitted Frame
without importing again. The option requires an explicit interactive resume,
does not authorize other tabs, and leaves backend selection and permission
gates unchanged.

`newt mcp import` names each rejected server, respects the config locks it
already holds, and reports each adopted server's readiness the same way as
`newt doctor`, `newt auth` and `/mcp`.

`newt mcp import --merge --grant-net` skips existing servers without creating
an empty authoritative config or granting their imported hosts. Adopted
servers and exact host grants share the existing atomic config transaction.
Import preserves existing settings, comments, and a symlinked config target;
the TOML editor may insert new tables before a trailing comment.

The serial `mcp_cli` real-resource tests ground the parser, config-edit, and
permission fixtures. They run in the scheduled, dispatched, and called
`mcp-import-real` workflow. The composed recovery test explicitly approves one
exact remote tool while retaining the baseline execution caveats.

`newt doctor --sign-toolchains` signs the user directory's `ocap/toolchains.toml`
with the existing operator identity after showing the ordered roots, PATH entries,
optional content pins, policy identity, and key fingerprint. It requires explicit
terminal approval, saves an owner-only backup including comments, and refuses
concurrent policy/key changes or failed root admission. It never creates a key,
auto-signs, or exposes a model tool. Restart sessions after changing policy.
The explicit serial acceptance harness uses only disposable keys:
`cargo test -p newt-agent --test toolchain_signing -- --sign-toolchains-3327`.

License: see [LICENSE](../LICENSE).

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 01:24 EDT | Date: 2026-09-18

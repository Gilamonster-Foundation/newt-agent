# agent-toolchain

Portable toolchain adapters and authority contracts for agent harnesses,
incubated in the Newt workspace. This crate must not depend on Newt, a TUI,
an inference provider, or a model prompt.

The target interface is ordinary commands with their original arguments,
output, and exit status. The caller supplies authority and execution; an
adapter never grants itself permission. Linux, macOS, and native Windows
must satisfy the same behavior contract through their platform backends.

## Extraction status

The initial slice extracts the existing authority types and the optional
`embedded-git` adapter contract. This is a compatibility boundary for the
existing engine, not a claim of Git CLI feature parity. It is not enabled
by default. Newt's current integration still opts into it while the native
command path is migrated.

`native_git` contains the portable commit-message, signing, and verified-ref
state machine plus bounded canonical helper messages. It owns no signing keys,
filesystem grants, subprocess executor, or transport authentication. The
embedding harness must bind it to an admitted native Git invocation and verify
actual repository state before reporting publication. Newt's current adapter
is a scoped implementation under validation; it does not establish complete
Git mutation or platform parity.

The default model tool surface should remain small: read, write, edit, and
command execution. Specialized adapters are optional extensions and must
not intercept commands they cannot faithfully execute. Newt owns operator
permissions, session audit, and contribution attribution; platform
confinement continues to enforce the actual effects of child processes.

See [the extraction plan](../docs/design/agent-toolchain.md) for migration
gates and the two live acceptance tasks.

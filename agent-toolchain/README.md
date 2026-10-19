# agent-toolchain

Harness-neutral contracts for running developer tools under an agent's
authority: the capability types (`GitCaveats`, `caveats`), the native Git
commit broker (`native_git`: message finalizing, signing, verified ref
updates) and the optional `embedded-git` adapter interface. It must not depend
on Newt, a TUI, an inference provider or a prompt. It is versioned and published
separately so another harness can use it.

**Where it sits.** The bottom layer. `newt-core` implements the harness side:
grants, audit, attribution. [`newt-git`](../newt-git/README.md) is one
adapter behind it. The caller supplies authority and execution; an adapter
never grants itself permission.

**Direction.** The target is ordinary commands with their real arguments,
output and exit status, with the same contract on Linux, macOS and Windows. The
model-facing surface stays small (read, write, edit, run a command), and an
adapter must not intercept a command it can't faithfully execute.

Migration plan and gates: [docs/design/agent-toolchain.md](../docs/design/agent-toolchain.md).

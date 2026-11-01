---
name: commit-attribution
description: Credit the model and the harness on commits you author with a Co-authored-by trailer, in the house format. Covers which commits the harness already attributes (all supported ones) and the rare external commit that needs a byline.
when_to_use: When you are about to commit, amend or reword a commit; when a commit you made is missing its Co-authored-by trailer; when you are unsure which model/harness identity to credit.
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: ["git"] }
  fs_read: all
  net: { only: [] }
  max_calls: unlimited
---

# Commit attribution

This was a standing rule of the default system prompt (`commit-attribution`). The
harness's own bookkeeping is the source of truth; this skill covers only what that
bookkeeping does not.

- **Harness-managed commits** are every supported Git commit you make: the harness's
  git tool, and ordinary `git commit` through `run_command`, which runs through the
  native commit broker and the ledger finalizer. Each records every contributing
  model/harness pair from the attribution ledger by itself. Never add a second
  trailer by hand, and never guess an identity.
- **Genuinely external or unmanaged Git** (a commit made outside the harness, which
  has no ledger entry) is the only case that needs a byline. Credit it in the house
  format, with the harness's identity (the newt-agent GitHub App,
  <https://github.com/apps/newt-agent>) and the model and build you are actually
  running as, e.g.
  `Co-authored-by: <model> (v<version> <build>) <309460085+newt-agent@users.noreply.github.com>`.
  The account name is not repeated in the parenthetical; the qualifier holds only the
  harness version and the build.
- When you amend or reword, keep whatever attribution the commit already has.

---
name: commit-attribution
description: Credit the model and the harness on commits you author with a Co-authored-by trailer, in the house format. Covers which commits need a hand-written trailer and which the harness already attributes.
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

- **Harness-managed commits** (made through the harness's git tool) record every
  contributing model/harness pair from the attribution ledger by themselves. Do not
  add a second trailer by hand.
- **A commit you make through the shell `git` path bypasses the harness's automatic
  trailer.** Credit yourself with a `Co-authored-by:` trailer naming the model and
  harness, e.g.
  `Co-authored-by: <model> (v<version> <build>) <309460085+newt-agent@users.noreply.github.com>`.
  The account name is not repeated in the parenthetical — the address already
  carries it, so the qualifier holds only the harness version and the build it was
  compiled from. The harness identity is the newt-agent GitHub App:
  <https://github.com/apps/newt-agent>.
- When you amend or reword, keep the trailer.

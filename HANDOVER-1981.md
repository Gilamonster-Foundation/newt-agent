# Handover — #1981 slash-command consolidation, slice 1

Written at a context budget stop. **Everything described here is committed.**
Nothing is pushed; there is no PR yet.

- **Worktree**: `~/workspaces/.worktrees/slash`
- **Branch**: `feat/slash-settings`
- **Rebased onto**: `999045bd` (includes RegionLease #1986)
- **Commits** (oldest first):
  - `9ddc4da4` docs: the authoritative slash inventory — 75 top-level, not 25
  - `3126cf1c` docs: the skipper that ate 760 production lines
  - `10e8357a` feat: the slash-command registry + the ratchet
  - `9c456b10` feat: `/settings` — the typed form absorbing editor mode

## Status against the four deliverables

| # | deliverable | state |
|---|---|---|
| 1 | authoritative inventory | **done** — `docs/decisions/slash_command_inventory.md` |
| 2 | decision doc: per-command absorb/keep/delete + receipts | **NOT STARTED — this is the remaining work** |
| 3 | `/settings` carrying its first family | **done** — `newt-tui/src/settings_form.rs` |
| 4 | ratchet on registered commands | **done** — `newt-tui/src/slash_registry.rs` |

> **The form commit is complete** (`9c456b10`). The next context should NOT
> redo it. The only outstanding deliverable is #2, the decision doc.

## What is left, concretely

Write `docs/decisions/slash_command_target_set.md`: one row per command —
**absorb into `/settings` / keep as a verb / delete** — plus, for every
surviving state-mutator, **where its receipt lands** (#1965).

**Generate the table from `slash_registry::COMMANDS`, do not hand-copy it.**
The registry already carries `family`, `disposition` and `receipt` for all 65
commands; a hand-written second list is exactly the drift this slice exists to
kill. A small script over the `cmd(...)` entries is the intended route.

The deciding line, from the operator: **a verb that merely SETS A VALUE is
absorbed; a verb that PERFORMS something stays.** `Panel` is the third case —
a chooser needing a usable region, sequenced behind the RegionLease work.

## Findings that shape the doc

1. **The premise was 3× off.** `dispatch_slash` is the LAST resort, not the
   dispatch: it handles 25 tokens; 50 more are claimed earlier by a `chat.rs`
   `if` chain, ten `parse_*` helpers in `lib.rs`, and `navigator_cmds`. Final
   surface: **65 commands / 79 tokens** (64/78 before `/settings`).
2. **A fourth dispatch shape exists.** `tool_round_limit_command_arg` uses an
   ARRAY of names with `find_map`/`strip_prefix`. It hides two unadvertised
   aliases: `/tool-rounds`, `/max-rounds`.
3. **Eleven reachable commands are advertised nowhere**: `/cognition`,
   `/compact`, `/detail`, `/edit-mode`, `/emacs`, `/markdown`, `/nano`,
   `/quit`, `/tab`, `/tenacity`, `/undo-lock`. Nothing advertised is
   unreachable, so the drift is one-directional: the dispatch outgrew the help.
4. **The tuning knobs are already consolidated — into `/psyche`, not
   `/settings`.** #1665 folded `/tenacity` and `/cognition` into it and left
   `retired_dial_redirect` behind as the shim precedent. `/psyche` is a PANEL,
   so absorbing that family means subsuming a panel. This is the biggest open
   design question and is why slice 1 carried editor mode only.
5. **33 of 64 commands mutate session state and record nothing durable.** That
   is #1965 sized, and it is the reason this is an audit fix rather than
   tidying.

## Open questions for the doc

- Does `/settings` subsume `/psyche`'s text path, with the panel staying a
  separate surface? Or does `/psyche` become `/settings`' rich rendering once
  RegionLease is usable?
- Do the `Panel`-disposition commands (`/backends`, `/permissions`, `/mcp`)
  fold into `/settings` after RegionLease, or stay distinct choosers?
- The navigator's 13 verbs are all `Keep` (they query, they do not set). Worth
  confirming that a `/nav <verb>` grouping is out of scope — it reduces the
  top-level count by 12 but is a keybinding/muscle-memory change the issue's
  non-goals may exclude.

## Traps that cost time — do not re-hit them

- **`crate::production_source` PANICS on `lib.rs` and `chat.rs`.** It splits on
  `"\n#[cfg(test)]\nmod tests {"`, which they no longer contain since #1949
  extracted their test bodies. `slash_registry::tests::production` is the local
  cut written for the declaration form; reuse it.
- **`#[cfg(test)]` does not always introduce a brace block.** In `lib.rs` it
  precedes `use …;` and `#[path = "…"] mod x;`. A skipper assuming a block
  scans to the next `{` anywhere and ate ~760 production lines. The count
  survived it, which is recorded as evidence rather than quietly fixed.
- **Raising a ratchet needs the growth to be the plan.** Slice 1 legitimately
  raised 64→65 / 78→79 / 33→34 because `/settings` is an addition and the
  shims stay. The bound comes DOWN when the deprecation window closes and
  `/vi /emacs /nano /edit-mode` retire. Never raise one to silence a surprise.
- **`Receipt` still carries a scoped `cfg_attr(not(test), allow(dead_code))`.**
  It retires when `settings_form::apply` writes an actual receipt. That is the
  natural next slice after the doc.

## Gates that were green at the stop

```
cargo fmt --all -- --check
cargo clippy -p newt-tui --features rich-tui --all-targets -- -D warnings
cargo clippy -p newt-agent --no-default-features --all-targets -- -D warnings
cargo test -p newt-tui --features rich-tui          # 1247 passed, 0 failed
```

`newt-core` was not modified by this branch. A whole-workspace gate has NOT
been run and needs the operator's go-ahead per box discipline.

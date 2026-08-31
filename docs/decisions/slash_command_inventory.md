# Slash commands: the authoritative inventory

**#1981 slice 1, deliverable 1.** Inventory-first, G0-style. The counting is
the design input, so it is done by **walking the dispatch**, and the number
that flatters the answer is the one to catch.

## The headline correction

| measure | issue's estimate | walked |
|---|---|---|
| top-level commands | ~25 | **75** |
| handled by `dispatch_slash` | (all of them) | **25** |
| intercepted *before* `dispatch_slash` | — | **50** |

**`dispatch_slash` is not the dispatch. It is the last resort.** Two thirds of
the surface never reaches it: `chat.rs` runs a long `if` chain over
`task.trim_start_matches('/')` that handles `/mcp`, `/memory`, `/dock`,
`/docs`, `/byline`, `/loadout`, `/posture`, `/mode`, `/plan`, `/tree`,
`/allow`, `/detail`, `/markdown`, `/compress`, `/compact`, `/undo-lock`,
`/remember`, `/status`, `/info`, `/transcript`, `/persona`, `/conversation`,
`/rename`, `/tab`, `/context`, `/permissions`, `/spill`, `/recall`,
`/resume`, `/roadmap`, `/search` and the whole navigator verb set — and only
falls through to `dispatch_slash` for the 25 it does not claim.

Anyone sizing this work from `dispatch_slash` alone — including this issue's
first scan — undercounts by 3×.

## Method, and why not the help text

Three sources exist. Only one is primary.

1. **The dispatch itself** (primary). Every site that strips a leading `/`
   and compares the result: `dispatch_slash`'s match arms, the `chat.rs`
   interception chain, the `parse_*` helpers in `lib.rs`
   (`parse_persona_command`, `parse_conversation_command`,
   `parse_recall_command`, `parse_resume_command`, `parse_roadmap_command`,
   `parse_compress_command`, `parse_spill_command`, `parse_search_command`,
   `tool_round_limit_command_arg`, `help_request`), and
   `navigator_cmds::parse_nav_command`'s verb match.
2. **`help_lines()`** — what the operator is *told* exists. Secondary: it can
   drift from the dispatch in both directions, and it has.
3. **The palette** — derived from `help_lines()`, so it inherits every drift.
   Not an independent source.

Two scanner bugs were hit and are worth recording, because both are the
"needle counts a name where a call form was needed" family:

* **Cutting production at the first `#[cfg(test)]` truncates `lib.rs` at
  ~line 700** and hides `dispatch_slash` (line 12597) entirely — the scan
  reported zero and looked clean. Fixed with a brace-depth skip, the rule the
  markup ratchet already uses.
* **A first pass modelled only `body == "x"` forms** and missed the
  `match verb { … }` shape the navigator and `/mcp` use after a `split_once`
  hop, undercounting by 18.
* **The brace-depth skip itself was wrong**, and this one is worth the most.
  `#[cfg(test)]` does not always introduce a brace block: in `lib.rs` it
  precedes `use …;` and `#[path = "…"] mod x;` declarations, because #1949
  extracted the test bodies to `lib_tests/`. A skipper that assumed a block
  scanned forward to the next `{` *anywhere* and skipped to its matching
  close — eating ~760 lines of production. Re-running with a skipper that
  distinguishes a block from a declaration yields **the same 75**: the bug
  was real and no command lived in the eaten region. That the number survived
  a scanner that was demonstrably reading the wrong text is better evidence
  for it than the first clean-looking run was.

`production_source`, the crate's own helper, cannot be used on these files
for the same reason: it splits on `\n#[cfg(test)]\nmod tests {`, which
`lib.rs` and `chat.rs` no longer contain, so it would panic rather than
silently truncate. That is the right failure — but it means the registry's
drift test needs its own cut, written against the declaration form.

## The drift, both directions

Comparing the walk against `help_lines()`:

* **11 reachable commands are advertised nowhere**: `/cognition`, `/compact`,
  `/detail`, `/edit-mode`, `/emacs`, `/markdown`, `/nano`, `/quit`, `/tab`,
  `/tenacity`, `/undo-lock`. An operator can only find these by reading
  source. Several are precisely the knob commands this epic proposes to
  absorb.
* **18 advertised tokens** needed the widened walk to locate, and all were
  found — no command is advertised that cannot be reached.

That asymmetry is itself the argument for a registry: the help text and the
dispatch are maintained by hand, separately, and they have already diverged
by eleven entries.

## 75 tokens is not 75 distinct commands

The discipline cuts both ways — the larger number must not flatter the
finding either. Some of the 75 are aliases of one another, and the dispatch
declares them in `|` groups: `exit|quit`, `compress|compact`,
`def|goto`, `text|grep`, `implementations|impls`, `rename|name`,
`cognition|psyche`.

So the honest statement is: **75 reachable top-level tokens, of which ~7 are
pure aliases of a sibling** — a distinct-command count in the high sixties.
Both numbers matter for different reasons: the token count is what a ratchet
must arm at, and the distinct count is what the consolidation must reduce.

## Where this lands

The consolidation target and the per-command disposition
(absorb into `/settings` / keep as a verb / delete), with the receipt
destination for every surviving state-mutator (#1965), build on this table.
The one line that decides each row: **a verb that merely SETS A VALUE is
absorbed; a verb that PERFORMS something stays.**

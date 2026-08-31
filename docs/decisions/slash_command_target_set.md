# The slash target set: absorb, keep, delete — and where the receipt lands

**#1981 slice 1, deliverable 2.** One row per top-level command. The inventory
(`slash_command_inventory.md`) counted the surface; this decides what happens
to each of it.

## The deciding line

From the operator, verbatim:

> A verb that merely **sets a value** is absorbed; a verb that **performs**
> stays.

That single test resolves almost every row, and it resolves the hard ones by
splitting a command rather than by arguing about it — see `/psyche` below.

Three dispositions, not two:

- **absorb** — the verb sets an enum- or value-shaped knob. It becomes a field
  of `/settings` and the verb becomes a shim that keeps working and names its
  new home. A removed command that answers "unknown command" is worse for an
  operator than the verb it replaced.
- **keep** — the verb performs: it probes, lists, writes a file, ends the
  session, moves the cursor. It stays a verb.
- **panel** — a chooser over a *runtime inventory* (configured backends,
  granted capabilities, connected MCP servers) rather than a closed value set.
  Distinct from both, and sequenced behind RegionLease (#1979/#1986).

## Receipts (#1965)

An operator raised a round cap from 40 to effectively unlimited and it left no
record in config, receipts, turns or artifacts. The cause is structural, and
`chat.rs` states it: *slash commands never reach the receipt path.*

So every surviving state-mutator gets a receipt destination in the table, and
`Receipt::Missing` is a finding, not a shrug. The five absorbed knobs now write
a content-addressed `newt_core::settings_receipt` line — one destination for
five verbs, which is the whole argument for absorbing them into one mutation
path instead of instrumenting each one.

## The three open questions, answered

**1. Does `/settings` subsume `/psyche`?** No — and the question dissolves once
the deciding line is applied to `/psyche`'s *parts* rather than its name.
`/psyche` is two things under one word. Its **panel** and its **status view**
perform, so they stay exactly where they are. Its **text setters** (`/psyche
cognition <level>`, `/psyche tenacity <level>`) only ever set, so they are
absorbed: they are now three-line callers of `settings_form::apply_and_record`
that keep their own prose and own no write. `/psyche obsessive` is the same
answer — a named posture that expands to two ordinary setting changes, and it
now takes the same path, the same #1668 posture pins and the same receipt as
`/settings cognition contemplating` would.

The rejected alternative was making `/psyche` the rich rendering of
`/settings`. It buys nothing: the form already renders on the plain scroller,
the RichTUI and the web, and a panel that duplicates it is a second surface to
keep in sync for the sake of a name.

**2. Do the `Panel` commands (`/backends`, `/permissions`, `/mcp`) fold into
`/settings` now that RegionLease has landed?** No — and this is a disposition
answer, not a scheduling one. A `/settings` field has a **closed value set
known statically**: five editor modes, four tenacity levels. Those three
enumerate a **runtime inventory** that has to be discovered — backends read
from config plus their reachability, capabilities that were actually granted,
MCP servers that are actually connected. Listing one is work, not a menu, and
`/backends` also *performs* (it repoints the session and persists the choice).
They stay distinct choosers. What RegionLease unblocks is rendering them as
leased inline panels — an upgrade to how they are shown, not a change to what
they are. `/settings` may one day *link* to them; it does not absorb them.

**3. Should the navigator's 13 verbs become `/nav <verb>`?** Out of scope, and
the reason is the deciding line again: all 13 **query** — they do not set. They
are `Keep` by the rule, so grouping them is a keybinding and muscle-memory
change, which the issue's non-goals exclude ("Not a keybinding redesign"). It
would cut 12 rows off the top-level count while removing no knob and adding no
provenance, which is the count moving without the problem moving.

## What is still owed

- `/rounds` is the command from #1965 itself and is still `Receipt::Missing`.
  It is not absorbable as written: `double`, `reset`, `config` and `unlimited`
  are relative and derived operations, not values, so it performs. It needs a
  receipt destination of its own, not a `/settings` field.
- The ratchet comes **down** by four tokens and one command when the editor
  shims (`/vi`, `/emacs`, `/nano`, `/edit-mode`) retire at the end of the
  deprecation window. Until then the surface is deliberately one command
  larger, and the ratchet comment says so.

## The table

Generated from `newt-tui/src/slash_registry.rs` — every row, including the
counts, comes from `COMMANDS`. A hand-maintained second list of sixty-five
commands is exactly the drift this slice exists to kill, so a test regenerates
this block and fails if it is stale. Update it with:

```
UPDATE_DOCS=1 cargo test -p newt-tui the_target_set_doc_is_generated_from_this_registry
```

<!-- BEGIN GENERATED: slash_registry::COMMANDS -->
| command | also typed as | family | disposition | receipt |
|---|---|---|---|---|
| `/edit-mode` | `/vi` `/emacs` `/nano` | Editor | absorb → `/settings edit-mode` | `~/.newt/receipts.jsonl` |
| `/memory` | — | Memory | keep — it performs | **none — #1965** |
| `/recall` | — | Memory | keep — it performs | — read-only |
| `/remember` | — | Memory | keep — it performs | **none — #1965** |
| `/search` | — | Memory | keep — it performs | — read-only |
| `/byline` | — | Meta | keep — it performs | — read-only |
| `/config` | — | Meta | keep — it performs | — read-only |
| `/docs` | — | Meta | keep — it performs | — read-only |
| `/exit` | `/quit` | Meta | keep — it performs | — read-only |
| `/help` | — | Meta | keep — it performs | — read-only |
| `/info` | — | Meta | keep — it performs | — read-only |
| `/settings` | — | Meta | keep — it performs | `~/.newt/receipts.jsonl` |
| `/setup` | — | Meta | keep — it performs | **none — #1965** |
| `/status` | — | Meta | keep — it performs | — read-only |
| `/version` | — | Meta | keep — it performs | — read-only |
| `/workspace` | — | Meta | keep — it performs | — read-only |
| `/backends` | `/backend` | Model | panel — a chooser, needs a region (#1979) | **none — #1965** |
| `/dgx` | — | Model | keep — it performs | — read-only |
| `/model` | — | Model | absorb → `/settings model` | **none — #1965** |
| `/models` | — | Model | keep — it performs | — read-only |
| `/probe` | — | Model | keep — it performs | — read-only |
| `/summarizer` | — | Model | absorb → `/settings summarizer` | **none — #1965** |
| `/callees` | — | Navigator | keep — it performs | — read-only |
| `/callers` | — | Navigator | keep — it performs | — read-only |
| `/compare` | — | Navigator | keep — it performs | — read-only |
| `/def` | `/goto` | Navigator | keep — it performs | — read-only |
| `/export` | — | Navigator | keep — it performs | — read-only |
| `/hierarchy` | — | Navigator | keep — it performs | — read-only |
| `/impact` | — | Navigator | keep — it performs | — read-only |
| `/implementations` | `/impls` | Navigator | keep — it performs | — read-only |
| `/map` | — | Navigator | keep — it performs | — read-only |
| `/tests` | — | Navigator | keep — it performs | — read-only |
| `/text` | `/grep` | Navigator | keep — it performs | — read-only |
| `/type` | — | Navigator | keep — it performs | — read-only |
| `/uses` | `/refs` | Navigator | keep — it performs | — read-only |
| `/allow` | — | Session | keep — it performs | **none — #1965** |
| `/compress` | `/compact` | Session | keep — it performs | **none — #1965** |
| `/context` | — | Session | keep — it performs | **none — #1965** |
| `/conversation` | — | Session | keep — it performs | **none — #1965** |
| `/crew` | — | Session | keep — it performs | **none — #1965** |
| `/dock` | — | Session | keep — it performs | **none — #1965** |
| `/mcp` | — | Session | panel — a chooser, needs a region (#1979) | **none — #1965** |
| `/permissions` | — | Session | panel — a chooser, needs a region (#1979) | **none — #1965** |
| `/rename` | `/name` | Session | keep — it performs | **none — #1965** |
| `/resume` | — | Session | keep — it performs | **none — #1965** |
| `/roadmap` | — | Session | keep — it performs | — read-only |
| `/spill` | — | Session | keep — it performs | — read-only |
| `/tab` | — | Session | keep — it performs | **none — #1965** |
| `/transcript` | — | Session | keep — it performs | — read-only |
| `/tree` | — | Session | keep — it performs | — read-only |
| `/undo-lock` | — | Session | keep — it performs | **none — #1965** |
| `/cognition` | `/psyche` | Tuning | absorb → `/settings cognition` | `~/.newt/receipts.jsonl` |
| `/detail` | — | Tuning | absorb → `/settings detail` | **none — #1965** |
| `/loadout` | — | Tuning | absorb → `/settings loadout` | **none — #1965** |
| `/markdown` | — | Tuning | absorb → `/settings markdown` | **none — #1965** |
| `/mode` | — | Tuning | absorb → `/settings mode` | **none — #1965** |
| `/nudge` | — | Tuning | absorb → `/settings nudge` | `~/.newt/receipts.jsonl` |
| `/persona` | — | Tuning | absorb → `/settings persona` | **none — #1965** |
| `/plan` | — | Tuning | absorb → `/settings plan` | **none — #1965** |
| `/posture` | — | Tuning | absorb → `/settings posture` | **none — #1965** |
| `/prompt` | — | Tuning | absorb → `/settings prompt` | **none — #1965** |
| `/retrieval` | — | Tuning | absorb → `/settings retrieval` | **none — #1965** |
| `/rounds` | `/tool-rounds` `/max-rounds` | Tuning | absorb → `/settings rounds` | **none — #1965** |
| `/tenacity` | — | Tuning | absorb → `/settings tenacity` | `~/.newt/receipts.jsonl` |
| `/thinking` | — | Tuning | absorb → `/settings thinking` | `~/.newt/receipts.jsonl` |

**65 commands, 79 tokens.** Absorb 17 · keep 45 · panel 3. Receipts: journalled 6 · read-only 31 · **missing 28**.

<!-- END GENERATED -->

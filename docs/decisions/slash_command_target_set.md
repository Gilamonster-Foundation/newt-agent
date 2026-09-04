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

## `/rounds`: a correction to this document (#1998)

This section previously read:

> `/rounds` is the command from #1965 itself and is still `Receipt::Missing`.
> It is not absorbable as written: `double`, `reset`, `config` and `unlimited`
> are relative and derived operations, not values, so it performs. It needs a
> receipt destination of its own, not a `/settings` field.

**That was half right and it drew the wrong conclusion.** The premise stands —
`double` and `unlimited` really are relative operations, not values. But the
conclusion conflated *the verb's affordances* with *the value it lands on*,
which is precisely the confusion the `/psyche` row resolves one level up: a
command is not one thing just because it is one word.

So `/rounds` splits the same way. The verb **performs the derivation** —
doubles the current effective cap, resolves `unlimited` against it, releases
the override — and then the **value it resolved to goes through
`settings_form::apply_and_record`** like every other setting. `/settings rounds
50` and `/settings rounds auto` work; `/rounds double` still works and now
leaves a receipt.

Two things that made this possible rather than merely desirable:

- The session override was a **local variable in `run_chat`**, which is why the
  escalation #1965 documents was unrecoverable: a receipt writer cannot read a
  local. It moved to `newt_core::tenacity` beside the other three inputs to
  `resolve_tool_round_limit`, along with the configured baseline it is measured
  against.
- The form only knew closed vocabularies. It now also knows a **number**, so a
  field whose value is `1..=10000` gets a text field instead of a menu — and is
  still a typed `InteractionDefinition`, so it still renders on the plain
  scroller, the RichTUI and the web.

The receipt's `from`/`to` carry the whole `ToolRoundLimit` — #1982's derivation
record, reused rather than re-declared — so the line says *320, from an
override, over a configured 40, under relentless*, and every one of those
fields is bound into its content address. The alias actually typed
(`/rounds`, `/tool-rounds`, `/max-rounds`) is bound in too.

`/rounds show` records nothing. It reads.

## What is still owed

- **The headless `solve` path is still asymmetric, and #1998 does NOT close
  it.** `newt-cli/src/solve.rs::solve_tool_round_limit` computes the same
  effective cap and discards the derivation, because solve persists no per-turn
  records. That is a different gap from this one: this is *an operator changing
  a setting from the prompt* (an event, now journalled); that is *a run's
  per-turn derivation* in a path with no turn records at all. Writing solve's
  resolved cap into the settings journal would conflate "the operator changed a
  setting" with "a run resolved a cap" and pollute the receipt shape —
  `--max-rounds` is a flag, already recorded by the invocation carrying it. It
  closes when solve grows turn persistence, and the comment in that file says
  so on purpose.
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
| command | also typed as | reached by | family | disposition | receipt |
|---|---|---|---|---|---|
| `/edit-mode` | `/vi` `/emacs` `/nano` | `/` command | Editor | absorb → `/settings edit-mode` | `~/.newt/receipts.jsonl` |
| `/memory` | — | retired → `/status memory` | Memory | keep — it performs | — read-only |
| `/recall` | — | retired → `/resume find` | Memory | keep — it performs | — read-only |
| `/remember` | — | `/` command | Memory | keep — it performs | **none — #1965** |
| `/search` | — | `/` command | Memory | keep — it performs | — read-only |
| `/byline` | — | retired → `/status byline` | Meta | keep — it performs | — read-only |
| `/config` | — | retired → `/status config` | Meta | keep — it performs | — read-only |
| `/docs` | — | retired → `/help docs` | Meta | keep — it performs | — read-only |
| `/exit` | `/quit` | `/` command | Meta | keep — it performs | — read-only |
| `/help` | — | `/` command | Meta | keep — it performs | — read-only |
| `/info` | — | retired → `/status` | Meta | keep — it performs | — read-only |
| `/settings` | — | `/` command | Meta | keep — it performs | `~/.newt/receipts.jsonl` |
| `/setup` | — | `/` command | Meta | keep — it performs | **none — #1965** |
| `/status` | — | `/` command | Meta | keep — it performs | — read-only |
| `/version` | — | retired → `/status version` | Meta | keep — it performs | — read-only |
| `/workspace` | — | retired → `/status workspace` | Meta | keep — it performs | — read-only |
| `/backends` | `/backend` | `/` command | Model | panel — a chooser, needs a region (#1979) | **none — #1965** |
| `/dgx` | — | `/` command | Model | keep — it performs | — read-only |
| `/model` | — | `/` command | Model | absorb → `/settings model` | **none — #1965** |
| `/models` | — | retired → `/status models` | Model | keep — it performs | — read-only |
| `/probe` | — | `/` command | Model | keep — it performs | — read-only |
| `/probe reset` | — | action inside a section | Model | keep — it performs | **none — #1965** |
| `/summarizer` | — | `/` command | Model | absorb → `/settings summarizer` | **none — #1965** |
| `/callees` | — | `/` command | Navigator | keep — it performs | — read-only |
| `/callers` | — | `/` command | Navigator | keep — it performs | — read-only |
| `/compare` | — | `/` command | Navigator | keep — it performs | — read-only |
| `/def` | `/goto` | `/` command | Navigator | keep — it performs | — read-only |
| `/export` | — | `/` command | Navigator | keep — it performs | — read-only |
| `/hierarchy` | — | `/` command | Navigator | keep — it performs | — read-only |
| `/impact` | — | `/` command | Navigator | keep — it performs | — read-only |
| `/implementations` | `/impls` | `/` command | Navigator | keep — it performs | — read-only |
| `/map` | — | `/` command | Navigator | keep — it performs | — read-only |
| `/tests` | — | `/` command | Navigator | keep — it performs | — read-only |
| `/text` | `/grep` | `/` command | Navigator | keep — it performs | — read-only |
| `/type` | `/inspect` | `/` command | Navigator | keep — it performs | — read-only |
| `/uses` | `/refs` | `/` command | Navigator | keep — it performs | — read-only |
| `/allow` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/cd` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/compress` | `/compact` | `/` command | Session | keep — it performs | **none — #1965** |
| `/context` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/conversation` | — | retired → `/resume` | Session | keep — it performs | **none — #1965** |
| `/crew` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/dock` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/end` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/mcp` | — | `/` command | Session | panel — a chooser, needs a region (#1979) | **none — #1965** |
| `/new` | `/clear` | `/` command | Session | keep — it performs | **none — #1965** |
| `/permissions` | — | `/` command | Session | panel — a chooser, needs a region (#1979) | **none — #1965** |
| `/rename` | `/name` | `/` command | Session | keep — it performs | **none — #1965** |
| `/restart` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/resume` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/roadmap` | — | `/` command | Session | keep — it performs | — read-only |
| `/spill` | — | `/` command | Session | keep — it performs | — read-only |
| `/start` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/tab` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/transcript` | — | `/` command | Session | keep — it performs | — read-only |
| `/tree` | — | `/` command | Session | keep — it performs | — read-only |
| `/undo-lock` | — | `/` command | Session | keep — it performs | **none — #1965** |
| `/cognition` | — | retired → `/settings cognition` | Tuning | absorb → `/settings cognition` | `~/.newt/receipts.jsonl` |
| `/detail` | — | `/` command | Tuning | absorb → `/settings detail` | **none — #1965** |
| `/loadout` | — | retired → `/status loadout` | Tuning | keep — it performs | — read-only |
| `/markdown` | — | `/` command | Tuning | absorb → `/settings markdown` | `~/.newt/receipts.jsonl` |
| `/mode` | — | `/` command | Tuning | absorb → `/settings mode` | `~/.newt/receipts.jsonl` |
| `/nudge` | — | `/` command | Tuning | absorb → `/settings nudge` | `~/.newt/receipts.jsonl` |
| `/persona` | — | `/` command | Tuning | absorb → `/settings persona` | **none — #1965** |
| `/plan` | — | `/` command | Tuning | absorb → `/settings plan` | **none — #1965** |
| `/posture` | — | `/` command | Tuning | absorb → `/settings posture` | **none — #1965** |
| `/prompt` | — | `/` command | Tuning | absorb → `/settings prompt` | `~/.newt/receipts.jsonl` |
| `/psyche` | — | `/` command | Tuning | keep — it performs | `~/.newt/receipts.jsonl` |
| `/retrieval` | — | `/` command | Tuning | absorb → `/settings retrieval` | **none — #1965** |
| `/rounds` | `/tool-rounds` `/max-rounds` | `/` command | Tuning | absorb → `/settings rounds` | `~/.newt/receipts.jsonl` |
| `/tenacity` | — | retired → `/settings tenacity` | Tuning | absorb → `/settings tenacity` | `~/.newt/receipts.jsonl` |
| `/thinking` | — | retired → `/settings thinking` | Tuning | absorb → `/settings thinking` | `~/.newt/receipts.jsonl` |

**72 registered, 57 of them typed as `/` commands (72 tokens).** Absorb 16 · keep 53 · panel 3. Receipts: journalled 11 · read-only 33 · **missing 28**.

<!-- END GENERATED -->

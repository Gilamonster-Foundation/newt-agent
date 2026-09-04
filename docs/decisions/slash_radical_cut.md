# Radical slash cut: one cockpit, sixteen verbs, one table

**Status:** Proposed — awaiting operator approval or amendment
**Supersedes:** the scope clause of `docs/decisions/harness_config_panel.md`; Q1, Q2 and Q3 of `docs/decisions/slash_command_target_set.md` (superseded text quoted in place, #1999 pattern)
**Depends on:** #1994 (registry + form + receipts), #1999 (/rounds through the recorder), #1986/#1990 (RegionLease), #2003/#2004 (help-drift guard + PTY acceptance). Main at `bff1967c`.

---

## 0. The amended deciding line

The operator's line from #1981 stands as written and is quoted here permanently:

> "a verb that merely sets a value is absorbed; a verb that performs stays"

Tonight's directive amends it from the same authority:

> "I want to radically simplify the '/' commands by leaning heavily on a very very rich '/settings' TUI" · "I want to eliminate most of the other '/' surfaces with a few specific exceptions."

**The amended rule.** A verb survives only if **all four** hold:

1. it **performs** — it does not merely set a value;
2. it is wanted **mid-turn or mid-task**, where a modal shell is barred from opening (`plain_scroller_tui.md` condition 1);
3. its answer must be **one typed line on a pipe** — a headless dependent (`newt solve` piped, `newt-acp-worker`, the eval harness, newt-as-a-wyvern-worker) uses it;
4. it is **charter-barred from or nonsensical inside** `/settings` (renders conversation/tool/agent content, or is the escape hatch used when the shell is wedged).

Everything else is a section, a field, or a deep link. Sixteen verbs pass. Sixty-five commands become sixteen.

---

## 1. The exception list — sixteen verbs

| verb | one-line justification |
|---|---|
| `/settings` | the cockpit itself — every value journey ends here |
| `/help` | the discovery bootstrap; a section of /settings cannot teach you /settings exists (absorbs `/docs` as a topic) |
| `/exit` (`/quit`) | leaving is one keystroke and must work when every richer surface is wedged |
| `/status` | the mid-task glance — read-only scrollback, absorbs 8 read verbs as topics, zero receipts by construction, and the shape a piped script consumes |
| `/new` (+ lifecycle family) | session boundaries are acts, not values; **semantics read precedes registration** (see §5 PR2) |
| `/resume` | the front door back into work: a chooser over a runtime inventory with a free-text query, absorbs `/recall` + `/conversation` |
| `/transcript` | viewer over conversation content — charter-barred from /settings |
| `/compress` (`/compact`) | the context-wedged rescue, wanted exactly mid-turn when the modal is barred |
| `/remember` | deterministic verbatim capture without spending an inference turn; free prose tail, works on a pipe |
| `/dock` | security kill-switch — emergencies do not navigate menus, and it must work headless on a wedged box |
| `/roadmap` | plan-loop performer (next/done/drive) rendering work-product; absorbs `/tree`, `/plan` |
| `/nav <verb> <args>` | 13 deterministic zero-inference code queries + `/search` + `/retrieval` under one prefix — **OPERATOR-CALL** (§6 Q1) |
| `/spill` | mid-run inspection of truncated tool output — charter-barred content viewer — **OPERATOR-CALL** (§6 Q4) |
| `/cd` | shell reflex; registered into the registry for the first time |
| `/dgx` | fleet ops with long-running progress on the daily-driver box; the shell has no progress machinery — **OPERATOR-CALL** (§6 Q2) |
| `/tab` | window switching; Alt chords are broken on macOS (the recorded reason slash twins exist, chat.rs:3786) — **OPERATOR-CALL** (§6 Q3) |

Rejecting Q1–Q4 as recommended moves the endgame between **13 and 18** commands. The `!` bang escape is not a slash command and is untouched.

---

## 2. Full disposition table (all 65 + ghosts)

Endgame surfaces: **VERB** (exception) · **Native** (a settings field, no slash token) · **Section** / **SectionAction** (inside the cockpit, reachable by deep link) · **Retired** (permanent pointer row; names its replacement forever, never dispatches).

### Editor
| cmd | endgame | receipt | slice |
|---|---|---|---|
| `/edit-mode` (`/vi` `/emacs` `/nano`) | Native (Field::EditMode, landed) | Journal (landed) | done; shims die PR14 (−1 cmd, −4 tokens, pre-declared) |

### Memory
| cmd | endgame | receipt | slice |
|---|---|---|---|
| `/memory` | Retired → `/status memory` | **truthing:** row is `Missing` with a read-only description — verify; if it writes nothing, reclassify `None_` **with the argument recorded in this doc** | PR1 (truthing), PR3 |
| `/recall` | Retired → `/resume find` | None_ | PR6 |
| `/remember` | **VERB** | parked Missing → event journal (§4.4). Not reclassified. | survives |
| `/search` | Retired → `/nav search` (whole verb; pin/exclude/rejects are per-run retrieval state, not settings) | None_ | PR11 |

### Meta
| cmd | endgame | receipt | slice |
|---|---|---|---|
| `/byline` | Retired → `/status byline` | None_ | PR3 |
| `/config` | Retired → `/status config` (show half); the edit half **is** /settings | None_ | PR3 |
| `/docs` | Retired → `/help docs` | None_ | PR3 |
| `/exit` (`/quit`) | **VERB** | None_ | — |
| `/help` | **VERB** | None_ | — |
| `/info` | Retired → `/status` (default view) | None_ | PR3 |
| `/settings` | **VERB** — the cockpit | Journal (landed) | PR4–PR13 |
| `/setup` | SectionAction `/settings backends add` (wizard re-homed); fresh-box auto-open **dual-gated: TTY ∧ interactive origin**, else one plain line naming the deep link | Journal on the resolved backend; secrets stay in the encrypted store, the receipt records *that* a key was set, never the key | PR9 |
| `/status` | **VERB** — topics: info (default), config, version, workspace, loadout, byline, memory, models | None_ by construction | PR3 |
| `/version` | Retired → `/status version` | None_ | PR3 |
| `/workspace` | Retired → `/status workspace` | None_ | PR3 |

### Model
| cmd | endgame | receipt | slice |
|---|---|---|---|
| `/backends` (`/backend`) | Section `/settings backends` — **Q2 fold #1** (§3.5); LINK-mode fallback if the locals precondition slips | Journal on the resolved choice via `apply_and_record`; Missing −1 | PR9 |
| `/dgx` | **VERB** (OPERATOR-CALL Q2) | reads None_; route changes → Journal as a backend field; warm/pull/rm parked Missing → event journal | survives |
| `/model` | Native (runtime-seeded Choice built at ask time) | Journal; Missing −1 | PR9 |
| `/models` | Retired → `/status models` **and** visible in the Backends section (headless read vocabulary stays uniform) | None_ | PR3 |
| `/probe` | SectionAction `/settings backends probe`; `probe reset` is a real state wipe | reads None_; **`probe reset` is a truthing raise in PR1** — an unrecorded mutator registered as Missing, destination = event journal | PR1, PR9 |
| `/summarizer` | Native field(s) | Journal; Missing −1 | PR9 |

### Navigator (13) — all Retired → `/nav <verb>`, **each keeping its own help line**
`/callees` `/callers` `/compare` `/def`(`/goto`) `/export` `/hierarchy` `/impact` `/implementations`(`/impls`) `/map` `/tests` `/text`(`/grep`) `/type` (+ ghost alias `inspect`) `/uses`(`/refs`) — all `None_`, all **PR11**. The N1 verb match becomes `/nav`'s parser (one site, recounted, not removed).

### Session
| cmd | endgame | receipt | slice |
|---|---|---|---|
| `/allow` | SectionAction `/settings permissions allow …` (resolves the fake-alias registration) | grants are events, not value transitions → **parked Missing row retained**, destination = event journal | PR10 |
| `/compress` (`/compact`) | **VERB** (action half); trigger/policy knobs → Context fields | run derivation, deliberately out of the settings journal; **parked Missing, not reclassified** | PR7 splits |
| `/context` | Section Context (strategy, features, trigger as fields; stats as view) | Journal per field; Missing −1 | PR7 |
| `/conversation` | Retired → `/resume` (list/show/restore/rename/delete as chooser actions) | parked Missing → event journal | PR6 |
| `/crew` | Section `/settings crew` (existing `crew_form` re-homed) | Journal; Missing −1 | PR12 |
| `/dock` | **VERB** (kill-switch); peers also read-only in Permissions | `disable` parked Missing → event journal, sequenced **before** window close | survives |
| `/mcp` | Section `/settings mcp`; LINK-mode fallback if the `mcp`-to-core precondition slips | durable config writes Journal; Missing −1; session mute is session-scoped state → parked, no scope field is added to `SettingChange` | PR10 |
| `/permissions` | Section `/settings permissions` (posture field + audit + grants + decision reopen) | posture Journal (Missing −1); audit None_; grants/reopen parked SectionAction rows | PR10 |
| `/rename` (`/name`) | **VERB** — Q5 answered *conversation metadata* (see §7 Q5); the free-text carve-out was ported anyway and generalized to `Text` fields | parked Missing → event journal, with the conversation ops | PR5 (answered) |
| `/resume` | **VERB** — absorbs `/recall` + `/conversation` | run-state; parked | PR6 grows it |
| `/roadmap` | **VERB** — absorbs `/tree`, `/plan` | parked Missing → event journal | PR12 |
| `/spill` | **VERB** (OPERATOR-CALL Q4); its `detail` knob leaves for Context | views None_; `reset` parked Missing | PR7 splits knob |
| `/tab` | **VERB** (OPERATOR-CALL Q3) | parked Missing (view state) | survives |
| `/transcript` | **VERB** | None_ | — |
| `/tree` | Retired → `/roadmap tree` | None_ | PR12 |
| `/undo-lock` | SectionAction `/settings permissions decisions reopen` — deep link is line-based so it works mid-flow where the modal cannot | **row retained as parked Missing** (#1749 reversals stay counted until the event journal lands) | PR10 |

### Tuning
| cmd | endgame | receipt | slice |
|---|---|---|---|
| `/cognition` (`/psyche`) | Native (landed); the `/psyche` **panel** becomes the Session section view (Q1 superseded by argument, §3.6) | Journal (landed) | panel fold PR8 |
| `/detail` | Native (Context) | Journal; Missing −1 | PR7 |
| `/loadout` | Retired → `/status loadout` (+ Audit section resolution view) | **truthing:** row is `Missing` on a read-only description — verify and reclassify `None_` **with the argument recorded**, exactly as `/memory` | PR1, PR3 |
| `/markdown` | Native | Journal; Missing −1 | PR4 |
| `/mode` | Native | Journal; Missing −1 | PR4 |
| `/nudge` | Native (landed) | Journal (landed) | done |
| `/persona` | **VERB** — it PERFORMS (criterion 1): `chat.rs:5500` rotates the conversation id, then resets memory, the system prompt, the artifact store, the preference pin and the backend route. A CRUD panel is its own later slice | parked Missing → event journal | corrected in PR5 |
| `/plan` | Retired → `/roadmap` (old Absorb row quoted in place) | never writes; redirect says "(nothing changed)" | PR12 |
| `/posture` | Native (Permissions) | Journal; Missing −1 | PR10 |
| `/prompt` | Native (Text); **quoted-template parsing preserved in the deep link**; rich edit via the panel field editor, never `InteractionView` | Journal; Missing −1 | PR5 |
| `/retrieval` | Retired → `/nav retrieval` — mis-registration fixed (only live handler is the nav ledger); old Absorb row quoted in place | None_ | PR11 |
| `/rounds` (`/tool-rounds` `/max-rounds`) | Native (landed; derivation verbs survive as `/settings rounds double`) | Journal (landed, recorder-pinned) | shims die PR14 |
| `/tenacity` | Native (landed; retired redirect stands) | Journal (landed) | done |
| `/thinking` | Native (landed) | Journal (landed) | done |

### Ghost annex (unregistered today — outside every ratchet)
| token | endgame | note |
|---|---|---|
| `/cd` | **VERB**, registered; `strip_prefix` shape rewritten into the counted interception form | PR2 |
| `/new` `/clear` `/end` `/restart` `/start` | registered **after a semantics read** — chat.rs:4487–4504 shows `/start` switches *without* finalizing while `/new` finalizes with note extraction. Only tokens proven identical become aliases; distinct behaviors keep distinct rows or become `/new` arguments | PR2 |
| nav alias `inspect` | registered alias of `/nav type` | PR11 |
| `!` bang escape | untouched — not a slash command | — |

---

## 3. The cockpit

### 3.1 Sections (index order = journey frequency)

1. **Session** — tenacity, cognition, thinking, nudge, rounds, mode, edit-mode, markdown, persona, prompt template, title
2. **Backends** — active backend, model, summarizer (runtime-inventory menus built at ask time); add (the old `/setup` wizard flow), edit, remove, probe, models matrix
3. **Permissions** — posture field, grants, prompted-decision audit, decision reopen (old `/undo-lock`), dock peers (read-only)
4. **MCP** — server list, session mute, durable enable/disable/auth
5. **Context** — strategy, feature toggles, compaction trigger policy, spill detail, memory/stats view
6. **Crew** — the existing `crew_form` wizard re-homed
7. **Audit** — receipts-journal viewer (`read_jsonl` + `is_intact` per line), loadout resolution, resolved-config dump

**Hard exclusion, carried forward from `harness_config_panel.md` constraint 2:** no section ever renders conversation, tool output, or agent-produced content. That cap is what stops /settings becoming a second conversation surface, and it is why `/transcript`, `/spill`, `/resume` (whose chooser shows conversation search hits) and `/roadmap` are verbs, not sections.

### 3.2 Navigation

Index = section list with current-value summaries (the shipped `field_menu` grown; it already shows values per row). Accelerator keys per section. `/` inside the shell filters rows fuzzily — **over the one registry table**, which is also the source the help corpus and palette derive from, so it is not a second command list (`palette.rs:11-15`).

### 3.3 Per-surface story

**RichTUI** — one parent event loop hosting the existing pure `PanelState`s as section states under **one** `RegionLease`. Open with `Shift` (config_panel precedent). **Resize uses `relocate` with `Refuse` (checked request) or `SuspendHolder` (report of a done move) — never `Shift`, which `relocate` rejects outright** (`arbiter.rs:332`). `InlineGuard`/`RawModeGuard` for raw-mode discipline (never the bare `enable_raw_mode` pair). Compile-gated to `rich-tui`, runtime-TTY-gated, operator-invoked, **never mid-turn**, RAII restore on every exit path including panic, reads durable state and never redefines it.

**Plain scroller / lean** — every section keeps a typed `InteractionDefinition` projection walked with the `crew_form` `Step::next` / `Fold` machine. The lean answer stays scrolled GFM lines, so the revisit rule never fires. Committed output is unchanged on every surface.

**Headless / piped** — three rules, all testable:
- **Reads print; they never open.** `/settings <section>` on a non-TTY prints the section's rows as plain lines. `/status <topic>` is a verb and always prints. A section *browse* never becomes the only route to a datum a script reads today.
- **Never prompts.** Bare `/settings` on a pipe prints the index and exits; EOF/Esc/Ctrl-C are `HumanQuestionOutcome` variants, never an implied answer. The `/setup` fresh-box auto-open is gated on **TTY ∧ interactive origin** and degrades to one line naming `/settings backends add`.
- **Discovery is bounded.** Every section action that performs runtime discovery (backend reachability, MCP connection, probe) takes a deadline; on expiry it prints an honest refusal naming what was unreachable. No spinner, no cursor controls, no hang. This is a named acceptance test, not a note.

**Web** — **parked, not claimed cheap.** `newt-web/src/shell.rs:341-352` extracts exactly one `ControlKind::Choice` and fail-closes to an empty string otherwise, and the permission gate is today's only publisher into the offer store. Multi-section web is a real multi-control renderer plus offer-store wiring, and `apply_and_record` is `pub(crate)` in `newt-tui` — a web write needs either a deliberate visibility change with a new pinned-call guard, or the mutation to move to core. Out of this train; filed separately.

### 3.4 Free text

`InteractionView` renders `ControlKind::Text` as a static row and returns `Cancelled` on Enter with no options (`interaction_view.rs:200-208`). So: **rich** free text uses the panel field editor (`backend_panel` FIELDS) or `rich_input`, never the interaction widget; **plain** uses the existing modal line; **headless** uses deep-link trailing args, with the `help_request` free-text carve-out (lib.rs:12529) ported to `/settings title <text>` and the quoted-template parsing (`commands/settings.rs:40-75`) preserved for `/settings prompt set "<template>"`.

### 3.5 Q2 answered, three reasons and three answers

> *Superseded (target_set.md:59-69):* "A `/settings` field has a closed value set known statically; these enumerate a runtime inventory that has to be discovered… Listing one is work, not a menu." · "`/backends` also performs." · "`/settings` may one day link to them; it does not absorb them."

1. **Runtime inventory.** The `InteractionDefinition` machinery never required static vocabularies — only the `Field` rows did. A section loader performs the discovery `backend_panel`'s `PanelSeed` resolution performs today (`backend_panel.rs:170-183`), then builds the menu. *Same work, new home.* The doc's sharp half — listing is work that can hang or half-fail — is answered by §3.3's bounded-discovery rule: a deadline, a partial-inventory render that marks what was unreachable, and an honest refusal on a pipe.
2. **`/backends` performs.** The #1999 template scales: the chooser performs discovery and the switch through the same owner (`apply_backend_choice`), and the resolved choice lands through `apply_and_record`, finally paying the panel's `Receipt::Missing` debt. **The #1999 precondition is carried, not assumed** — see §5's precondition gate.
3. **Link, don't absorb.** Kept as a **live declared fallback**, not spent: if the parent-loop work in PR8/PR9 slips, or a precondition slice stalls, the section lands in **LINK mode** — the /settings index links to today's panel, which keeps the receipts and the index entry without the single surface. Q2's concession is the retreat, and it is written into the migration.

### 3.6 Q1, cited correctly

> *Superseded (target_set.md:43-57):* the `/psyche` panel and status view "perform, so they stay exactly where they are"; the "second surface to keep in sync for the sake of a name" line was the doc's reason for **rejecting** a rich rendering of /settings, on the ground that the form already renders on three surfaces.

All three designs cited the rejection as licence to build the thing it rejected. The honest answer: **the panel's performance is dial editing**, and the cockpit's Session section reproduces it under one lease through the same setters, with receipts the panel never wrote. We supersede Q1 by argument — one surface, same setters, receipts gained — not by misquoting it. The `/psyche` verb gets a `retired_dial_redirect` naming the Session section, zero-write, wording-pinned.

### 3.7 Q3, answered by the directive plus a mechanism

> *Superseded (target_set.md:71-77):* grouping the 13 navigator verbs "would cut 12 rows off the top-level count while removing no knob and adding no provenance, which is the count moving without the problem moving."

The residue is real and the directive answers only half of it (tonight the count *is* the problem). The other half is answered mechanically: **each `/nav` subverb keeps its own `help_lines()` row**, so the palette still fuzzy-completes `callers`, discovery is not spent to buy the number, and the #2003 ratchet covers every subverb. If the operator rejects the fold, the 13 verbs return and the endgame is ~29 commands (§6 Q1).

---

## 4. Receipts and provenance

### 4.1 One table, one receipt authority

The registry stops being a *slash-command* table and becomes the **register of settings and commands**. Rows gain a surface:

```
Surface::Slash        // dispatchable verb (Keep / Absorb / Panel as today)
Surface::Native       // a settings field with no slash token
Surface::SectionAction// an operation inside a cockpit section
Surface::Retired{replacement}  // permanent pointer data; never dispatches
```

This single change resolves three findings at once:

- **Row-death no longer disarms receipts.** `receipt_for()` stays exactly what it is: one fail-closed lookup on one table (`slash_registry.rs:583-585`). An absorbed verb's row does not die at window close — it becomes `Native` and keeps its `Receipt::Journal` declaration. No second table, no disjunction, no forgeable destination.
- **Section actions cannot launder.** They are rows. `the_receiptless_state_mutators_are_counted_and_only_shrink` counts `Receipt::Missing` across **all** rows, whatever their surface, so moving `/undo-lock`'s reopen or `/probe reset` inside a section does not remove it from the debt.
- **`every_settings_field_is_registered_as_absorbed` stays a real join**, widened to accept `Slash+Absorb` **or** `Native`, and made **bidirectional**: every `Field` names exactly one row, and every `Native` row names exactly one `Field`. Likewise every `SectionAction` row must be named by a section's action table. You cannot hide a command as a field or an action.

### 4.2 Ratchets, redefined once and declared

| guard | new form |
|---|---|
| surface only shrinks | `slash_dispatchable_commands() <= 65` and `slash_tokens() <= 79`, counting **only** `Surface::Slash` rows; anti-vacuous: the count must be ≥ the exception-list size |
| total rows | may grow (fields + retired pointers), pinned by the bidirectional joins above, never by a bare `len()` |
| receiptless mutators | `Missing` counted across **all** surfaces; shrink-only **after PR1** |
| interception sites | unchanged, `== 21` exact, edited only on a real recount |
| retired pointers | every `Retired{replacement}` must resolve to a live row or field — no dangling pointers, ever |

**One declared truthing raise, in PR1 only.** PR1 may *raise* the `Missing` count, itemized line by line, to register mutators that exist today and are registered as read-only (`probe reset` is the confirmed case). This is "growth is the plan" in its intended sense: the alternative is laundering. After PR1 the number only falls. Symmetrically, PR1 reclassifies `Missing`→`None_` **only** for rows verified to write nothing (`/memory`, `/loadout` — both `Missing` on read-only descriptions), and **each reclassification carries its argument in this doc**. No row is reclassified by convenience: `/compress`, `/remember`, `/spill reset` stay counted.

### 4.3 The mutation path is unchanged

`apply` stays private; `apply_and_record` is the only route; `via` is the verb the operator actually typed; no-ops are recorded; recording is best-effort and never load-bearing; aliases survive in `accepts()`; values canonicalize so one value is one address.

**New: `via` for in-shell edits.** A cockpit edit records `via = "/settings <section>.<field>"` and applies **on commit** (`:w`, or Enter on the value menu), never per keystroke — a dial swept across five values writes one receipt, not five.

### 4.4 Operations do not go in the settings journal

`SettingChange` is a from→to transition on a named setting, and `apply_and_record` derives `from` from `field.value_now()`. A grant, a note append, a dock kill, a conversation delete, a decision reopen has no prior value. **Therefore this design mints zero new `SettingValue` variants for operations** (against nine in one alternative and five in another). Those rows stay `Missing` and *counted*, with a named destination: an **event journal** — a separate, content-addressed record type filed as its own issue and **sequenced before the window close** (§5 PR-E). If it slips, the parked set stays parked and honest; it never gets a fabricated baseline and never gets reclassified.

Any genuinely new *value* type (none is currently required) widens `SettingValue` wire-disjointly with a named disjointness regression test — `#[serde(untagged)]` means a variant that serializes like a bare string re-addresses every receipt on disk.

The solve-path separation stands: `--max-rounds` on a headless run is recorded by the invocation carrying it, not by the settings journal.

### 4.5 The wyvern lens on where to spend

Durable: the field/section schema (`InteractionDefinition`s), the `settings_receipt` format, the config-key and permission-name vocabulary, the registry and its conformance tests. Disposable: the ratatui shell. PR8's rendering work is written knowing the rewrite discards it, and no TUI code moves down into wyvern.

---

## 5. Migration train

Every slice: `UPDATE_DOCS=1 cargo test -p newt-tui the_target_set_doc_is_generated_from_this_registry`; regenerate `newt-eval/tests/golden/help-surface.golden` when `help_lines()` moves; update the scattered containment assertions it breaks (`lib_tests/skills_integration.rs`, `env_resolution.rs`, `core.rs`); extend the PTY grid (`settings_form_pty_test.rs`); `scripts/docs_check.py`; zero clippy; one issue per PR; no push to main; a mutation-proof red list in the PR body naming the test that goes red per stubbed mechanism. The palette never needs an edit — it is corpus-derived.

**The precondition gate (applies to PR7, PR9, PR10).** No section lands until the state its receipts must read lives in core — `a receipt writer cannot read a local` (#1999's stated precondition; the locals are named in chat.rs:3036/3049/3233). Each of those slices **opens with its own relocation step**, or the section lands in **LINK mode** (§3.5 answer 3). This is why `/resume`, `/compress`, `/spill`, `/roadmap`, `/dock` stay verbs: they keep their chain blocks and their locals, and need no relocation at all.

**Site-count honesty.** A shared `let slash_* =` binding survives until its *last* command dies, so a slice may only lower the pinned 21 when a real recount says so. Slices that actually free a site: PR3 (C2), PR6 (L2, L3), PR7 (C5/L6 partial), PR9 (C9 backend gate + model arms), PR10 (C1), PR11 (N1 becomes /nav's), PR14 (the rest). Other slices touch the `assert_eq!` only to reconfirm it.

| PR | content | ratchet / UAT reflection |
|---|---|---|
| **PR0** | this doc. Supersede `harness_config_panel.md` scope (carrying its five constraints); quote Q1/Q2/Q3 in place; record the amended deciding line and the four-part criterion | docs only; `docs_check.py` |
| **PR1** | **registry truthing + vocabulary.** Add `Surface::{Native,SectionAction,Retired}`; redefine the two surface ratchets onto `Surface::Slash`; count `Missing` across all surfaces; bidirectional field↔row and action↔row joins; retired-pointer resolution guard; the one declared truthing raise (probe reset et al.) and the two verified reclassifications with arguments | no surface change; declared `Missing` raise, itemized; mutation-proofed (stub the join → named test red) |
| **PR2** | **ghosts + lifecycle semantics.** Read chat.rs:4487-4504 *first*; register `/cd`, the lifecycle family (aliases only where behavior is proven identical), `inspect`; rewrite `/cd`'s `strip_prefix` into the counted shape | declared ratchet raise (growth is the plan); site pin edited in the same change; all new rows ship advertised |
| **PR3** | **/status fold** — 8 topics + `/docs`→`/help docs`; 9 rows become Retired | C2 dies → pin recount; golden + containment |
| **PR4** | Session fields A: `mode`, `markdown` | Missing −2; PTY grid +2 |
| **PR5** | Session fields B: `persona`, `prompt` (quoting preserved), `title` (carve-out ported) | Missing −3; OPERATOR-CALL checkpoint Q5 |
| **PR6** | `/resume` absorbs `/recall` + `/conversation` | L2/L3 die → recount; conversation ops parked, counted |
| **PR7** | Context section + splits (`/compress` action stays a verb, knobs become fields; `/spill detail`→field). **Precondition step included** | Missing −2; LINK-mode fallback declared |
| **PR8** | **the rich shell** — parent loop, one lease (`Shift` open, `Refuse`/`SuspendHolder` resize), index, filter; `/psyche` panel → Session section view with a zero-write retired redirect | no command changes; PTY acceptance for index + section entry; plain_scroller conditions enforced |
| **PR9** | **Backends section** — folds `/backends` `/model` `/models` view `/probe` `/setup` `/summarizer`. Setup auto-open dual-gated. **Post-command reload regression test** (chat.rs:6189 divergence) | Missing −3; big site recount; PTY backends grid |
| **PR10** | **Permissions + MCP sections**. **Precondition step included** (`mcp`, `active_posture` to core) or LINK mode | Missing −1 (posture); grants/reopen stay parked rows; C1 dies |
| **PR11** | **/nav** — register; 13 + `/search` + `/retrieval` retire to subverbs, **one help line each** | N1 becomes /nav's parser (recount, not removal); KNOWN_UNADVERTISED loses 4 |
| **PR12** | Crew section; `/roadmap` absorbs `/tree` and `/plan` (old rows quoted) | Missing −1 |
| **PR13** | Audit section — receipts viewer (`read_jsonl` + `is_intact`), loadout resolution, config dump | read-only; no ratchet moves |
| **PR-E** | **event journal** (separate issue): content-addressed record for grants, kills, reopens, note appends, compressions, conversation ops. Sequenced before window close | pays the parked set; if it slips, the set stays parked and counted |
| **PR14a–d** | **window close, one release, four slices**: (a) absorbed fields migrate to `Native` rows, dispatch arms deleted; (b) shims deleted, Retired rows kept as permanent pointers; (c) navigator + lifecycle closes; (d) endgame ratchet drop | ≈16 commands / ≈22 tokens; sites ≤ 9; `KNOWN_UNADVERTISED` → empty; goldens + generated doc regenerated. Four PRs, one release: muscle memory breaks once, review stays one-step-per-PR |

**Deprecation window:** one full release cycle after PR11, all retirements closing together. High-frequency verbs (`/resume`, `/compress`, `/cd`, the navigator family) never answer "unknown" — their Retired rows are permanent.

---

## 6. Findings ledger — every FATAL and SERIOUS, resolved

| # | finding (lens) | resolution |
|---|---|---|
| F1 | Read family absorbed into browse-refusing sections (headless) | **Avoided.** `/status` stays a verb with 8 topics + `models`; §3.3 rule: reads print on a pipe, they never open. |
| F2 | `/remember` behind a plain modal dies on pipes (headless) | **Avoided.** `/remember` is an exception verb with its inline free-text tail. |
| F3 | Endgame row-death disarms `receipt_for` and panics guard :915 (provenance, feasibility) | **Fixed by the one table** (§4.1): absorbed rows become `Native`, keeping their Journal declaration; ratchets count `Surface::Slash` only. |
| F4 | `run_chat`-locals relocation unscheduled (feasibility) | **Fixed:** per-slice precondition gate + LINK-mode fallback (§5); locals-heavy performers stay verbs and need no relocation. |
| F5 | Mid-task cluster absorbed into a mid-turn-barred surface (ergonomics) | **Avoided.** `/status`, `/compress`, `/resume`, `/remember`, `/spill`, `/dock`, `/cd`, `/tab` all stay verbs by criterion 2. |
| F6 | Token graveyard: dead verbs answer "unknown" (ergonomics) | **Fixed:** `Surface::Retired{replacement}` is permanent pointer data with a no-dangling guard. |
| F7 | Q2 stamped "answered" while answering one reason of three (q2) | **Fixed:** §3.5 answers all three, and keeps the link concession as a live fallback. |
| S1 | Nine (or five) new `SettingValue` op variants: wrong record shape, journal-scope conflation, untagged minefield (provenance, feasibility, q2) | **Fixed:** zero new op variants. Operations park as counted `Missing` and go to the event journal (§4.4). |
| S2 | Section actions escape the ratchet when their rows die (provenance) | **Fixed:** `SectionAction` rows are counted rows with a bidirectional join to the section's action table. |
| S3 | `SETTINGS_NATIVE_FIELDS` is a second receipt authority; `Disposition::Section` breaks the :915 join (q2) | **Fixed:** one table, one lookup, one bidirectional join (§4.1). Neither a second table nor an unjoined disposition. |
| S4 | Reclassifying `/compress`, `/remember`, `/loadout` `Missing`→`None_` by argument is test-invisible laundering (provenance, q2) | **Fixed:** only rows *verified* to write nothing may reclassify, and each carries its argument here (`/memory`, `/loadout`). `/compress` and `/remember` stay counted. |
| S5 | Phantom `Missing` decrements booked against `None_` rows; a mutator registered read-only cannot be fixed without raising the ratchet (q2) | **Fixed:** one declared, itemized truthing raise in PR1 (§4.2). Honesty over a pretty number. |
| S6 | `/setup` fresh-box auto-open prompts on eval/wyvern paths (headless) | **Fixed:** dual-gated on TTY ∧ interactive origin; degrades to one plain line. |
| S7 | Discovery on a pipe can hang with no timeout or refusal (headless, all three) | **Fixed:** bounded discovery with an honest refusal naming what was unreachable; a named acceptance test. |
| S8 | `/nav` fold destroys palette discovery for 13 verbs (ergonomics) | **Fixed:** one `help_lines()` row per subverb; the corpus rule permits it and #2003 then covers them. |
| S9 | Free-text family has no headless story; `help_request` carve-out and quoted-template parsing lost (headless) | **Fixed:** §3.4 — carve-out and quoting explicitly ported to the deep links. |
| S10 | Post-command reload divergence (chat.rs:6189) changes behavior silently on re-routed commands (feasibility) | **Fixed:** PR9 carries an explicit regression test; every moved command decides it in its PR body. |
| S11 | `/dock` kill-switch demoted into a section (headless, ergonomics) | **Avoided.** `/dock` is an exception verb; peers are readable in Permissions *and* via the verb. |
| S12 | Dock kill and decision reopens stay unwitnessed for the whole train (provenance) | **Mitigated:** PR-E is sequenced before window close, not "someday"; until then the rows stay parked and counted. |
| S13 | Web `/settings` claimed free; `shell.rs` renders one Choice and `apply_and_record` is `pub(crate)` (feasibility, provenance) | **Refuted and parked** (§3.3). No web claim is made in this train. |
| S14 | Per-slice site-count reductions the mechanism cannot deliver (feasibility) | **Fixed:** §5's site-honesty paragraph names which slices free a site; others only reconfirm the pin. |
| S15 | `/spill` and `/tab` folds cost high-frequency reflexes; macOS Alt chords are broken (ergonomics) | **Avoided:** both stay verbs (OPERATOR-CALLs Q3, Q4). |
| S16 | `/status`/glance family spent to reach a rounder number (ergonomics) | **Avoided:** `/status` is an exception verb; `/models` folded as a `/status` topic *and* a section view. |
| S17 | PR13 big-bang close conflicts with one-step-per-PR (feasibility) | **Fixed:** PR14a–d, four slices in one release. |
| S18 | Conversation rename/delete receipt destination left as a coin flip (provenance) | **Fixed:** parked `Missing` rows with the event journal as the named destination; no invented store log. |
| S19 | `ControlKind::Text` unreachable in the rich widget (q2) | **Fixed:** §3.4 routes rich free text to the panel field editor. |
| S20 | `/recall` full-text conversation search inside a section contradicts the config-only cap (q2) | **Avoided:** `/recall` folds into `/resume`, an exception verb outside the shell. |
| m1 | Q1 citation inverted in all three designs | **Fixed:** §3.6 quotes the actual holding and supersedes it by argument. |
| m2 | `relocate` rejects `Shift` | **Fixed:** §3.3 — `Shift` to open, `Refuse`/`SuspendHolder` to resize. |
| m3 | `/start` semantics differ from `/new` (skips extraction, leaves the conversation open) | **Fixed:** PR2 reads the code before registering; aliases only where behavior is proven identical. |
| m4 | `via` and apply granularity undefined for shell edits | **Fixed:** §4.3 — `via = "/settings <section>.<field>"`, applied on commit. |

---

## 7. Open questions — operator only

| # | question | recommendation |
|---|---|---|
| **Q1** | `/nav`: fold the 13 navigator verbs + `/search` + `/retrieval` under one prefix, keep all 13 as verbs, or delete the family and let the model's tools serve the journey? | **Fold**, with one help line per subverb so discovery is not spent to buy the count. Rejecting it returns 13 rows → endgame ≈29 commands. Deleting is defensible only if you judge the deterministic zero-inference query journey dead. |
| **Q2** | `/dgx` — keep the verb, or fold fleet ops into the Backends section? | **Keep.** It removes an entire unbuilt work item (modal progress rendering) and it is incident-speed on your daily-driver box. |
| **Q3** | `/tab` — keep the verb, or retire it to chords + palette? | **Keep** until a macOS-safe chord exists. `/detail` exists precisely because Option is a compose key; retiring `/tab` to a chord repeats the mistake that verb was created to fix. |
| **Q4** | `/spill` — keep the verb, or fold its views into `/transcript`? | **Keep.** Folding is new pager machinery for the same view, and it retrains a reflex used exactly while watching a run go wrong. |
| **Q5** | `/rename` → `/settings title`: is a conversation title a setting? | **ANSWERED (PR5): conversation metadata — `/rename` survives as a verb**, the alternative this row already named. Three reasons, in order of weight. (1) *Its state is a per-conversation DB row*, reached through `conversation_store` + `active_conversation_id` — two of the `run_chat` locals #1999 names, so absorbing it needs the full store relocation, not a slice-sized one. (2) *A title is not a session preference*: `~/.newt/receipts.jsonl` records what the operator set for the session, and every retitle of every conversation landing there conflates document metadata with settings. (3) §4.4 already routes conversation operations to the **event journal**, and a rename is one of them — absorbing it would put one conversation op in the settings journal while `list`/`restore`/`delete` wait for the other. The free-text carve-out was ported regardless, because `Text` fields need it. |
| **Q6** | Deprecation window length. | **One full release cycle** after PR11, all retirements closing together in PR14a–d — muscle memory breaks once, not thirteen times. |
| **Q7** | The event journal (PR-E): land it in-train, or accept ~10 parked `Missing` rows at the endgame? | **Land it before window close.** Without it the security kill-switch and #1749 decision reversals stay unwitnessed. Accepting the parked set is honest but leaves the highest-value provenance events recordless. |
| **Q8** | The PR1 truthing raise: approve a one-time, itemized increase to the receiptless-mutator count? | **Approve.** The alternative is a mutator that stays invisible because registering it would embarrass a number. |
| **Q9** | `/cd` — permanent exception verb, or shim-then-delete to `/settings cwd`? | **Permanent.** Shell reflex, one typed line, works on a pipe. |
| **Q10** | Is `/settings` allowed to show conversation *metadata* (titles, decision summaries) in Permissions/Audit, or does the config-only cap bar it? | **Metadata yes, content no** — that is the line I drew. A strict reading returns `/undo-lock` and the conversation ops to verbs (+2). |

---

## 8. Endgame numbers (declared, not aspirational)

- **Slash-dispatchable:** ≈16 commands / ≈22 tokens (13–18 / 18–26 depending on Q1–Q5), from 65 / 79.
- **Interception sites:** ≤ 9 (`help_request`, `dispatch_slash`, `/nav` parser, plus the surviving exception blocks) from 21 — exact number declared in PR14d.
- **Receiptless mutators:** rises once in PR1 (truthing), then falls to ≈10 parked run-state rows — or ≈0–2 if PR-E lands (Q7).
- **`KNOWN_UNADVERTISED`:** → empty.
- **Registry rows:** grow (fields + permanent pointers). That is the point: the *surface* shrinks while the *register* becomes complete for the first time.

---

## 9. Operator overlay — 2026-08-31/09-01 session

The synthesis above was produced before these operator statements. They are
authority, not input to be weighed, and they settle four of §7's questions.

**Named exception verbs.** Verbatim: *"I want a `/resume` I want a `/rename`
and I want a `/clear` or `/new` or `/end`."*

- `/resume` — already exception #6. Unchanged.
- `/rename` — **§7 Q5 is answered: `/rename` survives as a verb.** The table's
  Native-field row is overruled. Both may exist (the verb performing the
  rename, `/settings title` deep-linking to the same `apply_and_record`), but
  the verb is not a shim scheduled for deletion — the operator asked for it by
  name. PR5's OPERATOR-CALL checkpoint is resolved in advance.
- The conversation-boundary verb — the lifecycle family (§2, PR2) ships, and
  **the operator picks the name**: `/clear`, `/new`, or `/end`. PR2 reads
  `chat.rs:4487-4504` first and registers whichever is chosen as the primary
  token, the other two as aliases only where behaviour is proven identical.
  Naming note for that PR: `/clear` and `/new` read as "start fresh", `/end`
  reads as "stop here" — if they are two different acts, register two rows
  rather than three aliases of one.

**Key semantics are part of this surface contract.** As the verb count falls,
keys carry more of the interface. Three issues filed tonight are inputs to the
train, not separate concerns:

- **#2005 — Esc during a running turn must interrupt and return control.**
  Operator: *"I expect the ESC to eventually interrupt the agent's turn and
  return the control to me."* Today no Esc path interrupts a running turn.
  Sequence this **before** PR8's rich shell: the shell adds another Esc
  context, and adding contexts to an already-wrong key compounds the defect.
- **#2006 — vi mode must persist across editor remounts.** The reported
  "Esc toggles" is that: idle Esc is verified idempotent (INSERT→NORMAL, then
  no-op, live-tested on `bff1967c`), but `MountedEditor::new` starts INSERT on
  every mount, so NORMAL is silently lost at each prompt boundary. Vim's model:
  a submit is not a buffer close.
- **#2007 — `:help` is the vim help SYSTEM, not `/help`.** Docked navigable
  buffer, `Ctrl-]` tag-jump into the quick reference, `Ctrl-O`/`Ctrl-T` back,
  vi motions, `q`/Esc to close; content as data; reuse the existing jumplist.
  This is the editor teaching itself, and it is **exempt from the `/help`
  fold** — §2's `/docs`→`/help docs` row concerns the chat corpus only. The
  two must not be merged: `/help` lists commands, `:help` teaches the editor.

**What this changes in the numbers.** `/rename` stays a verb (+1 vs the
recommendation) and the lifecycle verb was already counted, so the endgame is
**≈17 commands** on the recommended answers to Q1–Q4, still from 65.

**Still operator-only** (§7, unanswered): Q1 `/nav` fold, Q2 `/dgx`, Q3 `/tab`,
Q4 `/spill`, Q6 deprecation-window length, Q7 event-journal sequencing, Q8 the
truthing raise, Q9 `/cd`, Q10 the metadata-vs-content line. Recommendations
stand as written; PR0 merges with them unanswered and PR1 does not start until
Q8 is answered.

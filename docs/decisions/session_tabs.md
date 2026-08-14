# Decision: bottom-anchored session tabs on the RichTUI surface (#1669)

**Status:** Proposed — ADR draft for operator acceptance. An application of the
2026-08-11 rich-surface amendment to `docs/decisions/plain_scroller_tui.md`
(inline viewport, TTY-gated, severable) and of the interaction grammar in
`docs/decisions/harness_config_panel.md` (which names #1669 as a consumer).
**Date:** 2026-08-14 (revised 2026-08-14 — state model correction, see below)
**Revision note:** the first draft specified tab switching as the in-session
`/resume` path verbatim, i.e. a **sparse pin overlay over live process
state**. That is unsound for tab activation: an unpinned tab silently
inherits the previously active tab's backend/cognition, so a tab's behavior
depends on which tab you visited before it. This revision separates the
**resume** verb (sparse overlay — correct, and #1684's semantics) from the
**activate** verb (reset to a session baseline, then overlay that tab's
explicit pin), adds a **deactivation flush** so posture changes made without
sending a prompt are not lost, and pins the whole contract in a normative
state model + state-transition table (P1). No implementation may proceed on
the pre-revision semantics.
**Related:** #1669 (this feature), **#1668 / PR #1684 (per-conversation
`PosturePin` — HARD dependency, in flight on `issue-1668/session-posture`;
its pin is behavioral/operator preference, NEVER authority, and this design
depends on that boundary)**, #1671 (session rename — merged;
tab labels ride it for free), #1030 (the in-session `/resume` path this design
generalizes), #531 (ex-command row), #416/#419 (`InputSurface` seam),
`docs/decisions/lean_rich_tui_morphologies.md`,
`docs/decisions/live_spill_viewport.md`, herdr `src/workspace.rs` /
`src/workspace/tab.rs` (shape reference only).

Operator-fixed constraints (non-negotiable inputs to this design):
bottom-anchored tab bar; one conversation per tab, each carrying its own
backend+psyche posture via #1668's `PosturePin` applied through the existing
setters on resume; dials stay process-global (one live copy — one tab is ever
active in the single-threaded REPL — but their *values* are tab-projected:
reset to the session baseline, then overlaid from the incoming tab's pin, on
every activation; see the state model); auto-names replaced by `/rename`
(#1671); vim navigation `gt`/`gT`/`<n>gt` +
`:tabnew`/`:tabclose`/`:tabn`/`:tabp`; **no**
tmux-prefix or herdr-leader chords (newt runs inside both); right-click tab
context menu via the existing mouse layer; rich-tui + TTY only — lean keeps
single-session behavior; no alternate screen, ever.

---

## TL;DR

A newt tab is a **held conversation**, and the REPL stays single-threaded — so
the whole design collapses to **"N claims, one live restore."** Switching a tab
reuses the proven in-session `/resume` machinery
(`restore_conversation_into_session`, `newt-tui/src/lib.rs:5790`) with exactly
four deltas: no claim release on the outgoing side, a **deactivation flush**
that persists the outgoing tab's pin + sidecar + input stash *before* anything
on the incoming side mutates, a **baseline-reset-then-overlay** posture
restore on the incoming side — `activate(B) ≡ baseline ⊕ B.pin`, never a
sparse overlay over live state, so B's behavior cannot depend on which tab was
visited before it (bounded, with defined failure semantics) — and a per-tab
`input_stash` so a half-typed prompt survives switching away and back.
**Activating a held tab and resuming a conversation are distinct verbs:**
`/resume` keeps #1668/#1684's sparse fail-open overlay (right where the
operator deliberately carries their live posture into a conversation they are
choosing to continue); tab activation restores from the session baseline, so
an unpinned field means *baseline*, not *whatever the previous tab left in the
process globals*. The bar
is one new bottom row of the existing `Viewport::Inline` region, rendered only
when ≥2 tabs exist — a single-tab session is byte-identical to today. Keys:
`gt`/`gT`/`<n>gt` via a count-carrying `Pending::G(Option<usize>)` (the
existing pending machinery **cannot** read the count — verified, see P3),
`:tab*` in the existing vi ex-line, and a `/tab` slash family as the
mode-agnostic text path; all four front doors (vi keys, ex, slash, mouse) feed
**one** `handle_tab_action` in `run_chat`. Mouse lands last: prompt-scoped,
button-only `MouseCaptureGuard`, double-gated (`[tui] mouse_viewport` opt-in
AND ≥2 tabs), one shared geometry function for render + hit-test, and a small
in-viewport context menu whose rename action **pre-fills `/rename`** into the
normal input line instead of growing a new text-entry widget. Four stacked
PRs; #1668 gates the first.

## Context

- `run_chat` is one ~5,200-line function whose entire session state is `let
  mut` locals (`newt-tui/src/chat.rs:808-1749`); there is no session struct to
  multiplex today. The conversation **row** already persists persona,
  scratchpad, plan snapshots, prompt receipts
  (`newt-core/src/conversation.rs:172-203`), and — once #1668 lands — the
  `PosturePin`. `restore_conversation_into_session` (`lib.rs:5790-5847`) is
  the single trusted hydration path, and the in-session `/resume` (#1030,
  `chat.rs:3492-3656`) already performs a full live conversation swap,
  including claim-guarding the target and rehydrating `pending_clarification`.
  A tab switch is that operation minus the claim release.
- **#1684 (`issue-1668/session-posture`, in flight) — verified against that
  branch:** `PosturePin` is a SPARSE operator-override record (`None` = the
  operator never pinned that dial; an untouched session round-trips an empty
  pin), `restore_pinned_posture` is a **fail-open overlay over the live
  process globals** (empty pin ⇒ complete no-op), and capture runs at exactly
  ONE production seam — the saved-turn chokepoint. All three properties are
  correct for `/resume` at process start, where live state *is* the launch
  baseline. All three are **wrong as tab-switch semantics**: (1) an overlay
  over live state makes an unpinned tab silently inherit the previous tab's
  backend/cognition — the activated tab's behavior depends on which tab was
  visited before it, which is not session isolation; (2) turn-seam-only
  capture means "change a dial, switch immediately, never send a prompt"
  persists nothing — the outgoing tab's pin is stale; (3) the two *compose*:
  the leaked global from (1) is captured into the unpinned tab's row at its
  next saved turn, making the leak durable. The state model in P1 exists to
  close all three; #1684 itself needs no shape change (see the coordination
  asks in the Slice plan).
- Model turns run **synchronously** inside the loop
  (`with_live_spill_watch(|| block_in_place(block_on(...)))`,
  `chat.rs:5216-5224`); `read_line` only runs between turns, and the rich
  inline viewport is torn down turn-scoped (`rich_input.rs:1031-1042`). There
  is no in-flight turn to switch away from — by construction, not policy.
- The rich surface is a `Viewport::Inline` region pinned to the terminal
  bottom, explicitly no alternate screen (`rich_input.rs:1-10, 425-432`);
  `RichStatus` feeds the header on **row 0**, the *top* of the viewport
  (`rich_input.rs:663-676`) — so "bottom-anchored bar" means a new *last* row
  of the viewport, not a change to the header.
- Doctrine: the 2026-08-11 amendment scopes plain-scroller constraints to the
  LEAN surface + headless path; RichTUI may host status regions if they stay
  inline, runtime-TTY-gated, and compile-time severable
  (`plain_scroller_tui.md:44-64, 245-265`). The revisit rule
  (`plain_scroller_tui.md:66-72`) requires a decision doc for semantics lean
  cannot express — this document is that doc: **tabs are RichTUI presentation
  over conversation switching; lean expresses the same capability as scrolled
  lines via `/resume`, `/new`, and `/rename`.**
- herdr supplies the *shape* to emulate — `Workspace { tabs: Vec<Tab>, active:
  usize }` with bounds-checked switch, refuse-last close, identity-preserving
  move (`herdr/src/workspace.rs:205-206, 504-513, 628-667`) — not the PTY
  internals, and (deliberately, see P1) not its monotonic tab numbers.
- Survey correction, verified in source: the claim that vi's `Pending::G` can
  read `self.count` is **false** — `take_count()` at `vi.rs:305` zeroes the
  count before `'g'` arms the pending at `vi.rs:381`. The key design in P3
  below exists because of this.
- Compat note: the command palette (#1674) merged while this draft was being
  judged; its rows compose inside the body area and join the same height
  budget — nothing here assumes its absence structurally.

## Decision

### P1 — Session multiplexing: a tab is a held conversation; switch REUSES the in-session `/resume` machinery under activation semantics

**Model — new file `newt-tui/src/tabs.rs`** (keep-files-small; pure, TTY-free,
unit-testable):

```rust
pub struct TabSet { tabs: Vec<TabState>, active: usize }

pub struct TabState {
    conversation_id: String,
    sidecar: TabSidecar,
    input_stash: String,          // unsubmitted prompt text, restored on reactivation
}

pub struct TabSidecar {
    turns_this_conversation: u32, // chat.rs:1574 — feeds close-time extraction
    last_resume_listing: Vec<String>,   // chat.rs:1585
    active_roadmap_id: Option<String>,  // chat.rs:1588
    interrupted_objective: Option<TurnPromptContext>, // chat.rs:1597 — set at
    // 5610, consumed at 1874, NOT restored by lib.rs:5790 (verified); without
    // this, tab A's objective upgrades tab B's bare "continue"
}
```

Ops transplant herdr's semantics: `switch(idx)` is a bounds-checked
assignment, `close(idx)` refuses the last tab and fixes up the active index,
`move_tab(from, to)` preserves the active tab by conversation-id identity,
plus `find_by_conversation(&str)` and `release_all(&ConversationStore)`.
**Deliberate divergence from herdr:** tab numbers are **positional** (idx+1),
renumbering on close like vim — because `<n>gt` is positional-absolute and the
bar must never disagree with the count prefix. Labels are never stored:
computed fresh per redraw from `store.title(id)` else `#<short-id>` (the #1671
rule, `chat.rs:1838-1848`, factored into a shared `tab_label` helper), so
`/rename` honesty is free and titles never go stale.

**Per-tab = conversation_id + everything the row already persists** (turns,
persona, scratchpad, plan, receipts, #1668's `PosturePin`) **+ the sidecar +
input_stash.** `pending_clarification` does *not* ride the sidecar — the
existing resume path rehydrates it (`chat.rs:3598-3613`). `pending_retry` is
provably `None` at read time (consumed at loop head, `chat.rs:1768`).

**Global (shared across tabs, on purpose):** `cfg`, `session_cwd`, the
`MemoryManager` *instance*, `system` String, spill/compaction stores +
session nonce, `semantic_index`, `where_is_index`, `experience_store`
(`chat.rs:1481-1538`), and all display/behavior overrides
(`markdown_override` etc., `chat.rs:989-1024` — operator dials, not
conversation state). **Authority/security state is global AND
posture-inert:** `active_posture` (the *permission* posture,
process-lifetime by declaration, `chat.rs:984-986`), approvals, and
credentials never appear in a `PosturePin`, are never reset or overlaid by
tab activation, and never migrate because UI/session state changed —
`inf_key` is resolved from `Config` by backend *name*; the pin/row persists
names only, never endpoints or keys.

**Tab-projected posture cells (one live copy, value owned by the active
tab):** the cognition/tenacity override globals, the operator baseline pair
`base_provider`/`base_model`, the `NEWT_PROVIDER`/`NEWT_DGX_MODEL` env slots,
and the backend quintet (`choice`/`inf_url`/`inf_model`/`inf_kind`/`inf_key`)
— these stay process-global/`run_chat`-local *cells* (correct: one tab is
ever active in the single-threaded REPL), but their **values are recomputed
on every activation as `baseline ⊕ incoming row's PosturePin`** — never
overlaid over whatever the previous tab left behind. The row plus the
baseline is the single source of truth; tabs carry no copy, so divergence is
unrepresentable — and because activation *resets before it overlays*, an
unpinned field cannot inherit a prior tab's override. See the state model
below for the full contract.

**Switch mechanics — `perform_tab_switch` in `chat.rs`,** extracted from the
in-session `/resume` block (`chat.rs:3547-3649`), structured as
**deactivate(outgoing) → activate(incoming)** with a staged, fail-safe
ordering:

1. **Stage 0 — read/validate (fallible, mutates NOTHING).** Load the
   incoming conversation row (turns, persona, scratchpad, plan, pin) fully
   into memory and validate the tab index. Any store error here aborts the
   switch with the outgoing tab still fully active and *untouched* — a failed
   tab restore must not partially mutate global state.
2. **Stage 1 — deactivate the outgoing tab.** Flush its `PosturePin` to its
   row (same capture as the saved-turn seam — see "Dirty-state persistence"
   in the state model), stash the sidecar fields, and stash the unsubmitted
   input buffer into `input_stash`. **Skip the claim release**
   (`chat.rs:3571-3574`) — every open tab holds its claim from open to close.
   (`/resume` inside a tab keeps releasing: it *replaces* the active tab's
   conversation.)
3. **Stage 2 — hydrate the incoming conversation** via
   `resume_session_conversation` → `restore_conversation_into_session`
   (memory `restore_turns`, scratchpad, step ledger, persona, prompt context,
   `compress_state` reset, id adoption, system rebuild — the one trusted
   path), operating on the row already read in Stage 0.
4. **Stage 3 — posture reset-then-overlay.** First **reset the tab-projected
   posture cells to the session baseline** (cognition/tenacity override
   globals, `NEWT_PROVIDER`/`NEWT_DGX_MODEL`, `base_provider`/`base_model`),
   then **apply the incoming row's `PosturePin` over that baseline** via
   #1668's apply-on-resume seam — the identical setters path
   (`apply_persona_backend`, the `/model` path,
   `set_persona_tenacity`/`set_persona_cognition`) — then refresh the backend
   quintet, so the next `set_runtime_context` shows the incoming
   `model@endpoint`. **Header honesty is an invariant**, and so is
   history-independence: after Stage 3 the live posture is a pure function of
   (baseline, incoming pin) — no residue of the outgoing tab survives.
5. **Stage 4 — restore the incoming side:** sidecar unstash; `input_stash`
   seeded through the existing type-ahead prefill (`rich_input.rs:1057-1060`)
   so the half-typed prompt reappears — the herdr/tmux "come back
   mid-thought" continuity, without carrying tab A's text into tab B.

Then emit a scrollback boundary banner (`── tab 2/3 · <title> ──` +
`auto_resume_banner`) via the print path — scrollback is the canonical log and
interleaved conversations need greppable seams.

**PosturePin apply contract (grafted requirement on #1668):**
`apply_persona_backend` runs adoption probes and `apply_model_choice` does
served-validation + Ollama warmup — network-adjacent work. The #1668 seam this
design consumes MUST be exposed as a **named callable** (e.g.
`apply_posture_pin_on_resume(...) -> Result<AppliedPin, PinApplyError>`) that
is **bounded and cache-tolerant**. Failure semantics, defined here: the switch
itself still lands (the restore is authoritative); because the Stage-3 reset
has already run, on apply failure the route quintet **lands at the session
baseline** (a defined, tab-independent state — NOT the previous tab's route),
a warning prints to scrollback, and the header/bar shows a `!pin` badge; the
apply retries at the next turn head, and if it still fails **the turn is
refused with an error** rather than run under a posture that contradicts the
pinned row. No half-rewritten quintet, no silent wrong-posture execution, and
no fallback that quietly re-inherits the outgoing tab's backend.

#### The state model: baseline, overlay, activation, deactivation

This subsection is normative. It exists because sparse restore semantics,
reused verbatim from the resume path, produce cross-tab state bleed: with tab
A pinned `backend=nemotron, cognition=high` and tab B unpinned, a sparse
overlay on switch A→B applies nothing — B silently runs on A's backend and
cognition, and B's behavior depends on which tab was visited before it. That
is not acceptable session isolation, so the model below is the contract every
implementation PR is tested against.

**State classes** (every posture-relevant datum is in exactly one):

| Class | Members | Lifetime | Written by |
|---|---|---|---|
| **Baseline** | `SessionBaseline { provider, model, cognition_override, tenacity_override }` — the operator's launch-time posture (CLI flags + env + config, snapshotted in `run_chat` after config resolution, before any restore) | process, **immutable** | session start only |
| **Tab-owned, persisted** | conversation row: turns, persona, scratchpad, plan, receipts, **`PosturePin`** (sparse operator overrides, #1668 shape — unchanged) | durable | saved-turn seam + deactivation flush |
| **Tab-owned, in-memory** | `TabSidecar` (4 locals) + `input_stash` | process | deactivation stash / activation unstash |
| **Tab-projected cells** | cognition/tenacity override globals, `NEWT_PROVIDER`/`NEWT_DGX_MODEL`, `base_provider`/`base_model`, backend quintet | process cells; **value = `baseline ⊕ active tab's pin ⊕ in-tab mutations`** | activation (reset+overlay) and in-tab dial commands |
| **Process-global, shared** | `cfg`, cwd, memory/spill/index/experience instances, display dials | process | operator dials only — a tab switch never touches them |
| **Authority/security** | permission `active_posture`, approvals, credentials/keys | process | NEVER by tab/posture machinery — posture-inert by construction |

**Resume ≠ activate — two verbs, two semantics:**

- **`resume` (conversation replacement):** the operator deliberately pulls a
  conversation into their *current* working context — startup resume,
  auto-resume, and `/resume <target>` replacing the active tab's
  conversation. Semantics: **#1668/#1684's sparse fail-open overlay over live
  state**, unchanged. This is correct at process start (live state IS the
  baseline, so overlay ≡ reset-then-overlay there) and acceptable in-tab
  (the operator is choosing to carry their live posture forward; unpinned
  fields keep *their own current* values — the values of the tab they are
  sitting in, not a third tab's).
- **`activate` (held-tab switch):** returning to a held context that must be
  self-consistent — `gt`/`gT`/`<n>gt`, `:tabn`/`:tabp`, `/tab <n>`, bar
  click, menu switch, and `/resume` targeting a conversation already open in
  another tab (`:tab drop`). Semantics: **reset the tab-projected cells to
  the baseline, then overlay the incoming row's pin** (Stage 3 above).
  Invariant: `activate(B)` yields identical live state regardless of which
  tabs were visited before, in any order.

**Naming:** #1684's rework resets the posture globals to an *invocation
baseline* on every conversation switch. That is the same object and the same
rule as this ADR's `SessionBaseline` — one concept at two layers, not two.
Implementers should see a single name; if #1684 lands the term first, this
ADR adopts it rather than introducing a synonym.

**Deactivation (the outgoing half of every switch/create/close/exit):**
before any incoming mutation, the outgoing tab (a) **flushes its
`PosturePin`** — the same `PosturePin::capture(base_provider, base_model)` +
dial-override snapshot the saved-turn seam writes, unconditionally (the write
is metadata-only, no MRU tick, and idempotent when nothing changed) — and (b)
stashes sidecar + input buffer. This closes the stale-pin hole: *change a
dial → switch immediately → never send a prompt* still persists the change,
so returning to the tab restores the changed value. Flush seams, complete
list: **saved-turn** (#1684, keeps per-turn provenance), **deactivation**
(switch-away, `/tab new`, close, `/resume`-replacement), and **session exit**
(every exit path, beside `release_all`). No flush-on-every-dial-mutation
write-through is required (rejected below).

**How #1684's action-scoped capture changes this rule's weight.** The
paragraph above assumes capture runs at the saved-turn chokepoint, which is
what makes the deactivation flush *load-bearing* for posture: without it the
dial change is simply lost. #1684's rework moves capture off that chokepoint
to explicit operator **actions** (successful `/backends`, `/backend`,
`/model`, `/psyche cognition|tenacity`, `/psyche obsessive`, psyche-panel
apply — dirty axes only, merged per-axis into the stored pin). Under that
model the dial change is already persisted *at the dial change*, so for
**posture** the deactivation flush degrades from load-bearing to
belt-and-braces — it remains correct and idempotent, but it is no longer the
thing that closes the hole. For the **sidecar and input stash** the
deactivation step stays load-bearing either way: those have no action seam
and exist only in memory. State this explicitly, because a reader who assumes
the saved-turn seam will otherwise mis-derive the invariant and conclude the
flush is the only defense.

**Dirty-state window (accepted, bounded):** posture mutations made after the
last flush seam are lost on **crash** — the same blast radius as a crashed
single session under #1684, and strictly smaller than before this rule (the
window used to extend across switches; now it ends at the next
deactivation). Sidecar + input stashes are in-memory and lost on crash by
design — the durable parts of a tab live on the conversation row.

**Crash/restart semantics:** a dead newt leaves N claimed rows;
`store.claim`'s stale-reclaim recovers them on next open (Risk 4 tracks the
multi-claim verification). Restart is NOT a tab-session restore: the process
starts with live state = baseline by construction, `session_start` resumes
one conversation via the **resume** verb (sparse overlay — which over a
fresh process is exactly `baseline ⊕ pin`, i.e. coincides with activation
semantics; the two verbs only diverge mid-session), and the other formerly
held conversations are ordinary closed-but-resumable rows reachable via
`/resume` or `/tab new` + `/resume`. No tab-set persistence in v1 — the tab
bar is session furniture, not durable state.

**Authority/security separation (restated as an invariant):** activation and
deactivation read and write *only* the tab-projected cells and tab-owned
state above. The permission posture, approval state, and credential material
are outside both the pin (names only, resolved against `Config` at apply
time) and the activation/deactivation code paths — security authority never
migrates because UI/session state changed, and no tab switch can widen or
narrow what the agent is allowed to do.

**State-transition table** (normative; "flush" = pin+sidecar+stash per the
deactivation rule; "reset⊕overlay" = Stage 3):

| Transition | Trigger(s) | Outgoing tab | Target/incoming | Posture rule | Claims | Failure containment |
|---|---|---|---|---|---|---|
| **create** | `/tab new`, `:tabnew`, menu | deactivate: flush + stash | fresh conversation id | reset to **baseline** (empty pin ⇒ overlay is a no-op) | outgoing keeps claim; fresh id claimed | claim-refused ⇒ abort, outgoing still active |
| **resume** (replace) | `/resume <target>` in a tab, target not open elsewhere; startup/auto-resume | flush (in-tab case), then release outgoing claim | target hydrated via the one trusted path | **sparse overlay over live state** (#1668/#1684, unchanged) | outgoing released, target claimed | Stage-0 read fails ⇒ abort untouched; claim held elsewhere in-session ⇒ becomes *activate* |
| **activate** | `gt`/`gT`/`<n>gt`, `:tabn`/`:tabp`, `/tab <n>`, click, menu, `/resume`→open tab | deactivate: flush + stash | held tab hydrated from its row | **reset⊕overlay** — result independent of visit history | both claims held throughout | Stage-0 fails ⇒ abort untouched; pin apply fails ⇒ **baseline** + `!pin` + turn refusal until resolved |
| **deactivate** | outgoing half of create/activate/close/exit | flush pin (unconditional), stash sidecar + input | — | live values captured; nothing reset yet | claim retained (except close/exit) | flush write fails ⇒ warn, switch proceeds (row keeps last-good pin) |
| **close** | `/tab close`, `:tabclose`, menu | if active: activate a neighbor first (full deactivate+activate), then release | conversation stays open + `/resume`-able | neighbor activation is a normal **reset⊕overlay** | closed tab's claim released; row NOT ended | refuse-last-tab; release failure warns, tab still leaves the bar |
| **crash / restart** | process death, any cause | no flush ran — mutations since last seam lost; stashes lost | restart = fresh process; `session_start` resume verb | live = baseline; startup overlay ≡ activation over a fresh process | N stale claims, stale-reclaimed on next open | authority state never persisted in pins ⇒ nothing to leak or recover |

**Properties the implementation MUST test** (these become PR-A/PR-C tests;
listed here so the contract survives slicing):

1. `activate(B)` is independent of the previously active tab:
   `A→B` and `C→B` (and cold `B`) land byte-identical tab-projected state.
2. Round-trip: `A→B→A` restores A exactly (pin, dials, quintet, sidecar,
   input stash).
3. Dial change → immediate switch → return restores the changed value
   (the deactivation-flush property).
4. Unpinned fields do not inherit prior tab overrides: with A pinned and B
   unpinned, `A→B` shows baseline on every unpinned axis.
5. A failed tab restore does not partially mutate global state (Stage-0
   abort leaves the outgoing tab fully active and untouched).
6. Security authority never migrates via tab machinery: permission posture
   and credential state are bit-identical across any switch sequence, and
   no `PosturePin` round-trip can carry them.

**Tab lifecycle:** `/tab new` / `:tabnew` = the `/new` reset path
(`handle_new_conversation` via `ConversationResetContext`) *without*
`end_conversation` — the outgoing conversation stays open and claimed — then
claim the fresh id (reuse the claim-refused-startup fresh-start block,
`chat.rs:1681-1712`, factored). **Close = release-without-end** (adopted from
the losing proposals at the judges' direction, replacing this proposal's
original end-on-close): closing a tab releases its claim and leaves the
conversation **open and `/resume`-able** — vim closes tabs without deleting
buffers; `/end` remains the verb that ends a conversation. Closing the active
tab switches to a neighbor first (persisting everything), then releases.
Closing the last tab is refused ("`:q` is how you leave newt"). `/resume`
targeting a conversation already open in another tab **switches to that tab**
(vim `:tab drop` semantics — also sidesteps same-pid re-claim ambiguity).
Every exit path (clean, `Eof`, `EndAndQuit`, `Fatal`) runs a final
deactivation flush of the active tab, then releases ALL held claims via
`TabSet::release_all` (extend `chat.rs:5888-5893`).

**In-flight turns (the synchronous restriction, stated as a state-model
precondition):** structurally impossible to interrupt by switching — turns
are synchronous, `read_line` runs between turns, and the viewport (bar
included) does not exist during a turn. The honest UX rule, documented in
`/tab` help: one model turn at a time; Esc-interrupt first, then switch. Zero
blocking code is written. **This is what makes the reset⊕overlay activation
safe:** a switch can only occur at the between-turns quiescent point, so
there is never a live turn reading the tab-projected cells while activation
rewrites them. If a future change makes turns concurrent or backgroundable,
this ADR's activation contract **must be revisited first** — the cells would
then need per-turn capture rather than process-global projection.

**Background tasks:** anything already running out-of-band (spill watch,
compaction, index maintenance, dock/web listeners) is bound to
**process-global shared** state and to the conversation it was started under,
NOT to the tab-projected cells; a switch neither pauses nor reconfigures it.
The rule that keeps this coherent is the injects rule below — no background
work may *start a turn* in a non-active tab, so no background task can
observe a half-applied activation or run under another tab's posture.

**Memory:** one `MemoryManager` ever; a switch is a store row load +
`restore_turns` + system rebuild (ms-scale). RAM stays flat regardless of tab
count — 8 tabs is not 8 resident context windows. `compress_state.reset()`
inside restore is correct: a switch is a conversation boundary.

**Web injects:** only the active tab's inbox executes (unchanged,
`chat.rs:1789-1793`); background tabs get a bar badge instead. Running a
background inject would execute under the active tab's projected posture
rather than its own — precisely the cross-tab bleed the state model forbids —
so dormancy is a **correctness property**, not a limitation. (The alternative,
activating a tab to run its inject, would move the operator's session
underneath them; rejected.)

**Ephemeral sessions** (`--ephemeral`, no `ConversationStore`,
`chat.rs:859-867`): `/tab` and `:tabnew` refuse with "tabs need conversation
persistence — this session is ephemeral." Single tab, no bar, exact
single-session behavior.

### P2 — The bar: one new last row of the inline viewport, visible only at ≥2 tabs

The tab bar is a **new last row** of the inline viewport — below the
background-jobs row — honoring "bottom-anchored" literally (the viewport is
pinned to the terminal bottom). Nothing merges into the row-0 header. Layout:
`[Length(1) header][Min(1) input(+#531 ex row)][Length(0|1) background][Length(0|1) tabbar]`.
The ex-command row stays inside the input area — correct layering: the `:`
line is editor-local, the bar is session chrome.

**Visibility rule:** the bar renders **only when `tabs.len() >= 2`.** A
single-tab session is byte-identical to today — zero chrome, zero height cost,
zero test churn — and the bar's appearance on second-tab creation rides the
existing height-change clear+rebuild (`rich_input.rs:1102-1111`) with no ghost
rows in scrollback.

**Height budget** (`rich_input.rs:1094-1101`):
`want = (rows + ex_extra).clamp(1, MAX_INPUT_ROWS) + 1 + background_extra + tab_extra`,
plus a new **`want.min(term_rows)` total clamp** — a latent bug today (an
inline viewport taller than the screen corrupts scrollback), more likely with
the extra row; fix it in the same PR that adds the row.

**Data flow:** new default-no-op trait method
`InputSurface::set_tabs(&mut self, Vec<TabLabel>)` (mirroring
`set_background_jobs`, `chat.rs:805`) — lean's default no-op *is* the "lean
keeps single-session behavior" guarantee; `lean_input.rs` is untouched by
construction. `run_chat` builds labels fresh each prompt refresh beside the
#1671 session-label block (`chat.rs:1838-1849`):
`TabLabel { text, active, pending }` where `text = "{pos}:{title|#shortid}"`
and `pending` is a **throttled** non-blocking count of that tab's
injected-prompt inbox — polled once per `read_line` entry, not per 250ms tick.

**Rendering:** active cell bold + accent; pending = `●` suffix; narrow
terminals shrink labels to a ~6-col floor, elide with `…`, and window the bar
around the active index so the active cell never scrolls out. Critically, one
pure function
`layout_tab_cells(width, &[TabLabel]) -> Vec<(Range<u16>, usize)>` computes
cell geometry — used by **both** the renderer and P4's hit-testing, so they
cannot disagree, and unit-tested without a TTY.

### P3 — Key routing: `Pending::G(Option<usize>)`, `:tab*` ex commands, one handler with four front doors

**The count fix (survey correction, verified):** in `vi.rs::normal()`,
`let n = self.take_count();` at line 305 runs **before** the `'g' =>
self.pending = Pending::G` arm at line 381 — so by the time `Pending::G` is
armed, `self.count` is already zero and a naive `2gt` reads no count. Change
`Pending::G` to **`Pending::G(Option<usize>)`**, capturing the count when
`'g'` arms it (`None` = no count typed — this also preserves the `gt` vs `1gt`
distinction that min-1 `take_count()` designs cannot express). `gg` keeps
ignoring the count (today's behavior, `vi.rs:271-278`). In the
`Pending::G(cnt)` branch: `'g'` → gg unchanged; `'t'` →
`Step::Tab(cnt.map_or(TabAction::Next, TabAction::Goto))`; `'T'` →
`Step::Tab(TabAction::Prev(cnt.unwrap_or(1)))`; any other char cancels
silently (existing contract). **Exact vim semantics:** `gt` wraps forward,
`{n}gt` is ABSOLUTE tab n, `gT` back, `{n}gT` n-back. Zero conflicts,
verified: Esc in NORMAL is the pending-cancel no-op (`vi.rs:221-226`),
Tab/BackTab stay jumplist (`236-239`), Ctrl-O stays jumplist back, bare `'t'`
stays char-search (`383`, only reachable with `Pending::None`). No Tab-key
cycling is bound.

**Ex commands:** extend `ex_input`'s match (`vi.rs:140-178`): `"tabnew"` →
New; `"tabclose"|"tabc"` → Close(active); `"tabn"|"tabnext"` (+ optional
`" {n}"`) → Next/Goto; `"tabp"|"tabprev"|"tabN"` → Prev. Unknown commands
still cancel silently; refusal feedback rides the existing `vi.msg` →
`echo_note` channel (`rich_input.rs:1184-1186`); update `help_text`. **No new
prompt-level mode** — the vi ex-line already is the ex surface. **`:q`/`:wq`
keep session semantics** (exit / end-and-quit): silently repurposing
established muscle memory that ends the whole session is worse than the vim
delta; `:tabclose` is the close verb, and the delta is documented in help.

**Signal threading:** add `Step::Tab(TabAction)` (`rich_input.rs:121-132`) and
`ReadOutcome::Tab(TabAction)` (`chat.rs:41-59`, with
`#[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]` — the `EndAndQuit`
precedent; lean never constructs it). The surface stays TabSet-**ignorant**:
`event_loop` maps `Step::Tab` → `return Ok(ReadOutcome::Tab(..))` after
stashing the unsubmitted buffer for the outgoing tab's `input_stash`. **All**
validation ("no tab 9", "can't close the last tab", ephemeral refusal) lives
in `run_chat`'s outcome match, delegating to one **`handle_tab_action`** —
the same handler the `/tab` slash family and P4's menu call. One handler,
four front doors, no divergence possible; errors print via `print_newt`.

```rust
enum TabAction {
    New, Next, Prev(usize), Goto(usize),          // Goto is 1-based positional
    Close(Option<usize>),                          // None = active tab
    Move { from: usize, to: usize },               // identity-preserving
}
```

**Text-parity layer — decided and justified:** the `/tab` slash family in
`run_chat`'s slash branch (pattern of the `/resume` block, `chat.rs:3492`):
`/tab` (list — also THE piped-visible view of tab state), `/tab new`,
`/tab <n>`, `/tab next|prev`, `/tab close [n]`, `/tab move <a> <b>`,
`/tab rename [n] <title>` (extract the pre-titling logic from `/rename`,
`chat.rs:3356-3410`, into a `rename_conversation(store, id, title)` helper so
it can target non-active tabs). Why a text path when tabs are rich-only: (1)
**emacs/nano** are modeless passthrough with no ex-line
(`rich_input.rs:222-227, 339-343`) and the operator banned prefix/leader
chords — `/tab` IS their keyboard path (a `C-x t` prefix table is exactly the
shape newt must not squat on inside tmux/herdr: rejected); (2) the house rule
(`harness_config_panel.md:100-103` names #1669) and the bare-`/psyche`
precedent (`chat.rs:3907-3911`): on **lean** the same commands print an honest
refusal — "tabs are a rich-TUI feature; this session is single-conversation —
use `/resume`" — gated on the existing surface-choice predicate
(`chat.rs:1208-1225`). Never silence, never unknown-command. Lean keeps
single-session behavior per doctrine; the refusal keeps the namespace
discoverable and the doctrine line explicit. Web-injected `/tab ...` stays
inert model text via the existing `ModelInputOrigin::is_operator` gate.

### P4 — Mouse: prompt-scoped capture, shared geometry, `BarMenu` overlay, rename-by-prefill

Today the prompt has no capture: `event_loop` discards non-Key events
(`rich_input.rs:1143`) and `MouseCaptureGuard` is turn-scoped inside
`with_live_spill_watch` (`lib.rs:10011-10013`). **Add prompt-scoped capture**
in `RichSurface::read_turn` (`rich_input.rs:1031-1042`):
`let _mouse = MouseCaptureGuard::maybe(tier && tab_count >= 2)` beside
`EnableBracketedPaste`, RAII-released before the fn returns — the same
button-only `?1000h`+`?1006h` (no drag/motion flood), same panic-hook
discipline (`mouse.rs:31-48, 105-113`).

**Double gate:** the existing stack — `mouse_capable()` (TTYs + TERM +
unix-only + feature gate, `lib.rs:7910-7932`) AND the `[tui] mouse_viewport`
opt-in defaulting FALSE (`lib.rs:7898-7902`) — **PLUS `tabs.len() >= 2`.**
Prompt capture takes away native terminal text selection while idle (a real
trade; Shift+click remains the native escape hatch), so it is paid only by
operators who opted into the mouse tier AND currently have a bar. Single-tab
prompts stay byte-identical; with the tier off, the bar is purely
informational and every action keeps its key/text twin — no degraded
half-mouse state.

**Hit-testing:** handle `Event::Mouse` before the Key discard. Row: the bar is
the viewport's last row and the viewport is pinned to the terminal bottom, so
`bar_row = terminal_rows - 1` — **re-read `crossterm::terminal::size()` per
event** so a mid-prompt resize cannot misroute clicks. Columns: reuse
`layout_tab_cells` (renderer and hit-test share one source of truth).
`Down(Left)` on a cell → `Step::Tab(Goto(pos))` — left-click-to-switch comes
free from the same hit-test. `Down(Right)` on a cell → open the menu.
Elsewhere / wheel: ignored as today.

**Context menu:** NOT a nested `config_panel` run (we are inside a live inline
terminal; a second `Viewport::Inline` would fight it). An in-viewport modal
owned by `event_loop` state:
`enum BarMenu { Closed, Open { tab: usize, sel: usize } }`. While `Open`,
`draw()` renders a small bordered list over the input area — **switch / rename
/ close / move ← / move →** — navigated with the house grammar (↑↓ Enter Esc,
`harness_config_panel.md:100-103`, same as `config_panel.rs:920-937`), keys
routed to the menu before `editor.input` in the same precedence slot as the vi
confirm gate (`rich_input.rs:240-247`). The menu temporarily grows `want` via
the existing height-rebuild machinery + the P2 term-height clamp, and degrades
(refuses to open) on terminals too short to host it. Menu commits emit the
**same `TabAction` values** as keys/slash: switch → `Goto`, close →
`Close(Some(i))`, move → `Move{..}` (identity-preserving rule applied in
`handle_tab_action`).

**Rename-by-prefill (grafted from the minimal-diff proposal, replacing this
proposal's original in-menu text field):** the menu's rename action closes the
menu and **pre-fills the normal input line** with `/rename ` (active tab) or
`/tab rename <n> ` (other tabs) via the existing type-ahead prefill seam — so
renaming flows through the already-merged #1671 path with **zero new
text-entry surface**. If the input line held unsubmitted text, it is preserved
as the surface's unsaved-history entry (`rich_input.rs:996-1001`) so ↑
recovers it. This deletes the most fragile piece of the mouse PR — the only
new editing widget in the whole design — and shrinks it to hit-testing plus a
static five-item list. The next redraw shows the new label automatically
because labels are always recomputed from `store.title`.

### P5 — Slicing

Four stacked PRs, model first, mouse last; #1668 gates the first. Full detail
in the Slice plan below. Every front door added later lands on the one
`handle_tab_action` engine from PR-A, so no PR re-opens switching semantics.

## Slice plan (PR train)

**Gate: #1668 (`PosturePin`) must merge before PR-A.** #1668's
implementation is now in flight as **PR #1684** (`issue-1668/session-posture`
— verified: `PosturePin`/`PostureApplyPlan` in `newt-core/src/runtime.rs`,
`restore_pinned_posture` in `newt-tui/src/lib.rs`, capture at the saved-turn
seam). This design's posture story is "switch is resume-shaped, but
activation resets to baseline first." **Coordination asks on #1684's review
(cheap now, expensive later) — none change the pin's shape or its resume
semantics:**

1. Expose apply-on-resume as a named, bounded, cache-tolerant callable
   (`restore_pinned_posture` already is one — keep it so) rather than
   re-inlining it; PR-A will call it *after* a baseline reset for the
   activation verb, and #1684's sparse-overlay call sites for the resume
   verb stay exactly as they are.
2. #1684 already hooks the in-session `resume_session_conversation` seams
   (verified at three call sites) — PR-A converts the *switch-to-open-tab*
   case only (`:tab drop`) to activation semantics; single-session resume
   behavior is untouched.
3. A #1684-visible defect independent of tabs — **resolved at the root, not
   by this flush.** Under saved-turn capture, "change a dial → `/resume`
   another conversation → never send a prompt" leaves the outgoing row's pin
   stale. The original recommendation here was to flush the outgoing pin
   before `/resume` replaces it. #1684's action-scoped capture supersedes
   that: the dial change is captured at the action, so there is nothing left
   to flush and the staleness is dissolved rather than documented. PR-A
   therefore inherits a closed hole on the posture axis and must not
   re-derive a flush as its defense — see "How #1684's action-scoped capture
   changes this rule's weight" above.
4. **No new fields in `PosturePin`.** The baseline is a session-start
   snapshot living beside `TabSet` in newt-tui; nothing else process-global
   moves into the pin. Authority/security state stays outside the pin
   entirely, per #1684's "behavioral/operator preference, NOT authority"
   boundary — this ADR depends on that boundary and must not erode it.

**PR-A — tab model + `/tab` text engine** (depends: #1668; label `risk:high`)
- `newt-tui/src/tabs.rs`: `TabSet`/`TabState`/`TabSidecar` (incl.
  `interrupted_objective` and `input_stash`) + herdr-shaped ops, fully
  unit-tested TTY-free (bounds-checked switch, refuse-last close,
  active-index fixup, identity-preserving move, positional renumber-on-close).
- `chat.rs`: `SessionBaseline` snapshot at session start;
  `perform_tab_switch` (the staged deactivate→activate of the state model:
  Stage-0 fallible read, deactivation flush + stash, hydrate, baseline
  reset⊕pin overlay with the `!pin`-lands-at-baseline failure semantics,
  sidecar/stash restore + scrollback banner), `handle_tab_action`, the
  `/tab` slash family, `rename_conversation` helper extraction,
  claim-per-tab lifecycle (stop releasing on switch; **close =
  release-without-end**; every exit path flushes then releases ALL claims
  via `release_all`, extending `chat.rs:5888-5893`), `/resume` tab-drop,
  lean + ephemeral refusals.
- This decision doc lands here (the revisit-rule doc precedes the surface).
- Tests — the six normative properties from the state model
  (activation history-independence; `A→B→A` exact round-trip;
  dial-change→switch→return restores the change; unpinned fields show
  baseline, never a prior tab's override; Stage-0 failure mutates nothing;
  authority/permission state bit-identical across any switch sequence) —
  plus: two-tab state-isolation incl. `interrupted_objective`; **two tabs
  with different `PosturePin`s → switch → assert route quintet + dials (the
  #1668 contract test)**; pin-apply-failure → quintet at baseline + `!pin` +
  turn refused; outgoing-claim-held-across-switch;
  close-leaves-conversation-resumable; close-last-refused;
  exit-flushes-and-releases-all; resume-verb keeps sparse-overlay semantics
  (regression against silently converting `/resume` to reset⊕overlay);
  ephemeral refusal.
- Deliverable: fully working session multiplexing via text, zero pixels
  changed — independently demoable.

**PR-B — bar render** (stacks on A; `risk:low` candidate)
- `InputSurface::set_tabs` default no-op; `RichStatus.tabs`; the bar row +
  ≥2-tab visibility rule + height budget + the **`want.min(term_rows)`**
  clamp; `layout_tab_cells`; label freshness + throttled pending-inject
  badge; loop-head `set_tabs` call beside `set_runtime_context`.
- Tests: pure layout/truncation/active-cell-windowing; height math; the
  single-tab zero-row invariant (byte-identical frame). Green alone: renders
  whatever PR-A reports; lean untouched.

**PR-C — vi keys** (stacks on A; B sequenced before it for demo honesty;
`risk:low` candidate)
- `vi.rs`: `Pending::G(Option<usize>)` **with a regression test pinning that
  `2gt` reaches `Goto(2)`** (the falsified survey assumption becomes a pinned
  test); `gt`/`gT` arms; `:tabnew`/`:tabclose`/`:tabc`/`:tabn`/`:tabp` ex
  extensions + help text.
- `rich_input.rs`: `Step::Tab` + `ReadOutcome::Tab` threading + the
  per-tab `input_stash` capture/restore through the type-ahead seam, with a
  regression test guarding turn-completes-while-switching interleave.
- Tests are pure vi state-machine tests, no terminal: `2gt` ⇒ `Goto(2)`,
  `gt` ⇒ `Next`, `2gT` ⇒ back-2, Esc cancels pending G, bare `t` still
  char-search, unknown ex still cancels, `gg` still ignores counts.

**PR-D — mouse** (stacks on B+C; last because it layers on all three and is
the only PR touching terminal-mode behavior)
- Prompt-scoped `MouseCaptureGuard` + the double gate; `Event::Mouse` arm +
  per-event size re-read; left-click `Goto`; right-click `BarMenu` overlay
  (static five items, house grammar, **rename-by-prefill** — no text widget).
- Tests: hit-map math (pure fn); gating matrix (opted-out / single-tab emits
  zero capture bytes); menu state machine TTY-free; menu-outcome →
  `TabAction` mapping.

Per repo law: one issue per PR — file #1669 sub-issues for A–D; every PR runs
`just check` locally (the #1098 `--no-verify` exception applies to the slow
hook only; CI on the PR is the gate); no pipeline files change, so no
hook-parity audit is triggered.

## Rejected alternatives

- **Reading `self.count` inside `Pending::G` for `<n>gt`** (both losing
  proposals). Verified false: `take_count()` at `vi.rs:305` zeroes the count
  before `'g'` arms the pending at `vi.rs:381`; as specified it ships a broken
  `2gt` that silently degrades to `gt` — a defect in an operator-fixed
  requirement. Judges docked both proposals for propagating the survey's
  wrong fact on the one problem the task said to verify; the
  `Pending::G(Option<usize>)` fix is the reason the winning key design works
  and additionally preserves the `gt`-vs-`1gt` distinction min-1 designs
  cannot express.
- **Resident per-tab live state** (each `TabState` owning its own
  `MemoryManager`, scratchpad, ledgers; switch = in-RAM swap). Judges: the
  benefit (instant switch) doesn't matter at human speed when store restore is
  ms-scale; it changes the memory profile (N resident context windows +
  `max_tabs` knob + footprint measurement), deviates from the operator's fixed
  "tab switch reuses exactly that [resume] apply path," and its price is a
  behavior-frozen mechanical refactor of the hottest 5,200-line function —
  hundreds of lines, multi-day borrow-checker fights, maximal merge-conflict
  exposure against #1668, near-unreviewable at that diff size.
- **herdr monotonic tab numbers.** Incoherent with positional `<n>gt`: after
  one close the bar can read `1: 3: 7:` while `3gt` targets *position* 3 —
  the count prefix and the bar disagree, a quiet bar-honesty violation a vim
  operator hits immediately. Positional renumber-on-close (vim's rule) wins;
  herdr contributes the shape and op semantics, not the numbering.
- **Prompt mouse capture without the ≥2-tab gate.** Opted-in single-tab users
  would lose native terminal selection at the idle prompt for nothing; the
  double gate keeps capture a progressive enhancement paid only when a bar
  exists.
- **No pending-inject badge / deferred badge.** Leaves background web prompts
  queued invisibly — a direct bar-honesty failure; the throttled
  once-per-`read_line` poll is cheap enough to land in PR-B.
- **`active_roadmap_id` as session-global.** Documented cross-conversation
  bleed; it is conversation-scoped and rides the sidecar.
- **Carrying the unsubmitted buffer to the *next* tab** (global type-ahead
  carry). Moves half-typed conversation-A text into conversation B's prompt;
  the per-tab `input_stash` (grafted from the resident-state proposal)
  delivers the tmux-familiar continuity without the wrong-tab leak.
- **Close = `end_conversation`** (this proposal's original semantics).
  Destructive default from a UI action; vim closes tabs without deleting
  buffers. Release-without-end keeps the conversation open and `/resume`-able;
  `/end` stays the ending verb. (Consequence, stated: a closed tab's
  conversation exits the session without close-time note extraction — same as
  switching away; extraction still runs on `/end` and `/new`.)
- **In-menu rename text field** (this proposal's original PR-D). The only new
  editing widget in the design, and its most fragile piece; rename-by-prefill
  through the merged #1671 `/rename` path deletes it outright.
- **Merging the bar into the row-0 header.** The header is anchored at the
  viewport *top* and already dense (clock, vi mode, session, model@endpoint,
  gauge); the operator fixed bottom anchoring.
- **A new prompt-level ex-command mode.** The vi ex-line already is the house
  ex surface; `:tab*` extends its existing match.
- **Tab-key tab-cycling, tmux-prefix or leader chords, `C-x t` for
  emacs/nano.** Tab/BackTab are jumplist; the operator banned prefix/leader
  shapes (newt runs inside tmux and herdr); emacs/nano get the `/tab` family
  and the mouse — matching precedent (they already live on slash commands).
- **`:q` closes the tab.** Guaranteed vim-expectation delta either way;
  repurposing a binding that today ends the whole session is the worse
  surprise. Documented in help.
- **Sparse pin overlay as tab-activation semantics** (this ADR's original
  "switch IS resume, verbatim"). With A pinned `backend=nemotron,
  cognition=high` and B unpinned, `A→B` applies an empty overlay and B runs
  on A's backend and cognition — the activated tab's behavior depends on
  which tab was visited before it, and at B's next saved turn the leaked
  values are captured into B's row, making the bleed **durable**. The
  reset-then-overlay activation verb is the fix; the sparse overlay is kept
  where it is correct (the resume verb).
- **Making `/resume` use reset-then-overlay too** (one verb everywhere).
  Would silently change single-session behavior #1684 deliberately chose:
  resuming a conversation from inside a working session should carry the
  operator's current unpinned dials forward, not snap them back to launch
  values. Two verbs, each documented, beats one verb that is wrong half the
  time. (PR-A carries a regression test pinning the resume verb's
  semantics.)
- **Write-through pin persistence on every dial mutation.** Would close the
  dirty window to zero, but puts a store write behind every `/cognition`,
  `/tenacity`, `/model`, `/backends`, and psyche-panel keystroke-commit —
  N writes per minute of dial-fiddling, and it destroys the saved-turn
  pin's meaning ("the posture this conversation's turns actually ran
  with"). The deactivation flush closes the *observable* hole (switch-away
  and exit) at three seams instead of N.
- **Per-tab baseline snapshots** (each tab remembering the live posture at
  its creation, and resetting to *that*). Reintroduces history-dependence
  through the back door: tab B created while A was pinned would inherit A's
  posture as B's "baseline" forever, and `create` would no longer be
  reproducible. One immutable session baseline keeps `activate` a pure
  function of (baseline, pin).
- **Persisting the tab set across restarts.** Tempting, but it makes crash
  recovery a state-restoration problem (which claims, which order, which
  active) for furniture; v1 keeps tabs session-scoped and leans on the fact
  that every tab's durable content is already an ordinary resumable
  conversation row. Revisit only with a concrete operator ask.

## Risks

1. **#1668/#1684 seam risk (the load-bearing dependency).** #1684's
   `restore_pinned_posture` is a *sparse overlay over live state* — correct
   for the resume verb, wrong for activation. If PR-A reuses it without the
   Stage-3 baseline reset, tab switches leak backend+psyche between tabs and
   the leak becomes durable at the next saved turn. Mitigation: the reset is
   a separate, explicitly-tested step (`activate(B)` history-independence and
   unpinned-fields-show-baseline are two of the six normative properties),
   plus the PR-A two-pins switching contract test and the coordination asks
   on #1684's review.
2. **Posture apply is network-adjacent.** Adoption probes / served-validation
   / warmup can stall a switch toward an unreachable backend. Mitigated by the
   bounded-apply contract and the defined failure semantics (`!pin` badge,
   quintet at the session baseline, turn refused on persistent failure) — but
   the failure UX is new surface and needs a UAT scenario. Note the
   deliberate asymmetry with #1684's single-session fail-open: on *resume*,
   an unusable pin leaves the operator's live posture alone; on *activation*,
   it lands at the baseline, because "leave it alone" there means "keep the
   previous tab's posture", which is the bug this ADR exists to close.
3. **Deactivation flush cost and failure.** Every switch now writes the
   outgoing row's pin. It is a small metadata write with no MRU tick
   (#1684's `update_posture` already has these properties, verified), but it
   is on the interactive switch path: it must stay non-blocking-cheap, and a
   failed write must only warn (the row keeps its last-good pin) rather than
   abort the switch. A slow store turns `gt` into a stutter — PR-A should
   measure it.
4. **Claim-lifecycle widening.** N held claims per process changes crash
   semantics: a dead newt leaves N stale claims, and today's exit paths
   release only the active conversation. Must verify `store.claim`'s
   stale-reclaim tolerates multiple claims from one dead pid (and decide
   whether newt-core needs a distinguishable `HeldBySelf` outcome — small
   newt-core change if so), and extend every exit path via `release_all`.
   `Fatal`/panic may still leak claims exactly as a single-session crash does
   today — same blast radius, more rows.
5. **run_chat carve-out hazard.** Mis-sorting one conversation-scoped local as
   global silently bleeds state between tabs (or resets an operator dial on
   switch). The sidecar audit currently closes the set
   (`pending_retry` provably `None` at read; `pending_clarification`
   rehydrated by the existing path; the four sidecar fields), but the
   guarantee is fragile: mitigation is an explicit sorted-locals table in
   `tabs.rs` docs, the two-tab isolation test, and a standing checklist
   comment at the locals block — every *future* conversation-scoped local
   must be sorted into row / sidecar / global or it resets on switch.
6. **Prompt-scoped mouse capture trade.** Even double-gated, opted-in
   multi-tab operators lose native click/selection at the idle prompt
   (Shift+click escape hatch); and the known SIGTERM/SIGHUP capture-leak gap
   (`mouse.rs:24-29`) now applies at the prompt — a much longer exposure
   window than a turn. Shipping mouse last (PR-D) means A–C deliver full
   value if D is rejected or gated further.
7. **Hit-test geometry drift.** The design assumes the bar is the terminal's
   last row; a mid-prompt resize or any future extra bottom row (e.g. the
   in-flight command palette) shifts it. Mitigations: per-event terminal-size
   re-read, and the single shared `layout_tab_cells` fn — but a palette
   landing concurrently must join the same height budget and bump the bar-row
   computation in one place.
8. **Terminal-height budget.** Header + input(≤8) + ex row + background + bar
   + open menu can exceed a short terminal; an inline viewport taller than the
   screen corrupts scrollback. The `want.min(term_rows)` clamp is required in
   PR-B and the menu must refuse to open on tiny terminals (PR-D).
9. **Background-tab injects change observable behavior.** Today an injected
   prompt runs on the next loop iteration; in a background tab it waits,
   badged, until the operator switches — correct for sole-writer and
   posture-correctness, but a docked newt-web user will perceive a hang.
   newt-web/dock docs must surface "queued, waiting on operator," and the
   badge polling adds throttled store reads.
10. **type-ahead seam reuse.** The per-tab `input_stash` restore and the
    rename-prefill both ride the global type-ahead buffer shared with mid-turn
    keystroke capture; single-threaded switching makes interleave unlikely but
    PR-C carries a regression test, and a dedicated `InputSurface` seed method
    is the fallback if review prefers more trait surface over the repurpose.
11. **Doctrine surface.** Lean must stay single-session; the `/tab` refusal
    keeps the boundary honest, and this doc (landing in PR-A, before any
    pixels) satisfies the revisit rule pre-emptively by recording `/resume` +
    `/new` + `/rename` as lean's equivalent path. Any future "tabs on lean"
    request triggers a **new** decision doc before the surface change
    (`plain_scroller_tui.md:287-289`) — the line is pre-drawn here.
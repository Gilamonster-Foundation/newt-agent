# Decision: bottom-anchored session tabs on the RichTUI surface (#1669)

**Status:** Proposed — ADR draft for operator acceptance. An application of the
2026-08-11 rich-surface amendment to `docs/decisions/plain_scroller_tui.md`
(inline viewport, TTY-gated, severable) and of the interaction grammar in
`docs/decisions/harness_config_panel.md` (which names #1669 as a consumer).
**Date:** 2026-08-14
**Related:** #1669 (this feature), **#1668 (per-conversation `PosturePin` —
HARD dependency, unbuilt as of this draft)**, #1671 (session rename — merged;
tab labels ride it for free), #1030 (the in-session `/resume` path this design
generalizes), #531 (ex-command row), #416/#419 (`InputSurface` seam),
`docs/decisions/lean_rich_tui_morphologies.md`,
`docs/decisions/live_spill_viewport.md`, herdr `src/workspace.rs` /
`src/workspace/tab.rs` (shape reference only).

Operator-fixed constraints (non-negotiable inputs to this design):
bottom-anchored tab bar; one conversation per tab, each carrying its own
backend+psyche posture via #1668's `PosturePin` applied through the existing
setters on resume; dials stay process-global (correct: one tab is ever active
in the single-threaded REPL); auto-names replaced by `/rename` (#1671); vim
navigation `gt`/`gT`/`<n>gt` + `:tabnew`/`:tabclose`/`:tabn`/`:tabp`; **no**
tmux-prefix or herdr-leader chords (newt runs inside both); right-click tab
context menu via the existing mouse layer; rich-tui + TTY only — lean keeps
single-session behavior; no alternate screen, ever.

---

## TL;DR

A newt tab is a **held conversation**, and the REPL stays single-threaded — so
the whole design collapses to **"N claims, one live restore."** Switching a tab
reuses the proven in-session `/resume` machinery
(`restore_conversation_into_session`, `newt-tui/src/lib.rs:5790`) with exactly
four deltas: no claim release on the outgoing side, #1668's `PosturePin` apply
on the incoming side (bounded, with defined failure semantics), a small
`TabSidecar` stash for the four locals the store cannot restore, and a per-tab
`input_stash` so a half-typed prompt survives switching away and back. The bar
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

### P1 — Session multiplexing: a tab is a held conversation; switch IS the in-session `/resume`

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

**Global (shared across tabs, on purpose):** `cfg`, operator baseline
`base_provider`/`base_model`, `session_cwd`, the `MemoryManager` *instance*,
`system` String, spill/compaction stores + session nonce, `semantic_index`,
`where_is_index`, `experience_store` (`chat.rs:1481-1538`), all
display/behavior overrides (`markdown_override` etc., `chat.rs:989-1024` —
operator dials, not conversation state), `active_posture`
(process-lifetime by declaration, `chat.rs:984-986`), and the
tenacity/cognition process-globals per the operator decision. The backend
quintet (`choice`/`inf_url`/`inf_model`/`inf_kind`/`inf_key`) stays a
`run_chat` local but is **rewritten on every switch from the incoming row's
`PosturePin`** — the row is the single source of truth; tabs carry no copy,
so divergence is unrepresentable.

**Switch mechanics — `perform_tab_switch` in `chat.rs`,** extracted from the
in-session `/resume` block (`chat.rs:3547-3649`) with four deltas:

1. **Skip the outgoing release** (`chat.rs:3571-3574`) — every open tab holds
   its claim from open to close. (`/resume` inside a tab keeps releasing: it
   *replaces* the active tab's conversation.)
2. **Stash the outgoing side:** sidecar fields + the unsubmitted input buffer
   into `input_stash`.
3. After `resume_session_conversation` → `restore_conversation_into_session`
   (memory `restore_turns`, scratchpad, step ledger, persona, prompt context,
   `compress_state` reset, id adoption, system rebuild — the one trusted
   path), **apply the incoming conversation's `PosturePin`** via #1668's
   apply-on-resume seam — the identical setters path (`apply_persona_backend`,
   the `/model` path, `set_persona_tenacity`/`set_persona_cognition`) — so the
   next `set_runtime_context` shows the incoming `model@endpoint`. **Header
   honesty is an invariant.**
4. **Restore the incoming side:** sidecar unstash; `input_stash` seeded
   through the existing type-ahead prefill (`rich_input.rs:1057-1060`) so the
   half-typed prompt reappears — the herdr/tmux "come back mid-thought"
   continuity, without carrying tab A's text into tab B.

Then emit a scrollback boundary banner (`── tab 2/3 · <title> ──` +
`auto_resume_banner`) via the print path — scrollback is the canonical log and
interleaved conversations need greppable seams.

**PosturePin apply contract (grafted requirement on #1668):**
`apply_persona_backend` runs adoption probes and `apply_model_choice` does
served-validation + Ollama warmup — network-adjacent work. The #1668 seam this
design consumes MUST be exposed as a **named callable** (e.g.
`apply_posture_pin_on_resume(...) -> Result<AppliedPin, PinApplyError>`) that
is **bounded and cache-tolerant**. Failure semantics, defined here: the switch
itself still lands (the restore is authoritative); on apply failure the route
quintet is left untouched, a warning prints to scrollback, and the header/bar
shows a `!pin` badge; the apply retries at the next turn head, and if it still
fails **the turn is refused with an error** rather than run under a posture
that contradicts the pinned row. No half-rewritten quintet, no silent
wrong-posture execution.

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
Every exit path (clean, `Eof`, `EndAndQuit`, `Fatal`) releases ALL held claims
via `TabSet::release_all` (extend `chat.rs:5888-5893`).

**In-flight turns:** structurally impossible to interrupt by switching — turns
are synchronous, `read_line` runs between turns, and the viewport (bar
included) does not exist during a turn. The honest UX rule, documented in
`/tab` help: one model turn at a time; Esc-interrupt first, then switch. Zero
blocking code is written.

**Memory:** one `MemoryManager` ever; a switch is a store row load +
`restore_turns` + system rebuild (ms-scale). RAM stays flat regardless of tab
count — 8 tabs is not 8 resident context windows. `compress_state.reset()`
inside restore is correct: a switch is a conversation boundary.

**Web injects:** only the active tab's inbox executes (unchanged,
`chat.rs:1789-1793`); background tabs get a bar badge instead. Running a
background inject would execute under the wrong process-global dials —
dormancy is a **correctness property**, not a limitation.

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

**Gate: #1668 (`PosturePin`) must merge before PR-A.** `PosturePin` exists
nowhere yet (zero grep hits over origin/main and worktrees); this design's
entire posture story is "switch IS resume, and #1668 makes resume apply the
pin." **Coordination ask on #1668's review (cheap now, expensive later):**
expose apply-on-resume as a named, bounded, cache-tolerant callable
(`apply_posture_pin_on_resume`) rather than inlining it in the `/resume` slash
branch — otherwise PR-A begins by refactoring freshly merged code. #1668's
review must also confirm the apply hooks the **in-session**
`resume_session_conversation` seam, not just the startup path — the exact gap
today's in-session `/resume` has (`chat.rs:3598-3613` never calls
`apply_persona_backend`).

**PR-A — tab model + `/tab` text engine** (depends: #1668; label `risk:high`)
- `newt-tui/src/tabs.rs`: `TabSet`/`TabState`/`TabSidecar` (incl.
  `interrupted_objective` and `input_stash`) + herdr-shaped ops, fully
  unit-tested TTY-free (bounds-checked switch, refuse-last close,
  active-index fixup, identity-preserving move, positional renumber-on-close).
- `chat.rs`: `perform_tab_switch` (resume-without-release + bounded
  `PosturePin` apply with the `!pin` failure semantics + sidecar
  stash/unstash + scrollback banner), `handle_tab_action`, the `/tab` slash
  family, `rename_conversation` helper extraction, claim-per-tab lifecycle
  (stop releasing on switch; **close = release-without-end**; every exit path
  releases ALL claims via `release_all`, extending `chat.rs:5888-5893`),
  `/resume` tab-drop, lean + ephemeral refusals.
- This decision doc lands here (the revisit-rule doc precedes the surface).
- Tests: switch≡resume equivalence; two-tab state-isolation (the leakage
  test, incl. `interrupted_objective`); **two tabs with different
  `PosturePin`s → switch → assert route quintet + dials (the #1668 contract
  test)**; pin-apply-failure → route untouched + turn refused;
  outgoing-claim-held-across-switch; close-leaves-conversation-resumable;
  close-last-refused; exit-releases-all; ephemeral refusal.
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

## Risks

1. **#1668 seam risk (the load-bearing dependency).** If `PosturePin`'s apply
   lands only on the startup resume path, or inlined rather than as a named
   callable, tab switches leak backend+psyche between tabs — the exact gap
   today's in-session `/resume` has. Mitigation: the PR-A two-pins switching
   contract test, and the named-callable + bounded/cache-tolerant + in-session
   seam asks placed on #1668's review *now*.
2. **Posture apply is network-adjacent.** Adoption probes / served-validation
   / warmup can stall a switch toward an unreachable backend. Mitigated by the
   bounded-apply contract and the defined failure semantics (`!pin` badge,
   route untouched, turn refused on persistent failure) — but the failure UX
   is new surface and needs a UAT scenario.
3. **Claim-lifecycle widening.** N held claims per process changes crash
   semantics: a dead newt leaves N stale claims, and today's exit paths
   release only the active conversation. Must verify `store.claim`'s
   stale-reclaim tolerates multiple claims from one dead pid (and decide
   whether newt-core needs a distinguishable `HeldBySelf` outcome — small
   newt-core change if so), and extend every exit path via `release_all`.
   `Fatal`/panic may still leak claims exactly as a single-session crash does
   today — same blast radius, more rows.
4. **run_chat carve-out hazard.** Mis-sorting one conversation-scoped local as
   global silently bleeds state between tabs (or resets an operator dial on
   switch). The sidecar audit currently closes the set
   (`pending_retry` provably `None` at read; `pending_clarification`
   rehydrated by the existing path; the four sidecar fields), but the
   guarantee is fragile: mitigation is an explicit sorted-locals table in
   `tabs.rs` docs, the two-tab isolation test, and a standing checklist
   comment at the locals block — every *future* conversation-scoped local
   must be sorted into row / sidecar / global or it resets on switch.
5. **Prompt-scoped mouse capture trade.** Even double-gated, opted-in
   multi-tab operators lose native click/selection at the idle prompt
   (Shift+click escape hatch); and the known SIGTERM/SIGHUP capture-leak gap
   (`mouse.rs:24-29`) now applies at the prompt — a much longer exposure
   window than a turn. Shipping mouse last (PR-D) means A–C deliver full
   value if D is rejected or gated further.
6. **Hit-test geometry drift.** The design assumes the bar is the terminal's
   last row; a mid-prompt resize or any future extra bottom row (e.g. the
   in-flight command palette) shifts it. Mitigations: per-event terminal-size
   re-read, and the single shared `layout_tab_cells` fn — but a palette
   landing concurrently must join the same height budget and bump the bar-row
   computation in one place.
7. **Terminal-height budget.** Header + input(≤8) + ex row + background + bar
   + open menu can exceed a short terminal; an inline viewport taller than the
   screen corrupts scrollback. The `want.min(term_rows)` clamp is required in
   PR-B and the menu must refuse to open on tiny terminals (PR-D).
8. **Background-tab injects change observable behavior.** Today an injected
   prompt runs on the next loop iteration; in a background tab it waits,
   badged, until the operator switches — correct for sole-writer and
   posture-correctness, but a docked newt-web user will perceive a hang.
   newt-web/dock docs must surface "queued, waiting on operator," and the
   badge polling adds throttled store reads.
9. **type-ahead seam reuse.** The per-tab `input_stash` restore and the
   rename-prefill both ride the global type-ahead buffer shared with mid-turn
   keystroke capture; single-threaded switching makes interleave unlikely but
   PR-C carries a regression test, and a dedicated `InputSurface` seed method
   is the fallback if review prefers more trait surface over the repurpose.
10. **Doctrine surface.** Lean must stay single-session; the `/tab` refusal
    keeps the boundary honest, and this doc (landing in PR-A, before any
    pixels) satisfies the revisit rule pre-emptively by recording `/resume` +
    `/new` + `/rename` as lean's equivalent path. Any future "tabs on lean"
    request triggers a **new** decision doc before the surface change
    (`plain_scroller_tui.md:287-289`) — the line is pre-drawn here.
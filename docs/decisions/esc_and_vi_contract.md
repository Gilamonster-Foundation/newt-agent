# Esc, vi, and the turn: what codex does, what newt does, and what to change

**Question asked:** "The escape is more important for the harness turn interaction than it is for the VIM interaction… we got the VIM behavior subtly wrong. Take a look at how Codex handles VIM mode."

**Short answer:** your instinct is the industry convention, and codex implements almost exactly it. But the vim behavior newt got wrong is not the Esc *binding* — `vi.rs:222-227` is vim-correct. It is that **newt destroys the entire vi state on every submit**, so the operator is never sure which mode they are in, which is what makes Esc feel like a toggle. Fix the amnesia first; the interrupt rung is then a ten-line change to one match arm.

---

## 1. What codex does

Codex partitions Esc on **one predicate**, not on staging.

| Turn running? | Esc means |
|---|---|
| yes | **interrupt, single press, no timer** |
| no | prime / advance Esc-Esc "edit previous message" (backtrack) |

Mutually exclusive by construction: `should_interrupt_running_task` requires `is_task_running` (`tui/src/bottom_pane/mod.rs:1310-1324`), `is_normal_backtrack_mode()` begins with `!is_task_running` (`:1346-1352`). So "is this the first or second Esc?" never arises for interrupt.

The interrupt predicate is worth reading whole, because it is the whole design:

```rust
self.keymap.chat.interrupt_turn.is_pressed(key_event)   // :1318  default = plain Esc (keymap.rs:921)
    && self.is_task_running                             // :1319
    && !(is_agent_command && code == KeyCode::Esc)      // :1320
    && self.no_modal_or_popup_active()                  // :1321
    && !self.composer_should_handle_vim_insert_escape(..) // :1322
    && self.status.is_some()                            // :1323
```

Everything that outranks interrupt is named as a negative conjunct in six lines, rather than as call ordering across five files. The staged Esc-Esc lives only in the idle half (`app_backtrack.rs:225-292`), is **count-based not timer-based**, and is legible in the footer at every stage (`esc esc to edit previous message` → `esc again to…`, `footer.rs:871-882`). The destructive gesture (Ctrl+C/Ctrl+D quit) is the one that got a timer, not this.

**vim's place in that stack.** Codex's rule is a one-line ownership statement:

> **Only vim INSERT owns Esc. vim NORMAL owns nothing — Esc falls through to the harness.**

`should_handle_vim_insert_escape` (`textarea.rs:323-334`) is applied as a **veto at six sites** — `app/input.rs:262` and `:269`, `chatwidget/interaction.rs:129` and `:139`, `bottom_pane/mod.rs:1322`, `chat_composer.rs:3197` — never as a mode branch. Its doc comment states the hazard exactly: *"In Vim insert mode, Escape is an editing transition rather than a popup cancel/backtrack shortcut. Letting the composer handle it first would close UI surfaces while leaving the textarea in insert mode."* Tests pin the two-press sequence: `pending_steer_esc_does_not_steal_vim_insert_escape` (`chatwidget/tests/composer_submission.rs:1205-1231`) asserts Esc #1 from insert emits no `Op`, Esc #2 emits `Op::Interrupt`.

**Mode persistence.** `VimMode` lives on the long-lived `TextArea` (`textarea.rs:122-124`), owned by a `ChatComposer` built once in `BottomPane::new` (`mod.rs:273`). No render, turn, history-recall or text-restore path touches it (`input_restore.rs:284-302` restores text/images/pastes and nothing else). Enabling vim lands in **Normal**, not Insert (`textarea.rs:242-250`). The one deliberate reset goes to **NORMAL after a successful dispatch** (`chat_composer.rs:2867-2878`, tested at `:5974`). Codex's residual gap: the vim *enable bit* is dropped on `ChatWidget` replacement (`/new`, fork, resume) — do not copy that.

**No `:` layer at all.** `grep "Char(':')"` over `codex-rs/tui/src` returns nothing. Codex spends `/` (vim's search key) on the slash menu instead. There is no codex precedent for `:help`.

**Two codex behaviors not to copy.** (a) During a running turn, `d` then Esc **interrupts the turn** instead of cancelling the pending operator — only *insert*-mode escape is subtracted from the predicate, and `BottomPane::handle_key_event` returns at `mod.rs:634-641` before the composer's operator-pending branch at `chat_composer.rs:3200` is ever reached. (b) `will_interrupt_turn_on_key_event` looks like an opt-in authorization gate for modal interrupts; it is not — it is a reporting hook whose only consumer pauses the active goal (`interaction.rs:20-26`), while the modal issues `app_event_tx.interrupt()` unconditionally from its own handler (`request_user_input/mod.rs:1218-1224`). Not a seam worth importing.

**Cross-harness check** (four other agent TUIs read): claude-code-rust binds **Esc = interrupt, Ctrl-C = quit** (`src/app/keys.rs:208-231`, `:65-68`). crush binds Esc double-tap within 2 s to cancel, Ctrl-C to quit. DeepSeek-TUI — the only other harness shipping a vim composer — guards its vim arm on `vim_mode != Normal` (`crates/tui/src/tui/ui.rs:5322`) *specifically so* NORMAL-mode Esc reaches `CancelRequest`, and never resets vim mode on submit or remount. reedline's vi layer **transforms Esc without consuming it** — it changes mode and still emits `ReedlineEvent::Esc` upward (`vi/mod.rs:165-176`). Nobody moves the interrupt off Esc; nobody lets vim swallow Esc unconditionally.

---

## 2. What newt does today

**The shipped path is the cockpit.** `newt-cli/Cargo.toml:66` default features + `config.rs:712` `FooterMode::Auto` + `prompt.rs:116` ⇒ on a unix TTY, rich surface → cockpit.

| Axis | cockpit (shipped) | classic / lean |
|---|---|---|
| Esc during a turn | **nothing** — `presenter.rs:961-996` has only a Ctrl-C arm; Esc falls to `editor.on_event` at `:991` | **interrupts** — `lib.rs:11735-11757`, `is_lone_esc` at `lib.rs:10309` |
| Interrupt key | Ctrl-C only, tiered (`:970-989`: 1st = `cancel` + `set_interrupt_pending`, 2nd+ = `hard`) | Ctrl-C **and** Esc, same tiers (`lib.rs:11771-11783`) |
| Editor live during a turn? | **yes** — `Presenter::run` (`:716-732`) polls keys unconditionally; mid-turn submits queue (`:1034-1041`) | no — editor rebuilt per read (`rich_input.rs:1548`), torn down on submit |

So **#2005's "today NO Esc path does this" is false for the codebase** and true only for the shipped surface. Esc-interrupt was built, tiered, grace-windowed and unit-tested — and then the cockpit took the keyboard and deliberately declined to port it. `presenter.rs:31` records the choice verbatim: *"Ctrl-C during a turn interrupts (Esc belongs to vi)."* This is a regression by decision, not a missing feature, which makes it cheap to reverse.

**The crux: what would Esc-interrupt structurally require?** Almost nothing.

- `turn_cancel` / `turn_hard` are already `Arc<AtomicBool>` shared with the terminal thread (`chat.rs:7168-7178`, comment: *"Shared with the terminal thread under the cockpit, which trips them from Ctrl-C"*).
- "Is a turn running" already exists as `self.turn.is_some()` — and it is the **same** flag that drives the spinner, so newt gets codex's affordance/behavior coupling for free from one flag where codex needs two (`status.is_some()` plus a separate `interrupt_hint_visible`).
- The landing zone is already wired: `chat.rs:7702-7712` prints `⊘ interrupted — back to you` and calls `persist_incomplete_turn(… TurnEndReason::Cancelled …)`. #2005's "must land on the #1976 persisted-turn path" is already satisfied by whatever trips `turn_cancel`.
- The permission modal cannot be stolen from: on `SurfaceRequest::Interact` the presenter blocks *inside* `handle_request` (`presenter.rs:813-877`) and never reaches `poll_keys`, so a new Esc arm structurally cannot fire during a prompt. No guard needed.

**Two things newt must not port from the classic watcher.** The 200 ms `escape_grace_ms` split-escape window (`lib.rs:11675`) is an artifact of reading raw bytes off fd 0. The cockpit uses crossterm `event::read()` (`presenter.rs:955`), which already disambiguates ESC from CSI — codex's event stream likewise does no Esc special-casing (`tui/src/tui/event_stream.rs:237-255`). And `EditorOutcome` (`rich_input.rs:1626-1637`) has five variants and no interrupt: the editor currently has no channel to say "I declined this key," which shapes the recommendation below.

**Esc consumers under the cockpit, in the order they see it:** palette (`rich_input.rs:1848` → `palette.rs:371`, swallowed) → vi `[y/N]` confirm (`vi.rs:184-191`) → vi ex line (`vi.rs:143`) → vi INSERT→NORMAL (`vi.rs:208-211`) → vi NORMAL cancel-pending, then no-op (`vi.rs:222-227`). emacs/nano: silent no-op (`rich_input.rs:350-354`). Panels (`config_panel`, `backend_panel`, `transcript_pager`, `splash`, `setup_tui`) each run their own blocking `event::read()` loop with their own Esc — five independent regimes, no shared dispatcher.

---

## 3. The subtle wrongs — four, ranked

**1. Every submit rebuilds the whole vi state, not just the mode.** `reset_after_submit` does `self.editor = Editor::new(self.edit)` (`rich_input.rs:1967`), and `Editor::new` hard-codes `vi: Vi::new()` (`:241-247`), and `Vi::new()` hard-codes `mode: Mode::Insert` (`vi.rs:75-88`). So Enter silently discards mode **plus** the jumplist (`jback`/`jfwd`), the `;`/`,` repeat target (`last_find`), pending, and count. #2006 names only the mode. This is the operator's "Esc toggles": you were in NORMAL, you sent a line, you are now in INSERT without being told, so your next Esc changes mode instead of doing what NORMAL Esc does. Reset-at-submit *has* precedent (reedline resets to Insert; codex resets to Normal) — but as a **chosen transition**, not as debris from a rebuild, and neither of them throws away the jumplist.

Also: #2006's stated cause is wrong for the shipped path. Under the cockpit the `MountedEditor` is built **once** (`presenter.rs:615`) and only rebuilt on `SurfaceRequest::Reload` (`:769`). It is not "remounted per prompt/turn." Four sites reset the mode, all through `Editor::new`: the one-time mount, `Reload`, the classic driver's per-read mount, and submit.

**2. Ctrl-C at an idle prompt nukes the draft *and* the mode.** `rich_input.rs:288-294` rebuilds the textarea and does `self.vi = Vi::new()`. Real vim's `i_CTRL-C` is insert→normal; tui-textarea's canonical vim example maps it exactly that way (`examples/vim.rs:381-386`). Clearing the draft is defensible (codex does the same, `clear_composer_for_ctrl_c`); silently flipping to INSERT is a second, independent source of the same disorientation as (1).

**3. Under the cockpit, vi is the one surface where the interrupt key cannot reach the harness.** `vi.rs:222-227` is *vim*-correct — Esc cancels a pending sequence, then is idempotent. The wrongness is at the layering level: newt made the vi layer **terminal** for Esc where reedline makes it **transformative**. But note the refutation: nothing above vi acts on Esc either. The presenter sees Esc first (`presenter.rs:961`) and declines it; `EditorOutcome` has no interrupt variant. So `vi.rs:226` is not the load-bearing line — there is simply no rung at the bottom of the ladder.

**4. The mode hint lies.** `rich_input.rs:385-398` advertises `^C interrupt` in **both** vi modes, including at an idle empty prompt where Ctrl-C does not interrupt anything — it clears your draft (see 2). Codex's discipline is that the affordance and the behavior share a condition. Whatever else changes, this string must become turn-conditional.

---

## 4. Recommendation — the Esc contract

**Esc, under the cockpit, first match wins:**

| # | Context | Esc does | Where it already lives |
|---|---|---|---|
| 1 | Modal open (permission / ask_question) | cancel / back | `tty/modal.rs:147` → `interaction_terminal.rs:120`; unreachable by any new arm — presenter is parked in `handle_request` |
| 2 | Palette open | close palette | `palette.rs:371` |
| 3 | vi `[y/N]` confirm pending | cancel confirm | `vi.rs:184-191` |
| 4 | vi `:` ex line open | cancel ex line | `vi.rs:143` |
| 5 | vi INSERT | → NORMAL, cursor Back | `vi.rs:208-211` |
| 6 | vi NORMAL with pending operator / count / `i_CTRL-O` | cancel the sequence, stay NORMAL | `vi.rs:222-227` |
| 7 | **turn running** (everything above declined) | **interrupt** — 1st press `cancel` + `set_interrupt_pending`, 2nd+ `hard` | **new rung**; flags at `chat.rs:7168-7178` |
| 8 | anything else (vi NORMAL idle, emacs, nano, lean idle) | no-op | today's behavior |

**Yes — vim NORMAL-mode Esc should interrupt a running turn.**

The defense against the alternative ("Esc belongs to vi; use Ctrl-C"), which is what `presenter.rs:31` chose:

- It makes vi the only newt surface where the conventional interrupt key does nothing, for no vim-fidelity gain: NORMAL Esc in real vim, once nothing is pending, is *defined* as a harmless no-op. Rung 7 costs the vi user nothing they had.
- Every comparable harness routes it: codex (`bottom_pane/mod.rs:1310-1324`), DeepSeek-TUI (`ui.rs:5322`, explicitly so Esc reaches `CancelRequest`), reedline (emits `Esc` upward and lets the host decide).
- Ctrl-C is a *worse* candidate under vi, not a better one: it is `i_CTRL-C`, a vim mode-exit key.
- newt's own classic path already ships exactly this ladder, with the same tiering, and #1704 already established the precedent that a higher-precedence consumer (spill explore mode) preempts the interrupt rung (`lib.rs:11751-11754`). Rung 7 is not a new policy; it is the cockpit rejoining the policy.

**Rung 6 above rung 7 is where newt should beat codex.** Codex subtracts only *insert*-mode escape from its predicate, so mid-turn `d` then Esc kills the turn (`mod.rs:634` returns before `chat_composer.rs:3200`). newt already tracks `pending`, `count` and `insert_normal` in `Vi`, so honouring them costs one extra `||` in a predicate. Take the free win.

**What carries the interrupt when Esc cannot:** Ctrl-C, unchanged, at every rung. The `presenter.rs:970` arm is untouched and remains unconditional while a turn runs, so rungs 1–6 can never strand the operator. That property is what makes it safe to let the editor's claims outrank interrupt — same as codex, where Ctrl+C is independent of vim.

**Where to put the decision.** Not in the presenter reaching into the editor, and not by making `vi.rs:226` propagate (there is nothing to propagate to). One accessor on `MountedEditor` — `esc_claimed(&self) -> bool`, delegating to `Editor`, ORing palette-open / confirm / ex / INSERT / pending-nonempty — defined **next to the claimants** in `rich_input.rs`/`vi.rs`, with the presenter arm reading:

```rust
Event::Key(key) if key.code == KeyCode::Esc && self.turn.is_some()
    && !self.editor.esc_claimed() => { /* same body as the Ctrl-C arm */ }
```

This is codex's one-readable-predicate lesson with codex's own bug fixed: the predicate lives where the claimants live, so a new Esc consumer (PR8's shell) cannot forget to register. The alternative — an `EditorOutcome::Interrupt` variant so the ladder terminates inside the editor — is structurally purer and worth doing *if* a third driver ever needs it; today it buys nothing the accessor doesn't, for more lines.

**Do not add a staged Esc.** Codex's Esc-Esc backtrack is only affordable because codex's idle Esc has no other owner. newt's Esc budget is fully spent on vi. Single press, no timer, no double-tap.

**As-filed corrections:**

- **#2005** — retitle. Esc-interrupt exists and works on lean/classic (`lib.rs:11735`); the cockpit dropped it deliberately. Drop the 200 ms grace window from scope (crossterm handles it). The persisted-turn requirement is already met by `chat.rs:7702-7712`. Add rung 6 (pending-sequence cancel outranks interrupt) to the acceptance criteria.
- **#2006** — the diagnosis is wrong ("remounted per prompt/turn" is false under the cockpit) and the scope is too narrow (jumplist and `last_find` are lost too). Root cause is `Vi::new()` hard-coding `Mode::Insert` at `vi.rs:77` while every `Editor::new` runs it. One fix site: carry the mode (and jumplist/last_find) through `Editor::new`, the way `presenter.rs:768` already carries the draft. Add: delete `self.vi = Vi::new()` at `rich_input.rs:290`.
- **#2007** — verdict stands, mechanism wrong. `help_text(Edit::Vi)` returns **three** lines (`rich_input.rs:187-203`), not one, and `/help` is already a separate slash-registry command (`slash_registry.rs:191`), so the two surfaces are not entangled. #2007 is "give `:help` a docked, scrollable region," full stop. No harness in the corpus has an ex-command layer — codex, DeepSeek-TUI, crush, claude-code-rust, reedline all have zero. Argue it from vim, not from precedent.

---

## 5. Cost and sequencing

| Work | Cost | Notes |
|---|---|---|
| **#2006** — mode survives `Editor::new` | small: one field threaded, one line deleted at `rich_input.rs:290` | The classic driver rebuilds per read *by design*, so codex's "keep it on a long-lived struct" is not available — newt must carry it explicitly. |
| **#2005** — rung 7 | small: one accessor + one presenter arm + tests | Flags, tiers, acknowledgment and the persisted-turn landing all already exist. |
| Mode-hint truthing (#4 above) | trivial | `rich_input.rs:389-394` becomes turn-conditional; `^C interrupt` only while a turn runs. |
| **#2007** — `:help` region | medium, and genuinely new | Reuse `transcript_pager` rather than inventing a region; it already owns a scroll loop and an Esc/`q` close. |

**Order: #2006 before or with #2005.** They are filed as independent; they are not. Rung 7 makes the mode indicator load-bearing — an operator who believes they are in NORMAL but was silently reset to INSERT will press Esc expecting an interrupt and get a mode change. Shipping #2005 alone makes the amnesia *more* visible, not less. #2005 before PR8 is already correct in #2009 and should stay.

**Before PR8 adds its Esc context**, the accessor from #2005 must be the registration point: the rich `/settings` shell registers as a claimant (rung ~2, alongside the palette) rather than adding a sixth independent `event::read()` loop like `config_panel`/`backend_panel` do today. That is the difference between one ladder and six.

**Out of scope, confirmed:** headless / `protocol_mode` has no cockpit and no watcher; the lean surface already interrupts on Esc via the watcher and needs nothing (`lean_input.rs:168-198` has no Esc arm, correctly).

---

## 6. Open questions, with recommendations

1. **Does submit reset the vi mode at all, and to what?** → **No reset.** Mode is session state (DeepSeek-TUI's position, and the one that matches "I put myself in NORMAL for a reason"). If you want one, reset to **NORMAL** (codex) — never INSERT. Either way it must be a chosen, tested transition, not a rebuild artifact. Jumplist and `last_find` survive regardless.
2. **Should vim NORMAL Esc interrupt?** → **Yes**, at rung 7, below pending-sequence cancel. Defended in §4.
3. **Make the interrupt key rebindable, as codex did (#24766)?** → **Not now.** One operator, one keyboard; codex needed it because Esc collided with a backtrack gesture newt does not have. Revisit only if someone asks for Esc back.
4. **Ctrl-C at an idle prompt: keep resetting the mode?** → **No.** Delete `self.vi = Vi::new()` at `rich_input.rs:290`; keep the draft clear. One line.
5. **Does an interrupt discard a queued line?** Today `queued` (`presenter.rs:1034-1041`) is untouched by `turn_cancel`. Codex explicitly preserves the draft across an Esc interrupt. → **Leave as-is, add a test** so it stays deliberate.
6. **`:help` — docked region or pager?** → **Pager.** `transcript_pager` already has the loop and the close key; a new docked region is a PR8-shaped problem, and #2007 is not blocking anything.
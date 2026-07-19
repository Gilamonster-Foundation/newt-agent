# Decision: a rich, mouse-driven spill viewport — an opt-in TTY capability tier

**Status:** Accepted (ratified by merge of #1303, 2026-07-19)
**Amends:** `live_spill_viewport.md` (its "post-completion scrollback" and "no mouse input" non-goals), `plain_scroller_tui.md` (the "no mouse handling" prohibition — now scoped to the non-interactive path + the base tiers), `lean_rich_tui_morphologies.md` (#527 — clarifies that this is orthogonal to the input morphology).
**Related:** #1235 / #1253 (bounded live-spill scrollback, keyboard), #416 / #527 (input morphologies), #771 (OSC-8 hyperlinks — the only prior clickable affordance).

---

## TL;DR

Add an **opt-in mouse layer** on top of newt's existing live-spill viewport: scroll with the wheel and by clicking the `▲`/`▼` rows, click `⧉`/`▣` to expand/collapse, and reopen a **bounded scrollable viewer over a completed tool's retained buffer** (superseding live-spill's post-completion non-goal). Keyboard navigation becomes **editor-mode-aware** (`/vi` · `/emacs` · `/nano`) on top of the always-present `↑`/`↓`/`Space`.

This is a **capability tier that degrades cleanly**: mouse-interactive TTY → keyboard-interactive TTY → non-interactive logger. Each tier is a strict subset of the one above. The mouse code is compile-time feature-gated (off in wyvern); mouse capture is held by an RAII guard released even on the panic/abandon path; and the non-interactive (piped / `newt worker` / `TERM=dumb` / headless) path is **byte-for-byte** unchanged.

## Context — and a correction to a common misconception

The live-spill viewport is **not** gated on the input morphology. It activates from `live_spill_capable()` (`newt-tui/src/lib.rs:5565-5588`):

```
platform_supported && feature_enabled && stdin_terminal && stdout_terminal && term != Some("dumb")
```

There is no `rich-tui` / footer / LeanTUI check anywhere in `live_spill.rs` / `spill_view.rs`. **So LeanTUI on a real TTY already runs the redraw viewport today** — the input morphology (`lean_rich_tui_morphologies.md`: footer + input widget) and the output viewport are *orthogonal* surfaces. Any design that says "lean = plain lines, viewport = rich only" is wrong about the code.

The correct axis for this decision is therefore **terminal capability**, not input morphology:

| Tier | Condition | Spill behavior |
|---|---|---|
| **Mouse** (new) | `live_spill_capable()` **AND** mouse opt-in on an interactive TTY | wheel/click scroll, click-expand, reopenable post-completion viewer, mode-aware keys |
| **Keyboard** (today) | `live_spill_capable()` | bounded live viewport, `↑`/`↓` scroll, `Space`/`Enter` expand — while a tool runs |
| **Logger** (today) | not a TTY / `TERM=dumb` / feature off / wyvern | plain committed lines; terminal scrollback is the history |

The two existing decisions forbid exactly the two things the operator wants on a real terminal — **mouse** (`plain_scroller_tui.md:29`, `:90`; `live_spill_viewport.md:135`) and **post-completion scrollback** (`live_spill_viewport.md:140-142`: "a future explicit viewer would require its own decision and interaction contract"). That last sentence is the explicit hook this decision fills.

## Decision

1. **Gate: an explicit opt-in on top of `live_spill_capable()`, on an interactive TTY.** The mouse tier activates only when `live_spill_capable()` is already true **and** the operator has opted in (a config key / flag, default off until the implementation stabilizes) **and** both **stdin and stdout are terminals** — mouse events arrive on **stdin**, so a stdin-piped session must never enable capture even if stdout is a TTY. There is no reliable "terminal reports mouse capability" probe; newt enables capture and tolerates a terminal that ignores it. The input morphology (lean/rich footer) is not consulted — this rides the viewport's own capability gate.

2. **Mouse capture is RAII-scoped and released even on the abandon path.** `EnableMouseCapture` is owned by a guard whose `Drop` emits `DisableMouseCapture`; it is created when a mouse-tier viewport opens and dropped when it closes, the terminal degrades, or the process unwinds. Crucially, `live_spill_viewport.md` rule 7 (`:107-116`) defines a teardown-miss path where a stalled presentation worker is atomically abandoned **without more terminal I/O** — the mouse guard MUST still release capture on that path (a process-level restore, e.g. a panic hook / final `Drop` on the owning handle), or the operator's terminal is left stuck in mouse-reporting mode. Guaranteed release is an acceptance criterion, not best-effort.

3. **Interactions the mouse tier adds** (a superset; the keyboard base always stays):
   - Wheel up/down and clicks on the `▲`/`▼` boundary rows scroll the buffered spill; `↑`/`↓` keep working identically.
   - Click `⧉` to expand / `▣` to collapse, up to "the safe terminal row budget" (`live_spill_viewport.md:70`) — **never a full alternate screen**, never a persistent dashboard.
   - **Post-completion viewer.** After a tool's canonical block is committed, the operator may reopen a bounded scrollable viewer over that tool's *retained buffer*. To stay inside the erase-geometry and "never rewrite committed scrollback" invariants (`live_spill_viewport.md:136-137`, rule 3/7), the viewer opens as a **fresh bounded overlay anchored at the current cursor row** (it does not redraw the old, possibly scrolled-away committed excerpt), takes **exclusive stdout + event-loop ownership** from the idle input surface while open (an explicit hand-off, mirroring how the active-tool coordinator owns stdout, `live_spill_viewport.md:60-65`), and on close releases ownership and restores the plain input prompt. The committed scrollback is never rewritten.

4. **Keyboard navigation is editor-mode-aware.** The session carries an editor keybinding (`/vi` · `/emacs` · `/nano`; `newt-tui/src/lib.rs:324`, `commands/settings.rs:93-99`). The viewport's keyboard navigation follows it, so reaching/moving through it feels native:
   - **vi**: `j`/`k` line, `C-d`/`C-u` half-page, `C-f`/`C-b` page, `gg`/`G` top/bottom.
   - **emacs**: `C-n`/`C-p` line, `C-v`/`M-v` page, `M-<`/`M->` top/bottom.
   - **nano**: arrows, `C-y`/`C-v` prev/next page, `M-\`/`M-/` first/last line.
   The base `↑`/`↓` and `Space`/`Enter` always work in every mode (the unchanged live-spill contract); mode bindings and mouse (clause 3) are additive. (Note: the preamble prints "vi (default)" while `config_cmd.rs:27` prints "shipped default: emacs" — a pre-existing repo inconsistency; this decision follows whatever mode is *active*, not a hardcoded default.)

5. **Erase-geometry preserved; mouse code compile-time-gated.** All repaint (mouse scroll, expand, mode-key nav, the post-completion overlay) goes through live-spill's exact-row erase model (`live_spill_viewport.md` rule 3/7) — never terminal autowrap inside the frame. Mouse support is a **compile-time feature gate** (folded under the existing `live-spill` / `rich-tui` features, `newt-tui/Cargo.toml:51,57`), so a wyvern `--no-default-features` build **never links** the mouse code — "runtime-gate" is not sufficient for "never link" and is not offered. (There is zero mouse code in the workspace today, so this is a clean-slate addition.)

## Non-goals / boundaries

- **No change to the non-interactive path** (piped / `newt worker` / `TERM=dumb` / wyvern) — byte-for-byte the 0.7.3 output. This is the load-bearing constraint.
- **No change to the keyboard-tier viewport for operators who don't opt in** — the existing `↑`/`↓`/`Space` behavior on any capable TTY is unchanged.
- No full alternate-screen for the chat; no panes/splits; no persistent status dashboard. The mouse tier is a bounded, dismissible overlay on one tool's output, not a chat framework. If it grows to persist across the whole session or coordinate chat layout, it crosses this decision's boundary (the Revisit Trigger `live_spill_viewport.md` already sets).

## Acceptance

- **Non-interactive golden test** (piped / `newt worker` / `TERM=dumb`): byte-for-byte identical to the pre-change baseline. (The interactive live-spill TTY output is escape-sequence/timing dependent and is tested separately by its existing harness — it is NOT part of the static golden capture.)
- **Mouse-capture scoping proof**: capture is enabled only with a live mouse-tier viewport on a stdin+stdout TTY with opt-in; **released even on the rule-7 abandon/panic path** (a test that forces the teardown-miss and asserts the terminal is restored); never enabled on any non-interactive or non-opt-in path.
- The keyboard contract (`↑`/`↓`, `Space`/`Enter`) is unchanged; mode-aware keys and mouse are purely additive.

## Consequences

- Newt gains genuine terminal interactivity (click-scroll, click-expand, post-completion review, mode-native keys) as an **opt-in capability tier**, while the amphibious lean/headless identity and the graceful non-interactive degradation are untouched.
- Implementation is a follow-on (the mouse-capture RAII guard + panic-path restore, the post-completion overlay ownership hand-off + anchoring, the mode-key routing, the feature gating). This doc fixes the contract and the invariants; it changes no code.

---
Authored by Beaver (MacBook, Claude Fable 5) at Shawn's direction, 2026-07-19; corrected against an adversarial source review (the viewport is TTY-gated, not morphology-gated).

# Decision: two presentation morphologies of the plain scroller — LeanTUI and RichTUI

**Status:** Accepted (decided by Shawn Hartsock, 2026-06-20). Refines — does not
revoke — `docs/decisions/plain_scroller_tui.md` (itself already amended 2026-06-17
by #416's inline rich input). The plain-scroller invariants still hold; this doc
records how the chat surface now wears two faces.
**Date:** 2026-06-20
**Related:** newt-agent#527 ("Prompt as service log: splitting RichTUI and
LeanTUI"), #416 (the ratatui inline rich surface this builds on), PR #419 (the
`InputSurface` seam that makes surfaces severable),
`docs/decisions/plain_scroller_tui.md` (the load-bearing rule),
`docs/decisions/plan_editor_ephemeral_tui.md` (the other carve-out).

---

## TL;DR

newt's chat surface is **one plain-scroller code path** behind the
[`InputSurface`] seam, presented in **two morphologies**:

- **LeanTUI** (flight / wyvern / cloud) — a dead-simple, hand-rolled crossterm
  text box. Each prompt renders as a timestamped **server-log line**
  `[2026-06-20 14:32:01] ❯ <prompt>`, so the conversational stream doubles as a
  greppable log when captured (`script`, tmux, a pipe). The default off a TTY and
  whenever the footer is off (`-n` / `--neat` / `--lite` / `--plain`).
- **RichTUI** (human) — the #416 ratatui *inline* surface, now a **two-line**
  view: a status header `[2026-06-20 14:32:01] vi --INSERT-- <model> @ <endpoint>`
  over the `❯` input row. The default on a TTY.

Both are expressible as scrolled lines: no alternate screen in the chat path, no
panes, no widgets in the output; scrollback is still the log; behavior over SSH /
tmux / pipe is unchanged. #527 **refines** the plain-scroller rule rather than
breaking it.

## Why

newt is amphibious — it serves a human *and* runs headless where wyvern flies.
In a cloud/headless context the agent's conversational stream **is** a server
log. #527 makes that explicit (the lean morphology), and — having split the two
audiences — lets the human-facing rich surface embrace being human-facing (the
two-line header) instead of being a compromise that tries to serve both at once.

## Decisions

1. **Lean is a runtime choice, not just a compile-time one.** Previously "lean"
   meant the `--no-default-features` build (rich-tui feature off). Now a
   rich-capable binary drops to the lean morphology at runtime via
   `-n` / `--neat` / `--lite` / `--lean` / `--flight` (aliases of `--plain`, i.e.
   `NEWT_FOOTER=off`), and **non-TTY / piped input is always lean** — which is
   exactly when the "stream is a server log" framing matters. Surface selection
   (`run_chat`): footer-on + TTY → RichTUI; footer-off → LeanTUI.

2. **The lean surface is hand-rolled on crossterm, not rustyline.** `lean_input.rs`
   is a minimal text box: typed text, basic cursor editing (←/→, Home/End,
   Backspace/Delete), copy-and-paste, and ↑/↓ file-backed history. **No
   vi / emacs / nano editing modes** — that richness lives in the rich surface.
   On a non-TTY it bypasses raw mode and reads plain stdin lines, echoing
   `[ts] ❯ <input>` into the log. (rustyline is retained transiently only for the
   rare footer-on-non-TTY rich-single-line case; its removal is tracked.)

3. **The rich surface is two lines.** `header_line` carries the full-datetime
   clock, the vi `--INSERT--` / `--NORMAL--` word (or `emacs` / `nano`), and
   `<model> @ <endpoint>` (refreshed each turn via the default-noop
   `InputSurface::set_runtime_context`, so a `/model` switch shows up next turn).
   The clock and mode update live because the inline loop already redraws every
   frame — the live mode indicator is free.

4. **The chevron dims on submit via the committed form, not an in-place rewrite.**
   On Enter the inline widget is cleared and `echo_submitted` writes the committed
   line to scrollback as `[ts]` then a **dimmed `›  <body>`** — the bright live
   `❯` frozen into an at-rest log marker. This honors the plain-scroller rule (no
   blind rewrite of committed scrollback) and needs no cursor gymnastics.

5. **The "thinking" spinner is the existing braille animation, reused.** The
   `⠋ thinking…` line (`SPINNER_FRAMES` + `with_thinking_spinner`, gated on
   `color && thinking_stream_enabled()`) already fills the wait for the first
   token and erases itself with `\r\x1b[K`. With (3)+(4) the rich after-Enter
   sequence is `[ts]` / `›  <body>` / `⠋ thinking…`, matching #527. Piped output
   (color off) shows no spinner — no `\r` spam in the log.

6. **Color / theme is a first-class axis.** `ColorMode`
   (`auto|always|never|minimal|inverted|dark|light|mono`) via `--color`, the
   `--mono` shorthand, and `[tui] color`, layered over the existing `[tui.colors]`
   palette. Precedence: `--color` / `NEWT_COLOR` > `NO_COLOR` / `TERM=dumb` >
   `[tui] color` > `auto`. **Documented deviation:** an *explicit* `--color`
   overrides `NO_COLOR` — if you ask for color on the command line, you get it;
   `NO_COLOR` still wins over a *persisted* config choice.

## Plain-scroller invariants upheld

- No alternate screen in the chat path. The rich surface uses the ratatui
  **inline** viewport (the #416 carve-out), not full-screen; the lean surface is
  pure line editing.
- Scrollback is the log: every committed prompt + the assistant output remain in
  terminal history. The lean morphology makes those prompts self-timestamping.
- SSH / tmux / pipe parity: piped/non-TTY degrades to the lean server-log lines;
  `NO_COLOR` / `TERM=dumb` drop color.

## Strippability (the wyvern tier)

The **lean** morphology (hand-rolled crossterm, no ratatui/tui-textarea) is the
wyvern-faithful surface. The ratatui inline rich surface stays behind the
`rich-tui` cargo feature; `cargo build --no-default-features` yields a lean-only
build, and `just check` guards that it compiles + lints clean.

## Terminal-capability floor for the rich tier (#1426, decided 2026-07-27)

**The rich surface requires an emulator that reflows on width change. One that
does not is a lean-tier terminal — run `--lean`.**

Surfaced by #1426: `live_spill.rs`'s `erase_output` computes its rewind by
dividing the painted line widths by the CURRENT column count, so a width shrink
assumes the emulator rewrapped the already-painted rows. That is what mainstream
emulators do, and ANSI exposes **no portable reflow capability query** — there is
nothing to probe, so the code must simply assume one behavior.

Assuming reflow is the right assumption because the alternative is worse in the
case that matters. The two available assumptions fail differently:

| Assumption | Reflowing emulator | Non-reflowing emulator |
|---|---|---|
| **reflow** (chosen) | correct | over-rewinds |
| no-reflow | leaves stale rows on every shrink | correct |

Choosing no-reflow would degrade the *common* case to be safe in the rare one.
Choosing reflow keeps the common case exact and pushes the rare one to the tier
built for it: `--lean` is a plain scroller with minimal formatting and no redraw
region, so it has no rewind to get wrong. **If the emulator has word wrap we use
it; if it does not, we do not** — the same either/or that already governs color
(`NO_COLOR` / `TERM=dumb`) and the mouse tier (#1303).

This is therefore a stated **requirement of the rich tier**, not a latent bug.
`erase_output` needs no height clamp: `MoveUp` already saturates at row 0, so a
clamp changes the emitted bytes without changing where the cursor lands.

## Consequences

- The default footer-off prompt changed from the legacy `\w $` to `[ts] ❯`. A
  custom `[tui] prompt` / `NEWT_PROMPT` template still overrides it.
- Two input surfaces now satisfy `InputSurface` on a TTY (rich) plus the lean box;
  both share the history-file format and the EMFILE/raw-mode safety story.
- **Revisit trigger (inherited):** if a needed interaction cannot be expressed as
  scrolled lines, write a new decision doc that supersedes the plain-scroller rule
  — do not land the surface change first.

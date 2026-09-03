# Decision: the persistent prompt (type-ahead → echo row → always-mounted editor)

Date: 2026-08-12
Status: accepted (phase 1 implemented; phases 2–3 planned)

## Problem

While a turn runs, the keyboard watcher owns stdin in cbreak with echo off.
Everything that is not an interrupt (lone Esc, Ctrl-C) or a viewport-nav key
was read and **dropped**. To the operator this presents as three UX failures
observed together (2026-08-12 session report):

1. Typing during "thinking…" vanishes silently — no echo, no buffer.
2. An Esc/Ctrl-C press produced no on-screen acknowledgment until the turn's
   next checkpoint, so a *working* graceful cancel read as a hang.
   (Fixed separately: the spinner now swaps its label to "interrupting…"
   within one tick — see `newt_core::tty::set_interrupt_pending`.)
3. There is no input surface at all until the turn ends.

The contrast target is the Claude Code TUI: a spinner row above an
always-mounted input box that accepts typing at any time.

## Constraints

- **Plain-scroller rule** (`plain_scroller_tui.md`, 2026-08-11 amendment):
  the LEAN surface and the piped/headless path stay a plain scroller — no
  alternate screen, no panes. The feature-gated RichTUI surface MAY host
  richer widgets.
- **One line, one writer** (`newt_core::tty` line arbiter, #1312): exactly one
  ephemeral writer owns the terminal's bottom line. During a turn that writer
  is the spinner (or the live spill viewport). A second uncoordinated writer
  is how the pre-arbiter hang happened; any echo of typed text must go
  through the arbiter, never beside it.
- Interrupt keys (Esc, Ctrl-C tiers) keep absolute priority; type-ahead must
  never delay or absorb them.

## Decision — three phases

### Phase 1 (this PR): type-ahead capture, both surfaces

The turn key decoder classifies every ground byte: interrupts (unchanged),
viewport-nav keys (unchanged), and now **text** — everything else, instead of
dropping it. The watcher drains decoder text into a process-wide bounded
buffer (`newt_tui::type_ahead`, 4 KiB cap) **once, at watcher exit** — a
per-read drain would reset the decoder's buffer between keystrokes (typing
arrives one byte per read), silently breaking the Space/Enter latch and
backspace editing. The next prompt pre-fills with it: the lean surface via
its paste-sanitized insert, the rich surface via its textarea constructor.

Key-priority details:

- `Space`/`Enter` keep their expand/collapse contract **while the type-ahead
  buffer is empty**; once typing has begun they are text. A user steering the
  spill viewport loses nothing; a user mid-sentence gets their spaces.
- Editor-mode nav keys (vi `j`/`k`/`gg`…, mouse-tier opt-in only) win over
  text when the mode consumes the byte; unconsumed bytes are text.
- No echo yet: phase 1 is deliberately invisible until the prompt returns.
  Zero new writers, zero arbiter changes — the plain-scroller rule holds for
  the LEAN surface with no caveats.

### Phase 2: live echo row (RichTUI only)

Grow the line arbiter to lease a **two-row ephemeral block**: status/spinner
on top, a `› typed-ahead…` echo row beneath. One lease, one owner, one erase —
the arbiter's invariants extend from "the line" to "the block"; a second
independent writer is still unrepresentable. The echo row renders the
type-ahead buffer read-only (cursor stays hidden); editing beyond backspace
waits for phase 3. Gated on the RichTUI feature + TTY tier; LEAN keeps
phase-1 behavior.

### Phase 3: always-mounted editor (RichTUI only)

The rich surface's existing textarea/editor stays mounted during the turn and
becomes the bottom block's second row(s); submitting mid-turn queues the
message for the next turn (visible as a "queued" chip). The turn watcher
stops decoding text itself and instead forwards ground bytes to the mounted
editor. This is the Claude Code interaction model, scoped to RichTUI per the
plain-scroller rule.

## Amendment 2026-08-16 — the silent-tool gap is NOT a phase-2 problem (#1727)

Problem item 3 ("no input surface at all until the turn ends") conflated two
failures with different fixes. During a **long, silent tool call** — a
`run_command` waiting on the network, an MCP call, `experience_recall` — the
row under the `⚙` header showed *nothing*: not "no editor", but **no liveness
cue at all**, so waiting was indistinguishable from hung and operators killed
healthy processes (#1727).

That gap was never a missing writer that phase 2's block lease had to create.
The ONE spinner already existed; it was simply scoped to the *inference* call
and gone before the tool ran, and `LiveToolOutput::start` is bookkeeping whose
first paint is the child's **first byte**. Closed in the tool funnel
(`execute_tool_with_display_cancellable`) by holding a per-call spinner that
yields the row to the live-output viewport on that first byte
(`agentic::tools::live_output::ToolSpinner`). Zero arbiter changes; `LineCaps`
gates it exactly as it gates the thinking spinner, so pipes stay byte-identical
and the plain-scroller rule holds with no caveats.

Phases 2–3 stand for what they actually address — the *editor* being absent
during a turn — and are further amended by #1718: the terminal now belongs to
a UI thread that is merely idle between surface requests, which changes what
"always-mounted" costs. That correction is owned by the phase-3 PR, not here.

## Amendment 2026-08-16 — phase 3 lands as the cockpit; phase 2 is skipped

Phases 2–3 above were written before #1718 moved the session onto its own
thread. That changed the terrain, and the shipped design (#1669 cockpit,
`newt-tui/src/cockpit/`) is different from what they describe in three ways
worth stating so nobody builds the old plan later:

- **The block lease is not needed.** Phase 2's "grow the line arbiter to a
  two-row ephemeral block" solved a problem the arbiter no longer has to
  solve: on the rich surface the terminal thread is now the ONLY writer to
  the real terminal. It takes fd 1/2 onto a pty for the session's lifetime,
  reads the master, and lays every finished line into scrollback above a
  bottom block it keeps mounted. The arbiter keeps its one-row lease and its
  writers unchanged — they simply write to the pty now — and the spinner's
  own frames become the cockpit's status row without a second writer being
  created. Zero arbiter changes.
- **A pty, not a pipe.** Three checks decide behaviour, not styling, on
  `stdout().is_terminal()`: `LineCaps` (may a spinner paint), the permission
  gate's `interactive` (default-DENY without asking when false), and the
  modal prompt's raw-mode path. A pipe would flip all three. A pty slave
  keeps every answer.
- **The turn watcher is not spawned under the cockpit.** Phase 3's "the
  watcher forwards ground bytes to the mounted editor" is replaced by the
  terminal thread reading the keyboard itself, under the same arbiter stdin
  token the watcher used — which is what keeps a mid-turn `PromptWindow`
  working unchanged. Ctrl-C interrupts (every press is counted and
  acknowledged on the spinner label, #2010);
  **Esc belongs to vi**, a deliberate change from the watcher's lone-Esc
  cancel.

Found on the way, and **fixed separately in #1770** rather than here: the
modal's raw-mode guard relied on crossterm's process-global "prior mode"
static, which makes a second `enable_raw_mode` a no-op — so under any other
raw-mode owner the modal ran canonical+echo (keys buffered until Enter, kernel
echo over the editor). It now saves and restores the exact termios itself
(`tty/modal.rs`). The cockpit is simply the first component in the tree that
owns raw mode while a modal opens; the repair is `newt-core`-only and stands
on its own, so it landed ahead of this change instead of inside it.

Not in this slice: mid-turn *submission into the running turn* — a submit
during a turn is queued for the next `ReadLine` and shown as a `queued` chip;
steering the live turn through `SessionSteeringInbox` is the follow-up. The
live/completed spill viewports are not constructed under the cockpit in v1
(they paint with cursor motion the presenter drops); the tool spinner (#1727)
covers liveness meanwhile.

## Known seams (phase 1, documented not fixed)

These are pre-existing races/ambiguities that type-ahead capture makes newly
*observable*; fixing them means arbiter or turn-lifecycle surgery that phase 2
owns:

- **Esc then fast typing.** A printable byte arriving within the 200 ms
  escape-grace window reads as an Alt-chord and is dropped (neither interrupt
  nor text). Genuinely ambiguous input; unchanged from the pre-capture
  behavior except that the loss is now visible by contrast.
- **Modal-prompt boundary.** A mid-turn permission prompt waits for the
  watcher's in-flight iteration (≤ ~300 ms); keys pressed exactly there are
  captured as type-ahead rather than answered to the modal, and resurface as
  prefill. The window predates this work (the keys were previously eaten
  silently). Phase 2's block lease is the structural fix.
- **Turns that never reach a prompt.** A watcher that isn't followed by
  `read_line` (e.g. an injected turn) leaves the buffer to concatenate with
  the next turn's capture, bounded by the 4 KiB cap. The prefill is visible
  and editable, so staleness is recoverable by the user.

## Out of scope

- Mid-turn *submission* into the running turn (phases 1–2 queue for the next
  prompt; phase 3 adds queued-message UI, still not mid-turn injection).
- Any LEAN-surface rendering change.
- Windows: the watcher is unix-only today; type-ahead rides the same cfg.

# Decision: newt's chat surface is a plain scroller (amphibious by design)

**Status:** Accepted (decided by Shawn Hartsock, 2026-06-12)
**Date:** 2026-06-12
**Related:** newt-agent#89 ("is a rich interactive TUI in-scope for newt's
'sharp minimal binary' identity?" — closed by the newt / gilamonster-agent
split; this doc records the standing answer), PR #301 (the preamble always
shows), `docs/decisions/mesh_integration.md` (newt as a mesh worker).

---

## TL;DR

newt is deliberately a **simple line-scrolling terminal application** — like
bash. The chat surface is rustyline input plus `println!` output; the
terminal's own scrollback buffer is the history. There is no alternate
screen in the chat path, no custom scroll region, no panes, no status bars,
no widgets, no mouse handling.

This is not a temporary limitation. It is a load-bearing design choice:
**newt is amphibious** — the same agent serves a human at a terminal *and*
runs headless where wyvern-agent flies. Advanced TUI bells and whistles
belong in the **gilamonster-agent** and **monitor-agent** repos, not here.

## Context — the three tiers

The Foundation agent designs form a spectrum, and newt sits deliberately in
the middle of it:

| Tier | Agent | Surface | Authority (ocap) |
|------|-------|---------|------------------|
| Flight (headless swarm) | **wyvern-agent** | None. A very stripped-down version of this same agent design, built for "flight": no TUI creature comforts at all, very light, fully headless. Communicates **only via agent-mesh**. | **Locked into the embedded brush.** No escape hatch, no mechanism to ask for privileges — there is no human on the other end to ask. |
| Amphibious (human + headless) | **newt-agent** | Plain scroller. One code path that works identically for a human over SSH/tmux *and* piped/headless in swarm contexts. | ocap by default, with a deliberate escape hatch (`--disable-ocap` / `--yolo`, #297) **for usability testing** — a human-in-the-loop affordance only. |
| Swarm control | **drake-agent** | The interface point for wyvern-agent meshes: like newt-agent, but built specifically for controlling agentic swarms of wyvern-agents organized into flights. | Human-side control plane (its own decision doc when built). |
| Human-facing rich UI | **gilamonster-agent**, monitor agents | Advanced TUI: panes, live status, dashboards, the feature matrix. | n/a here. |

newt is built amphibious **on purpose**: it is the hands-on testing ground
for the same agent design that flies in wyvern. A human can sit in the loop,
drive the agent interactively, and observe exactly what a headless flight
would do — because the interactive surface adds almost nothing on top of the
headless behavior. Every TUI creature comfort newt grows is something wyvern
has to strip back out, and something that makes "what the human tested" and
"what the swarm runs" diverge. Keeping the scroller plain keeps the
amphibian honest.

The authority column follows the same amphibious logic as the surface
column. newt may escape ocap *because* it is the usability-testing ground —
the hatch exists for a human at the keyboard, never for unattended runs.
wyvern flies headless in a swarm, speaks only agent-mesh, and therefore has
no hatch and no privilege-request path at all: the embedded brush is the
whole world. The pairing is the point — the agent design gets its hands-on
testing in newt with the hatch available, and flies in wyvern with the
hatch welded shut.

The practical wins compound with the architectural one:

- **SSH/tmux/pipe parity.** No alt screen and no custom scroll means newt
  behaves identically over SSH, inside tmux, and when stdin/stdout are
  pipes. (See the chat-section comment in `newt-tui/src/lib.rs`.)
- **Scrollback is the log.** Everything the agent ever printed is in the
  terminal's own history — searchable, copy-pasteable, capturable with
  `script`/asciinema, never destroyed by a redraw.
- **Degrades to dumb terminals.** `NO_COLOR`, `TERM=dumb`, and non-TTY
  stdout all drop to plain text (`color_supported`).
- **Small surface, small deps.** A scroller has no layout engine to
  maintain and no resize/redraw bug class at all.

## Decision

1. **The chat path stays a plain scroller.** Line-oriented input
   (rustyline), line-oriented output, terminal-owned scrollback. Like bash.
2. **No advanced TUI in newt.** The following do not get added to the chat
   path: alternate screen, raw-mode UI loops, scroll regions, panes/splits,
   persistent status bars, full-screen widget frameworks (ratatui, cursive,
   …), mouse handling, live-updating dashboards, multi-line redraws.
3. **Feature pressure gets redirected, not absorbed.** Wants for richer UI
   belong in **gilamonster-agent** (the feature-rich agent matrix that
   inherits newt's published crates) or the **monitor-agent** repos. If a
   newt change seems to need a richer surface, that is the signal the
   feature belongs in those repos — not that newt should grow the surface.
4. **Strippability is a requirement.** wyvern-agent is a stripped-down
   build of this same agent design. Anything added to newt's interactive
   surface must be cleanly severable so the headless core stays light.

### Standing carve-outs (the full list — additions need a new decision)

- **The startup splash** — the first of two ephemeral alternate-screen
  surfaces (see also the `/plan` editor below). It lives in the alt screen,
  vanishes before chat starts, and the inline preamble is printed into real
  scrollback in both `--splash` and `--no-splash` modes (PR #301 — the
  preamble always shows). ratatui is permitted as a dependency *only* for the
  alt-screen surfaces; if both ever drop it, drop the dependency.
- **The `/plan` editor** — the second ephemeral alternate-screen surface,
  opened **only on explicit human request** (`/plan edit`) to edit a
  structured plan document as a form instead of hand-writing TOML. Like the
  splash: feature-gated out of the headless/wyvern build, RAII-torn-down so it
  cannot leak (see #302), and on close it prints the canonical plan into real
  scrollback exactly as a headless run would. The plain-text plan document
  stays the source of truth; the chat path stays a plain scroller. Full
  rationale and the cross-tier (wyvern ingestion) boundary:
  `docs/decisions/plan_editor_ephemeral_tui.md`.
- **rustyline's internal raw mode** during a `readline` call (line editing,
  history navigation). Output remains scrolled text.
- **The status in the prompt line** — the status (`<ts> · <model> · <ws> ·
  <mode>`) is folded into the rustyline **prompt** itself, not a separate
  surface:

  ```text
  [2026-06-16 11:59:02] gpt-4.1 | emacs | newt-agent ❯ <input>
  ▸ <output>…
  ```

  This is the cargo-progress-bar trick done for free: **rustyline already
  floats and redraws its prompt line at the bottom** (cursor, resize, redraw),
  so the status sits at the at-rest tail while idle, scrolls away naturally as
  output comes, and stays in scrollback as a greppable per-turn **log marker** —
  **no region, no pinning, no cursor games, no width dependency**. Multi-line
  entry (newt conventions, not canonical editor bindings): **Ctrl-O** inserts a
  newline (reliable, both modes); **Shift-Enter** does too where the terminal
  emits a distinct sequence (many send a bare CR, so it often no-ops); a
  **`"""`/`'''` fence** alone on the first line opens a markdown-style block
  (Enter adds lines, a matching closing fence submits — terminal-independent);
  and a trailing **`\`** continues **bang lines only** (multi-line `! …` shell
  commands — a chat line ending in `\` is literal and submits). Enter submits.
  rustyline's vi mode does **not** implement vi's canonical `o`/`O` open-line,
  and a `bind_sequence` cmd cannot enter vi insert mode — so Ctrl-O is a newt
  convention, not vi (tracked upstream: kkawakam/rustyline#946).

  **Fully customizable** — it is just the default `[tui] prompt` template.
  Tokens come in a readable `$NAME` form and a terse `\x` form: `$TIMESTAMP`/`\t`,
  `$DATE`, `$TIME`, `$MODEL`/`\m`, `$MODE`/`\M`, `$USER`/`\u`, `$HOST`/`\h`,
  `$WS`/`\w`, `$PATH`/`\W`, `$VERSION`/`\v`. An explicit `[tui] prompt` (or
  `NEWT_PROMPT`) wins; e.g. `$USER@$HOST:$PATH # ` (or `\u@\h:\W # `) gives a
  bash-like prompt. `/prompt` lists the tokens live; `newt config` emits a
  documented starter with the default prompt + token comments. `[tui] footer =
  auto` (default) uses the rich default on a TTY and a plain `\w $ ` off one;
  `on` forces rich; `off`/`--plain` forces plain. **Never** rich off a TTY
  (pipes, `newt worker`); `wyvern-agent` strips it.

  > **Rejected: a pinned idle status bar.** A version that pinned the status to
  > the bottom rows via a DECSTBM **scroll region** was prototyped and **backed
  > out** — it fought the terminal's natural scroll (content popped *up* into
  > the region) and the region/cursor teardown corrupted output across turns.
  > The "no scroll regions / no persistent status bars" rule above **stands**.
  > Anything fancier than rustyline belongs in `gilamonster-agent`.
- **ANSI color and column escapes in scrolled output** (header, prompts,
  diff coloring), always behind `color_supported` degradation.
- **Single-line, same-line indicators** — e.g. the `▸ thinking…` indicator
  erased with `\r` (`print_thinking` / `erase_line`). One line, erased in
  place, never a region.
- **The `!` bang-escape** — a prompt line starting with `!` runs the remainder
  as a host command (`bang_command` / `run_bang_escape`) with **inherited
  stdio**, *between* readlines, so an interactive child (e.g. `! pa login`'s
  browser SAML) owns the real TTY and its output scrolls live. No alternate
  screen, no region — it extends rustyline's standing readline carve-out
  (control is out of `readline` when the child runs; cooked mode already
  restored). It is a **human action only**: the model has no channel to type at
  the prompt, so it can never invoke `!`, and it deliberately runs with the
  user's own host authority (no OCAP/Caveats leash — that governs only
  *model*-initiated `run_command`). `wyvern-agent` (no interactive loop) has no
  bang-escape. A `!` line is **recolored live** via rustyline's `Highlighter`
  (bold `accent` `!` sigil + the whole command in the `shell_mode` tint) so it
  reads as obviously *not* a chat message — within rustyline's existing carve-out,
  no region. Two independent slots from a small `[tui.colors]` palette (`accent` /
  `shell_mode` / `dim`, each a named color or `#rrggbb`, or `"none"` to disable
  that slot), defaulting to built-ins and dropped entirely under `NO_COLOR` /
  non-TTY. Multi-line entry uses the trailing-`\` continuation, and the bang line
  is handed to `$SHELL -c` with the `\` intact so the shell does its own
  line-joining.

## Consequences

- PR review guidance: a diff that introduces `EnterAlternateScreen`,
  ratatui, or raw-mode event loops anywhere outside the splash is rejected
  or redirected to gilamonster-agent / monitor repos, citing this decision.
- No live progress panes in newt — long-running work reports by printing
  lines (which is also exactly what a headless flight wants in its logs).
- The `run_pilot` stub in `newt-tui` stays a stub here: a full-screen
  pilot/monitor surface is precisely the kind of thing this decision sends
  to the monitor-agent repos. Follow-up: the `newt-tui` crate description
  ("ratatui: code mode + pilot mode") overstates the role ratatui plays and
  should be reworded when next touched.
- Revisit trigger: if newt genuinely cannot express a needed interaction as
  scrolled lines, write a new decision doc that supersedes this one — do
  not land the surface change first.

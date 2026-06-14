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
- **ANSI color and column escapes in scrolled output** (header, prompts,
  diff coloring), always behind `color_supported` degradation.
- **Single-line, same-line indicators** — e.g. the `▸ thinking…` indicator
  erased with `\r` (`print_thinking` / `erase_line`). One line, erased in
  place, never a region.

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

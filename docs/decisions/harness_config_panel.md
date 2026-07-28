# Decision: a footer-launched harness config panel (severable, TTY-only)

**Status:** **Proposed** (awaiting decision by Shawn Hartsock) — an amendment to
`docs/decisions/plain_scroller_tui.md`.
**Date:** 2026-07-28
**Related:** `docs/decisions/plain_scroller_tui.md` (the plain-scroller doctrine
this amends), `#416` (the rich inline input carve-out — the precedent),
`docs/decisions/lean_rich_tui_morphologies.md`, `docs/decisions/live_spill_viewport.md`,
the tenacity work (#1469 / #1478 / #1479 / #1480 — the `[tenacity]` dial +
`/tenacity` command + footer indicator this panel would front).

---

## TL;DR

Permit **one** deliberately-opened, transient **harness config panel** —
launched from the footer (e.g. by navigating to the tenacity indicator and
pressing enter) — that lets a human at a TTY inspect and adjust **operator
configuration**: the active model profile and the tenacity knobs. It is **not**
the chat surface, **not** a persistent widget in the chat flow, and it compiles
out of the headless/wyvern path entirely. The plain-scroller rule continues to
govern the chat path and all committed output.

## Why this does not break the amphibian

The plain-scroller doctrine's load-bearing claim is that *the interactive surface
adds almost nothing on top of the headless behaviour*, so "what the human tested"
and "what the swarm runs" don't diverge. A config panel does not violate that,
because it touches **configuration inputs, not agent behaviour or output
rendering**:

- The panel reads/writes the SAME resolved config the headless path consumes —
  the model profile and the `[tenacity]` level. It is a viewer/editor for
  `effective_tenacity()` + the model card, nothing the agent loop doesn't already
  obey. Setting tenacity to `relentless` in the panel is identical to
  `/tenacity relentless` or `--tenacity relentless`; the swarm runs the same dial.
- It renders **no agent output**. The chat scroller, the committed line output,
  and the piped/headless path are untouched. There is no alternate-screen chat,
  no panes over the conversation, no status bar competing with the line arbiter.
- It is **severable and TTY-only**, exactly like the rich inline input (#416) and
  the live spill viewport: gated behind the same TTY + morphology checks, behind
  an `InputSurface`-style seam, so wyvern-agent and every piped run never compile
  or reach it. Every creature comfort wyvern must strip, it strips here too — but
  a config panel is not something wyvern *has*; wyvern is locked into its embedded
  brush and configured out-of-band, so there is nothing to diverge.

The panel is therefore the **operator's cockpit for the dials the agent already
obeys**, not a new agent surface. It keeps the amphibian honest because the agent
loop is byte-for-byte the same whether the dial was set by the panel, the slash
command, the flag, or the config file.

## Constraints (the carve-out, precisely)

1. **Transient overlay, deliberately opened and closed.** Opened by an explicit
   operator action (footer navigation / a key), it draws, takes input, applies
   the change, and returns to the plain scroller. It does not live in the chat
   flow between turns and never redraws over streaming agent output.
2. **Config only.** Scope is limited to operator configuration: model profile
   selection + the tenacity knobs (and future harness dials of the same kind).
   It MUST NOT render conversation, tool output, or any agent-produced content.
3. **Severable + TTY-only.** Behind the existing TTY/morphology gates and an
   input-surface seam, so it is absent from the headless/piped path and from
   wyvern-agent by construction (compiles out, mirroring #416 / #419).
4. **Writes go through the same setters.** The panel calls `set_cli_tenacity`
   (and the model-selection path) — the identical process-globals the flag and
   slash command use — so there is one resolution order and no panel-only state.
5. **No dependency creep.** No new heavyweight TUI framework (ratatui, etc.) on
   the chat path. If the panel needs richer drawing than the existing rich-input
   morphology provides, that is a separate, explicitly-authorized decision.

## What this does NOT authorize

- Panes, dashboards, or status bars over the **chat** conversation (still
  forbidden; that is gilamonster-agent / monitor-agent territory).
- Any surface on the **headless/piped** path or in **wyvern-agent**.
- Rendering agent output in anything but the plain line scroller (+ the existing
  narrow live-spill exception).

## Decision requested

Accept this carve-out so #14 (the harness panel itself) can be built against a
documented seam, or reject/reshape it. Until accepted, the panel is not built;
the tenacity dial remains fully usable via `--tenacity`, `[tenacity]` config, and
`/tenacity` (shipped), and the footer shows the static indicator (#1480).

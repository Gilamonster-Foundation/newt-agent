# Decision: live tool spills use a turn-scoped inline viewport

**Status:** Accepted (decided by Shawn Hartsock, 2026-07-16); **amended
2026-07-17** by
[issue comment 4998746992](https://github.com/Gilamonster-Foundation/newt-agent/issues/1235#issuecomment-4998746992)
to add bounded expand/collapse state.
**Date:** 2026-07-16 (amended 2026-07-17)
**Supersedes:** the blanket prohibition on multi-line redraws in
`docs/decisions/plain_scroller_tui.md`, but **only** for the TTY-only,
turn-scoped live tool spill viewport defined here. The plain-scroller decision
continues to govern committed output and every other chat-path surface.
**Related:** newt-agent#1235 (bounded tool-output spill and scroll interaction),
`docs/decisions/lean_rich_tui_morphologies.md` (the two input morphologies),
`docs/decisions/plain_scroller_tui.md` (the standing amphibious rule).

---

## TL;DR

While a tool is running on an interactive TTY, newt may redraw one bounded
block beneath that tool's committed `⚙` audit line. Each tool starts at the
configured collapsed height. The block tails the tool's output by default; Up
scrolls into its buffered history and Down returns toward the tail, reattaching
live follow at the bottom. When the tool finishes, newt erases the live block
and prints exactly one canonical completed spill block into real terminal
scrollback.

At a reachable output boundary, `⧉` marks the collapsed state and offers an
expand action; Space or Enter expands retained output up to the terminal's safe
row budget. The boundary then shows `▣`, which collapses back to the configured
height. When every retained line fits, no scroll thumb is shown.

This is a narrow output-surface exception, not a general TUI. It adds no
alternate screen, DECSTBM scroll region, panes, mouse handling, persistent
status, or full-screen event loop. Non-TTY and headless runs never emit cursor
movement for this feature and retain the ordinary completion-only output path.

## Context

The first round of #1235 made every completed tool result use the same bounded,
tail-biased spill rendering. That solves transcript consistency, including for
commands such as `find`, but it cannot show a long-running command as it grows
or let a human inspect lines just above the three-line default window.

The existing plain-scroller decision deliberately forbids live multi-line
redraws and requires a superseding decision before such a surface lands. The
need here is narrower than a pane or dashboard: one tool already owns a bounded
result block for part of one model turn, and the block can be removed before a
normal, immutable transcript entry is committed. Keeping that boundary
explicit preserves newt's headless behavior and prevents this primitive from
becoming an ambient terminal UI framework.

## Decision

1. **The live viewport exists only for an active tool on a TTY.** It starts
   after the tool's `⚙` audit line and ends before the tool's canonical result
   is committed. It is compiled and selected so a headless/flight build can
   sever it without changing tool execution or result semantics.

2. **One coordinator owns stdout for the lifetime of the viewport.** Tool
   runners publish renderer-neutral byte chunks; they do not write terminal
   control sequences or race the renderer. The coordinator serializes tool
   chunks, redraws, cancellation feedback, permission transitions, and final
   commit. A permission prompt or any other component that needs the terminal
   must first suspend or finish the live frame.

3. **Geometry is bounded for each active frame.** `[tui].spill_lines`
   determines both the collapsed live row count and the canonical completed
   spill row count (default `3`). Every tool starts collapsed. `⧉` expands
   retained output only up to the safe terminal row budget; `▣` restores the
   configured count. Requested rows are always capped to safe terminal
   capacity. The renderer clips content deliberately to the detected terminal
   width; accidental terminal autowrap must not change the rows subsequently
   erased. Resize rebuilds the frame from buffered lines and is polled during
   the active turn, but never turns the viewport into a full-screen surface.

4. **The default position follows the tail.** New chunks keep the newest lines
   visible. Up detaches tail-follow and moves toward older buffered lines. Down
   moves toward newer lines; reaching the bottom reattaches tail-follow so
   subsequent chunks are visible. The gutter communicates position using the
   established spill vocabulary: `▲` for hidden lines above, `▼` for hidden
   lines below, `▒` for the track, `▓` for the active thumb, `⧉` for the
   collapsed boundary's expand action, and `▣` for the expanded boundary's
   collapse action. `▲` or `▼` replaces the boundary action on a side where
   retained lines are hidden. When expansion makes scrolling unavailable, no
   thumb is rendered. The completed spill keeps `…` as truncation/completion
   vocabulary; it is not the live expand/collapse control. Live history retains
   at most 4096 logical lines.

5. **Input handling remains turn-scoped.** Up and Down are consumed for the
   active spill viewport only while a tool is running. Escape and Ctrl-C keep
   their existing cancellation behavior. Fragmented escape sequences are held
   for a bounded inter-byte grace period and parsed as sequences; this avoids
   mistaking normally delivered arrow sequences for a lone Escape while
   acknowledging the terminal protocol cannot distinguish an arbitrarily late
   continuation from a real Escape press. Space or Enter activates the visible
   `⧉`/`▣` boundary state. Outside an active tool, input history and editing
   behavior are unchanged.

6. **Display sanitization cannot change the result.** Live chunks are treated
   as untrusted terminal data. ANSI/OSC sequences and unsafe C0 controls are
   removed or rendered harmless for the viewport, with newline and tab handled
   by the viewport's own layout rules. Sanitization applies only to this
   operator-facing display projection: the authoritative tool result delivered
   to the model, spill store, audit logic, and completion renderer is unchanged.

7. **Completion makes the transcript canonical.** On ordinary completion, the
   coordinator erases every row of the active frame, restores the cursor/mode it
   changed, and invokes the existing completed-result renderer exactly once. If
   a stalled or panicking presentation worker misses the bounded teardown
   deadline, the coordinator atomically abandons that generation without more
   terminal I/O before invoking the canonical renderer. An already-painted frame
   can remain as a terminal artifact in that fallback, but the stale generation
   cannot repaint, rewind over, or overtake the committed block. The committed
   block is the same canonical block a completion-only run would print;
   viewport position and tail-follow state never alter it.

8. **Non-TTY output stays completion-only and byte-stable.** Pipes, captured
   logs, `TERM=dumb`, workers, and headless/wyvern paths receive no live-frame
   cursor escapes or intermediate repaint records. They keep the `⚙` audit line
   followed by one completed spill block, preserving scriptability.

9. **`/spill` is a session override, not persisted configuration.** `/spill`
   (and `/spill status`) reports the effective row count and whether live
   interaction is available. `/spill <N>` changes the collapsed live and
   completed height for subsequent tools in the current session; `0` retains
   the existing unbounded completed rendering and disables the live viewport.
   `/spill reset`
   returns to resolved `[tui].spill_lines`. The override is discarded at
   session end and never mutates the config file implicitly.

## Explicit Non-Goals

- No alternate screen or DECSTBM/custom terminal scroll region.
- No panes, splits, persistent status/footer, dashboard, or mouse input.
- No attempt to rewrite output that has already been committed to terminal
  scrollback.
- No change to object-capability checks, permission prompts, output caps,
  redaction, model-facing payloads, or spill-store recovery.
- No promise that an ended tool remains interactively scrollable. After final
  commit, terminal scrollback is again the history; a future explicit viewer
  would require its own decision and interaction contract.

## Consequences And Review Rules

- The streaming seam must sit below every shell execution engine that claims
  live support, while preserving the same authorization and result-envelope
  path used by completion-only dispatch. An engine that cannot stream honestly
  degrades to the canonical completion block instead of simulating progress.
- Tests must cover pure viewport geometry, tail detach/reattach, expand/collapse
  state, split escape sequences, display sanitization, width/resize behavior,
  teardown on every exit path, and byte-identical non-TTY completion output.
- PR review should reject reuse of the live-frame coordinator for ambient
  status, multi-tool panes, post-completion rewriting, or any surface outside
  the active-tool lifetime. Those remain gilamonster-agent/monitor-agent work
  under the plain-scroller decision.

## Revisit Trigger

If the viewport needs to persist across tools, exceed the terminal-bounded
active-tool budget, or coordinate general chat layout, it has crossed this
decision's boundary. Write a new decision before broadening it; do not stretch
this exception into a terminal framework.
